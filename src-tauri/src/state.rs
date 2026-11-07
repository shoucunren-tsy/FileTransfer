use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc::Sender;
use tokio_tungstenite::tungstenite::Message;

use crate::session;

pub const DEFAULT_PORT: u16 = 8080;
/// 同时允许的手机/平板数量上限，防止异常客户端耗尽内存与带宽。
pub const MAX_DEVICES: usize = 5;
/// 超过该时长没有任何入站消息，视为僵尸连接，可被清理或替换。
pub const DEVICE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
pub const MAX_FILE_SIZE: u64 = 5 * 1024 * 1024 * 1024; // 5 GiB
/// 剪贴板文本上限，防止一次性推入超大 payload 撑爆内存。
pub const MAX_CLIPBOARD_TEXT: usize = 1024 * 1024;
pub const MAX_ALLOWED_PATHS: usize = 500;
pub const ALLOWED_PATH_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// 传输超过该时长没有新分块即视为死亡，清理临时文件。
pub const TRANSFER_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// 发给手机的发送队列容量，手机读得慢时形成背压。
pub const SINK_BUFFER: usize = 64;

pub type MessageSender = Sender<Message>;

/// 一台已连接设备（手机 / 平板）。
pub struct Device {
    pub id: u64,
    pub name: String,
    pub ip: Option<String>,
    pub sink: MessageSender,
    pub last_activity: Instant,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub id: u64,
    pub name: String,
    pub ip: Option<String>,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceList {
    pub devices: Vec<DeviceInfo>,
    pub count: usize,
    pub max: usize,
}

/// 桌面端既有的读取键为 snake_case（session_id / download_dir），
/// 这里不要改 camelCase，否则会话号与保存目录会显示为空。
#[derive(serde::Serialize)]
pub struct ServerInfo {
    pub session_id: String,
    pub ip: String,
    pub port: u16,
    pub connected: bool,
    pub download_dir: String,
    pub devices: Vec<DeviceInfo>,
    pub max_devices: usize,
}

/// 正在接收的手机上传传输。
pub struct TransferState {
    /// 发起该传输的设备 ID；设备断开或换会话时据此回收临时文件。
    pub device_id: u64,
    pub tmp: PathBuf,
    pub written: u64,
    /// 下一个期望的分块序号，用于拒绝重复块与跳号。
    pub next_seq: u64,
    pub last_activity: Instant,
}

pub struct AppState {
    pub app: AppHandle,
    pub session_id: String,
    /// 实际监听端口（8080 被占用时自动顺延）。
    pub port: u16,
    /// 连接 ID 自增器；每个连接拿到的编号同时作为设备 ID。
    pub conn_id: u64,
    /// 在线设备：设备 ID -> 连接信息。
    pub devices: BTreeMap<u64, Device>,
    /// 用户通过原生对话框选择、允许读取的文件路径（值=最近一次授权时间）。
    pub allowed_read_paths: HashMap<PathBuf, Instant>,
    /// 手机上传文件的临时写入状态：transfer_id -> 传输状态。
    pub active_transfers: HashMap<String, TransferState>,
    pub download_dir: PathBuf,
}

impl AppState {
    pub fn new(app: AppHandle) -> Self {
        let download_dir = load_download_dir();
        cleanup_stale_parts(&download_dir);
        Self {
            app,
            session_id: session::new_session_id(),
            port: DEFAULT_PORT,
            conn_id: 0,
            devices: BTreeMap::new(),
            allowed_read_paths: HashMap::new(),
            active_transfers: HashMap::new(),
            download_dir,
        }
    }

    pub fn device_list(&self) -> DeviceList {
        DeviceList {
            devices: self
                .devices
                .values()
                .map(|d| DeviceInfo {
                    id: d.id,
                    name: d.name.clone(),
                    ip: d.ip.clone(),
                })
                .collect(),
            count: self.devices.len(),
            max: MAX_DEVICES,
        }
    }

    pub fn emit_devices(&self) {
        let _ = self.app.emit(crate::PHONE_STATUS_EVENT, self.device_list());
    }

    /// 移除一台设备并收回它未完成的传输；返回设备与待删除的临时文件。
    /// 调用方负责发送 Close 与删除文件，避免在持锁期间做磁盘 IO。
    pub fn take_device(&mut self, id: u64) -> Option<(Device, Vec<PathBuf>)> {
        let device = self.devices.remove(&id)?;
        let mut tmp = Vec::new();
        self.active_transfers.retain(|_, t| {
            if t.device_id == id {
                tmp.push(t.tmp.clone());
                false
            } else {
                true
            }
        });
        Some((device, tmp))
    }

    /// 清掉已关闭或长时间没有入站消息的连接；
    /// 返回是否有变化，以及需要删除的传输临时文件。
    pub fn prune_stale(&mut self) -> (bool, Vec<PathBuf>) {
        let now = Instant::now();
        let stale: Vec<u64> = self
            .devices
            .iter()
            .filter(|(_, d)| {
                d.sink.is_closed() || now.duration_since(d.last_activity) >= DEVICE_IDLE_TIMEOUT
            })
            .map(|(id, _)| *id)
            .collect();
        let mut files = Vec::new();
        for id in &stale {
            if let Some((d, tmp)) = self.take_device(*id) {
                let _ = d.sink.try_send(Message::Close(None));
                log::info!("清理失效设备连接：{} ({})", d.name, id);
                files.extend(tmp);
            }
        }
        (!stale.is_empty(), files)
    }

    /// 授权读取某个路径，并在条目过多时自动剪枝。
    pub fn allow_read(&mut self, path: PathBuf) {
        self.allowed_read_paths.insert(path, Instant::now());
        if self.allowed_read_paths.len() > MAX_ALLOWED_PATHS {
            self.prune_allowed_paths();
        }
    }

    /// 清理过期或已不存在的白名单项，保证长跑时内存不无限增长。
    pub fn prune_allowed_paths(&mut self) {
        let now = Instant::now();
        self.allowed_read_paths.retain(|path, granted| {
            now.duration_since(*granted) < ALLOWED_PATH_TTL && path.exists()
        });
        if self.allowed_read_paths.len() > MAX_ALLOWED_PATHS {
            let mut items: Vec<(PathBuf, Instant)> = self.allowed_read_paths.drain().collect();
            items.sort_by_key(|a| std::cmp::Reverse(a.1));
            items.truncate(MAX_ALLOWED_PATHS);
            self.allowed_read_paths = items.into_iter().collect();
        }
    }
}

fn config_dir() -> PathBuf {
    dirs::config_dir()
        .map(|p| p.join("drop-app"))
        .unwrap_or_else(|| PathBuf::from("drop-app"))
}

fn default_download_dir() -> PathBuf {
    // 默认保存到系统应用配置目录下的 received 子目录，避免任意写盘。
    let dir = config_dir().join("received");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 清理上次异常退出遗留的临时文件（只清本应用自己的命名，避免误删用户文件）。
fn cleanup_stale_parts(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_own_temp_file(&name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 临时文件名为 `.{原文件名}.{16 位十六进制散列}.part`。
/// 保存目录是用户自己的文件夹，不能见 `.part` 就删——那可能是别的程序的下载分片。
fn is_own_temp_file(name: &str) -> bool {
    let Some(inner) = name
        .strip_prefix('.')
        .and_then(|rest| rest.strip_suffix(".part"))
    else {
        return false;
    };
    let Some((_, suffix)) = inner.rsplit_once('.') else {
        return false;
    };
    suffix.len() == 16 && suffix.chars().all(|c| c.is_ascii_hexdigit())
}

/// 启动时恢复上次选择的保存目录；没有记录或路径非法则回退默认目录。
fn load_download_dir() -> PathBuf {
    let path = config_dir().join("settings.json");
    if let Ok(text) = std::fs::read_to_string(&path) {
        // PowerShell 等工具写 UTF-8 会带 BOM，serde_json 不认，要先剥掉
        let text = text.trim_start_matches('\u{feff}');
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
            if let Some(s) = v.get("download_dir").and_then(|x| x.as_str()) {
                let p = PathBuf::from(s);
                if p.is_absolute() {
                    let _ = std::fs::create_dir_all(&p);
                    return p;
                }
            }
        }
    }
    default_download_dir()
}

/// 用户改完保存目录立即落盘，避免下次启动被悄悄重置。
pub fn save_download_dir(dir: &Path) {
    let base = config_dir();
    let _ = std::fs::create_dir_all(&base);
    let json = serde_json::json!({ "download_dir": dir.display().to_string() });
    let _ = std::fs::write(base.join("settings.json"), json.to_string());
}

#[cfg(test)]
mod tests {
    use super::is_own_temp_file;

    #[test]
    fn only_cleans_own_temp_files() {
        assert!(is_own_temp_file(".photo.jpg.0123456789abcdef.part"));
        assert!(!is_own_temp_file("download.part"));
        assert!(!is_own_temp_file(".other-app.part"));
        assert!(!is_own_temp_file(".photo.jpg.backup.part"));
        assert!(!is_own_temp_file(".photo.jpg.0123456789abcde.part"));
        assert!(!is_own_temp_file(".photo.jpg.0123456789abcdef.part.bak"));
    }
}
