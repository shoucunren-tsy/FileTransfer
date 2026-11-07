use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tauri::AppHandle;
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::Message;

use crate::session;

pub const DEFAULT_PORT: u16 = 8080;
pub const MAX_FILE_SIZE: u64 = 5 * 1024 * 1024 * 1024; // 5 GiB

pub type Sender = UnboundedSender<Message>;

#[derive(serde::Serialize)]
pub struct ServerInfo {
    pub session_id: String,
    pub ip: String,
    pub port: u16,
    pub connected: bool,
    pub download_dir: String,
}

pub struct AppState {
    pub app: AppHandle,
    pub session_id: String,
    /// 实际监听端口（8080 被占用时自动顺延）。
    pub port: u16,
    /// 当前 WebSocket 客户端发送通道（同一时间只允许一台手机连接）。
    pub sink: Option<Sender>,
    pub connected: bool,
    /// 用户通过原生对话框选择、允许读取的文件路径。
    pub allowed_read_paths: HashMap<PathBuf, ()>,
    /// 手机上传文件的临时写入状态：transfer_id -> (临时文件路径, 已写入字节数)。
    pub active_transfers: HashMap<String, (PathBuf, u64)>,
    pub download_dir: PathBuf,
}

impl AppState {
    pub fn new(app: AppHandle) -> Self {
        let download_dir = load_download_dir();
        Self {
            app,
            session_id: session::new_session_id(),
            port: DEFAULT_PORT,
            sink: None,
            connected: false,
            allowed_read_paths: HashMap::new(),
            active_transfers: HashMap::new(),
            download_dir,
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
    // 清理上次异常退出残留的 .part 临时文件。
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".part") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    dir
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
