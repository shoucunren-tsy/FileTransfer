use std::fs::OpenOptions;
use std::io::{Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use base64::Engine;
use tokio::sync::RwLock;

use crate::state::{AppState, TransferState, MAX_FILE_SIZE};

pub const CHUNK_SIZE: u64 = 512 * 1024; // 与前端保持一致
/// 同时接收的上传数量上限。
const MAX_ACTIVE_TRANSFERS: usize = 64;
/// 同时接收的上传总大小上限（防止恶意客户端靠数量撑爆磁盘）。
const MAX_ACTIVE_TRANSFERS_TOTAL: u64 = 5 * 1024 * 1024 * 1024;
/// 最终文件名保留的最大字符数（Windows 单个文件名上限 255）。
const MAX_FILENAME_CHARS: usize = 180;
/// 临时文件名的最大字符数，给保存目录本身留出路径长度余量。
const MAX_TEMP_NAME_CHARS: usize = 80;

#[derive(serde::Serialize)]
pub struct FileChunk {
    pub data: String, // base64
    pub next_offset: u64,
    pub eof: bool,
}

#[derive(serde::Serialize)]
pub struct PickResult {
    pub paths: Vec<String>,
    pub error: Option<String>,
}

#[derive(serde::Serialize)]
pub struct ClipboardPayload {
    pub kind: String,
    pub text: Option<String>,
    pub files: Vec<String>,
    /// kind=image 时为 data URL，供界面预览；文本/文件为 None。
    pub image_preview: Option<String>,
}

#[tauri::command]
pub async fn get_clipboard_payload(
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<ClipboardPayload, String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| format!("读取剪贴板失败: {e}"))?;
    let file_paths = clipboard
        .get()
        .file_list()
        .unwrap_or_default()
        .into_iter()
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    if !file_paths.is_empty() {
        let mut s = state.write().await;
        let files = file_paths
            .into_iter()
            .map(|path| {
                s.allow_read(path.clone());
                path.display().to_string()
            })
            .collect();
        return Ok(ClipboardPayload {
            kind: "files".into(),
            text: None,
            files,
            image_preview: None,
        });
    }

    if let Ok(image) = clipboard.get().image() {
        let mut encoded = Cursor::new(Vec::new());
        let mut encoder = png::Encoder::new(&mut encoded, image.width as u32, image.height as u32);
        // arboard 返回的是紧凑排列的 RGBA8 像素缓冲；必须显式声明颜色类型与位深，
        // 否则 png 编码器会按默认的 Grayscale/8bit 计算期望长度（width*height），
        // 与实际的 width*height*4 不符，write_image_data 会返回 ImageBufferSize 错误，
        // 进而导致“发送剪贴板到手机”在遇到图片时直接失败。
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("编码剪贴板图片失败: {e}"))?;
        writer
            .write_image_data(&image.bytes)
            .map_err(|e| format!("编码剪贴板图片失败: {e}"))?;
        writer
            .finish()
            .map_err(|e| format!("编码剪贴板图片失败: {e}"))?;
        // 时间戳命名，手机端下载文件名可读；顺手清掉超过 10 分钟的旧临时图。
        let deleted = cleanup_stale_clipboard_images();
        if !deleted.is_empty() {
            let mut s = state.write().await;
            for path in deleted {
                s.allowed_read_paths.remove(&path);
            }
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "clipboard-image-{}-{:04x}.png",
            ts,
            rand::random::<u16>()
        ));
        let png_bytes = encoded.into_inner();
        let image_preview = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png_bytes)
        );
        std::fs::write(&path, &png_bytes).map_err(|e| format!("保存剪贴板图片失败: {e}"))?;
        state.write().await.allow_read(path.clone());
        return Ok(ClipboardPayload {
            kind: "image".into(),
            text: None,
            files: vec![path.display().to_string()],
            image_preview: Some(image_preview),
        });
    }

    let text = clipboard.get_text().unwrap_or_default();
    Ok(ClipboardPayload {
        kind: "text".into(),
        text: Some(text),
        files: vec![],
        image_preview: None,
    })
}

/// 读取用户通过原生对话框选择的文件的一个分块（base64）。
#[tauri::command]
pub async fn read_file_chunk(
    path: String,
    offset: u64,
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<FileChunk, String> {
    let p = PathBuf::from(&path);
    {
        let s = state.read().await;
        if !s.allowed_read_paths.contains_key(&p) {
            return Err("未授权的文件路径".into());
        }
    }
    tokio::task::spawn_blocking(move || read_chunk_blocking(&p, offset))
        .await
        .map_err(|e| format!("读取任务失败: {e}"))?
}

fn read_chunk_blocking(p: &Path, offset: u64) -> Result<FileChunk, String> {
    let meta = std::fs::metadata(p).map_err(|e| format!("读取文件信息失败: {e}"))?;
    if meta.len() > MAX_FILE_SIZE {
        return Err("文件超过 5 GiB 限制".into());
    }
    let mut f = OpenOptions::new()
        .read(true)
        .open(p)
        .map_err(|e| format!("打开文件失败: {e}"))?;
    f.seek(std::io::SeekFrom::Start(offset))
        .map_err(|e| format!("定位文件失败: {e}"))?;
    let mut buf = vec![0u8; CHUNK_SIZE as usize];
    let n = f.read(&mut buf).map_err(|e| format!("读取文件失败: {e}"))?;
    buf.truncate(n);
    let data = base64::engine::general_purpose::STANDARD.encode(&buf);
    let next = offset + n as u64;
    Ok(FileChunk {
        data,
        next_offset: next,
        eof: next >= meta.len(),
    })
}

#[tauri::command]
pub async fn get_file_size(
    path: String,
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<u64, String> {
    let p = PathBuf::from(&path);
    {
        let s = state.read().await;
        if !s.allowed_read_paths.contains_key(&p) {
            return Err("未授权的文件路径".into());
        }
    }
    let meta = std::fs::metadata(&p).map_err(|e| format!("读取文件信息失败: {e}"))?;
    if meta.len() > MAX_FILE_SIZE {
        return Err("文件超过 5 GiB 限制".into());
    }
    Ok(meta.len())
}

#[tauri::command]
pub async fn pick_files(
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<PickResult, String> {
    // 先弹出对话框（可能耗时），避免长时间持有状态锁。
    let files = rfd::FileDialog::new()
        .set_title("选择要发送的文件（可多选）")
        .pick_files();
    let Some(files) = files else {
        return Ok(PickResult {
            paths: vec![],
            error: None,
        });
    };
    let mut paths = Vec::new();
    {
        let mut s = state.write().await;
        for f in &files {
            if f.is_file() {
                s.allow_read(f.clone());
                paths.push(f.display().to_string());
            }
        }
    }
    Ok(PickResult { paths, error: None })
}

#[tauri::command]
pub async fn register_files(
    paths: Vec<String>,
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<PickResult, String> {
    let mut accepted = Vec::new();
    let mut s = state.write().await;
    for path in paths {
        let file = PathBuf::from(path);
        if file.is_file() {
            s.allow_read(file.clone());
            accepted.push(file.display().to_string());
        }
    }
    Ok(PickResult {
        paths: accepted,
        error: None,
    })
}

#[tauri::command]
pub async fn pick_download_dir(
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<String, String> {
    let Some(dir) = rfd::FileDialog::new()
        .set_title("选择手机文件的保存目录")
        .pick_folder()
    else {
        return Ok(state.read().await.download_dir.display().to_string());
    };
    if !dir.is_absolute() {
        return Err("保存目录必须是绝对路径".into());
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败: {e}"))?;
    {
        let mut s = state.write().await;
        s.download_dir = dir.clone();
        log::info!("保存目录已更改为 {}", dir.display());
    }
    crate::state::save_download_dir(&dir);
    Ok(dir.display().to_string())
}

/// 在系统文件管理器中打开保存目录，或定位到某个已接收的文件。
/// 前端传来的路径会被当作命令参数交给系统，必须先验证它确实落在保存目录内。
#[tauri::command]
pub async fn reveal_path(
    path: String,
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<(), String> {
    let raw = PathBuf::from(&path);
    if !raw.is_absolute() {
        return Err("必须是绝对路径".into());
    }
    let root = state.read().await.download_dir.clone();
    let canon_root =
        std::fs::canonicalize(&root).map_err(|_| "保存目录不存在，请重新选择".to_string())?;

    // 归属判定走规范化路径（canonicalize 会解掉 .. 与符号链接），避免绕出保存目录。
    match std::fs::canonicalize(&raw) {
        Ok(canon) => {
            if !canon.starts_with(&canon_root) {
                return Err("只能打开保存目录内的文件".into());
            }
            open_in_file_manager(&raw, canon.is_file())
        }
        Err(_) => {
            // 文件可能已被移走或改名：退回打开它所在的目录，前提是该目录仍在保存目录内。
            let parent = raw
                .parent()
                .filter(|p| p.is_dir())
                .ok_or_else(|| "路径不存在".to_string())?;
            let canon_parent =
                std::fs::canonicalize(parent).map_err(|_| "路径不存在".to_string())?;
            if !canon_parent.starts_with(&canon_root) {
                return Err("只能打开保存目录内的文件".into());
            }
            open_in_file_manager(parent, false)
        }
    }
}

/// 调起系统文件管理器。select=true 时定位并选中该文件，false 时打开目录本身。
#[allow(unreachable_code)]
fn open_in_file_manager(target: &Path, select: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let text = target.to_string_lossy().into_owned();
        // 路径里不可能有双引号（Windows 文件名禁用字符），出现即视为非法输入。
        if text.contains('"') {
            return Err("路径包含非法字符".into());
        }
        // 交给 std 去拼命令行：它按 Windows 规则加引号，含空格的路径能原样还原成一条参数。
        // 注意不能用 raw_arg——它只做字符串追加、不会补分隔空格，会把 exe 和参数粘成一坨。
        let mut cmd = std::process::Command::new("explorer.exe");
        if select {
            cmd.arg(format!("/select,{text}"));
        } else {
            cmd.arg(&text);
        }
        // explorer.exe 即使成功也返回退出码 1，因此只能看进程有没有被拉起。
        return cmd
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("打开文件管理器失败: {e}"));
    }

    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("open");
        if select {
            cmd.arg("-R");
        }
        return cmd
            .arg(target)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("打开 Finder 失败: {e}"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // Linux 没有“选中文件”的概念，统一下降到打开所在目录。
        let dir = if select {
            target.parent().unwrap_or(target)
        } else {
            target
        };
        return std::process::Command::new("xdg-open")
            .arg(dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("打开文件管理器失败: {e}"));
    }

    Err("当前平台暂不支持打开文件夹".into())
}

/// 手机上传：写入一个分块（base64）。首次调用时创建临时文件。
#[tauri::command]
pub async fn receive_file_chunk(
    id: String,
    name: String,
    size: u64,
    seq: u64,
    data: String,
    device_id: u64,
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<u64, String> {
    if size > MAX_FILE_SIZE {
        return Err("文件超过 5 GiB 限制".into());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&data)
        .map_err(|e| format!("分块解码失败: {e}"))?;

    enum Step {
        Duplicate(u64),
        Write(PathBuf, u64),
        OutOfOrder,
    }

    let step = {
        let mut s = state.write().await;
        if let Some(t) = s.active_transfers.get_mut(&id) {
            if seq < t.next_seq {
                Step::Duplicate(t.written)
            } else if seq > t.next_seq {
                Step::OutOfOrder
            } else {
                Step::Write(t.tmp.clone(), t.written)
            }
        } else {
            // 设备已断开/被替换后，迟到的分块不能再新建传输，否则会留下无人认领的临时文件。
            if !s.devices.contains_key(&device_id) {
                return Err("设备已断开，传输已中止".into());
            }
            if seq != 0 {
                Step::OutOfOrder
            } else {
                if s.active_transfers.len() >= MAX_ACTIVE_TRANSFERS {
                    return Err("并发接收的文件过多".into());
                }
                let active_total: u64 = s.active_transfers.values().map(|t| t.written).sum();
                if active_total + size > MAX_ACTIVE_TRANSFERS_TOTAL {
                    return Err("同时接收的文件总量过大".into());
                }
                let sanitized = truncate_filename(&sanitize_filename(&name), MAX_TEMP_NAME_CHARS);
                // 用散列后缀区分并发传输：截断 id 尾部在恶意构造下可能碰撞，
                // 两个传输共用临时文件会互相污染数据。
                let tmp =
                    s.download_dir
                        .join(format!(".{}.{}.part", sanitized, transfer_suffix(&id)));
                s.active_transfers.insert(
                    id.clone(),
                    TransferState {
                        device_id,
                        tmp: tmp.clone(),
                        written: 0,
                        next_seq: 1,
                        last_activity: Instant::now(),
                    },
                );
                Step::Write(tmp, 0)
            }
        }
    };

    match step {
        Step::Duplicate(written) => Ok(written),
        Step::OutOfOrder => {
            abort_transfer(state.inner(), &id).await;
            Err("分块顺序异常，传输已中止".into())
        }
        Step::Write(tmp, written) => {
            if written + bytes.len() as u64 > size {
                abort_transfer(state.inner(), &id).await;
                return Err("数据超过声明大小，传输已中止".into());
            }
            let len = bytes.len() as u64;
            let write_path = tmp.clone();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let mut f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&write_path)
                    .map_err(|e| format!("打开临时文件失败: {e}"))?;
                f.write_all(&bytes).map_err(|e| format!("写入失败: {e}"))
            })
            .await
            .map_err(|e| format!("写入任务失败: {e}"))??;

            let mut s = state.write().await;
            match s.active_transfers.get_mut(&id) {
                Some(t) => {
                    t.written += len;
                    t.next_seq = seq + 1;
                    t.last_activity = Instant::now();
                    Ok(t.written)
                }
                None => Err("传输已被取消".into()),
            }
        }
    }
}

async fn abort_transfer(state: &Arc<RwLock<AppState>>, id: &str) {
    let tmp = {
        let mut s = state.write().await;
        s.active_transfers.remove(id).map(|t| t.tmp)
    };
    if let Some(tmp) = tmp {
        let _ = tokio::fs::remove_file(tmp).await;
    }
}

/// 手机上传完成：把临时文件改名为最终文件名。
#[tauri::command]
pub async fn finalize_receive(
    id: String,
    name: String,
    size: u64,
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<String, String> {
    let entry = { state.write().await.active_transfers.remove(&id) };
    let final_name = sanitize_filename(&name);
    let dir = state.read().await.download_dir.clone();

    let Some(entry) = entry else {
        // 空文件不会产生任何分块，也就没有临时记录：原子占名后直接留下空文件。
        if size == 0 {
            let final_path = claim_in_dir(dir, final_name).await?;
            log::info!("已保存手机文件：{}", final_path.display());
            return Ok(final_path.display().to_string());
        }
        return Err("未找到对应传输".into());
    };

    if entry.written != size {
        let _ = tokio::fs::remove_file(&entry.tmp).await;
        return Err(format!(
            "文件不完整：已接收 {} 字节，应为 {} 字节",
            entry.written, size
        ));
    }
    // 先原子占住目标名（create_new），再把临时文件改名覆盖自己占的占位文件：
    // 直接 unique_path + rename 是「先查后用」，并发同名上传可能互相覆盖。
    let final_path = claim_in_dir(dir, final_name).await?;
    // 杀毒软件/索引服务可能短暂占用占位文件，重试几次再判定失败，避免整份上传白传。
    let mut last_error = None;
    for attempt in 0..3 {
        match tokio::fs::rename(&entry.tmp, &final_path).await {
            Ok(()) => {
                last_error = None;
                break;
            }
            Err(e) => {
                last_error = Some(e);
                if attempt < 2 {
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                }
            }
        }
    }
    if let Some(e) = last_error {
        let _ = tokio::fs::remove_file(&final_path).await;
        let _ = tokio::fs::remove_file(&entry.tmp).await;
        return Err(format!("保存文件失败: {e}"));
    }
    log::info!("已保存手机文件：{}", final_path.display());
    Ok(final_path.display().to_string())
}

/// 在保存目录内原子占住一个不重名的路径（同名就换下一个序号）。
async fn claim_in_dir(dir: PathBuf, name: String) -> Result<PathBuf, String> {
    tokio::task::spawn_blocking(move || claim_unique_path(&dir, &name))
        .await
        .map_err(|e| format!("创建文件失败: {e}"))?
}

/// 以 create_new 原子判重：文件系统保证同名只会成功一次。
fn claim_unique_path(dir: &Path, name: &str) -> Result<PathBuf, String> {
    for i in 0..10_000usize {
        let candidate = candidate_name(dir, name, i);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("保存文件失败: {e}")),
        }
    }
    Err("无法生成不重名的文件名".into())
}

fn candidate_name(dir: &Path, name: &str, index: usize) -> PathBuf {
    if index == 0 {
        return dir.join(name);
    }
    let stem = Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let ext = Path::new(name)
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    dir.join(format!("{stem} ({index}){ext}"))
}

/// FNV-1a 64 位散列：把任意传输 ID 映射成固定长度的十六进制后缀。
fn transfer_suffix(id: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn sanitize_filename(name: &str) -> String {
    // 先统一分隔符再取最后一段。不能直接用 Path::file_name：
    // Windows 会把 "a:b.txt" 里的 "a:" 当成盘符前缀，把文件名前半段吞掉。
    let normalized = name.replace('\\', "/");
    let base = normalized.rsplit('/').next().unwrap_or("");
    // 控制字符会破坏日志与终端输出；双向文本格式字符会让显示顺序与真实字节不一致。
    let base: String = base
        .replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\0'], "_")
        .chars()
        .filter(|c| !c.is_control() && !is_invisible_format(*c))
        .collect();
    // Windows 会静默去掉文件名末尾的空格和点，这里提前处理保持一致。
    let base = base.trim().trim_end_matches(['.', ' ']);
    if base.is_empty() || base == "." || base == ".." {
        return "received-file".into();
    }
    let base = truncate_filename(base, MAX_FILENAME_CHARS);
    let stem = Path::new(&base)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| base.clone());
    let upper = stem.trim_end_matches(['.', ' ']).to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && matches!(upper.as_bytes()[3], b'1'..=b'9'));
    if reserved {
        format!("{base}_")
    } else {
        base
    }
}

/// 不可见格式字符：双向控制符、零宽字符、BOM 等，可用来伪装扩展名或隐藏真实内容。
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{061C}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
    )
}

/// 超长文件名按字符截断，尽量保留扩展名，便于双击时系统正确识别类型。
fn truncate_filename(name: &str, max_chars: usize) -> String {
    if name.chars().count() <= max_chars {
        return name.to_string();
    }
    let path = Path::new(name);
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let ext_len = ext.chars().count();
    if ext_len == 0 || ext_len >= max_chars / 2 {
        return name.chars().take(max_chars).collect();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem: String = stem.chars().take(max_chars - ext_len).collect();
    format!("{stem}{ext}")
}

/// 清理本应用写到 %TEMP% 的旧剪贴板图片缓存（含历史 drop-app-clipboard-* 命名）。
/// 返回已删除的路径，供调用方同步清理读取白名单。
fn cleanup_stale_clipboard_images() -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let dir = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return removed;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ours = (name.starts_with("clipboard-image-")
            || name.starts_with("drop-app-clipboard-"))
            && name.ends_with(".png");
        if !ours {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|age| age.as_secs() > 600)
            .unwrap_or(false);
        if stale {
            let path = entry.path();
            if std::fs::remove_file(&path).is_ok() {
                removed.push(path);
            }
        }
    }
    removed
}

/// 读取最近的应用日志（尾部 N 行），供桌面端日志面板使用。
#[tauri::command]
pub fn get_recent_logs(app: tauri::AppHandle, limit: Option<usize>) -> Result<Vec<String>, String> {
    use tauri::Manager;

    let dir = app
        .path()
        .app_log_dir()
        .map_err(|e| format!("获取日志目录失败: {e}"))?;
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("读取日志目录失败: {e}"))?;

    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(best, _)| modified > *best) {
            newest = Some((modified, path));
        }
    }

    let Some((_, path)) = newest else {
        return Ok(Vec::new());
    };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("读取日志失败: {e}"))?;
    let limit = limit.unwrap_or(200).min(2000);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(limit);
    Ok(lines[start..].iter().map(|line| line.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_path_traversal() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(
            sanitize_filename("../../windows/system32/cmd.exe"),
            "cmd.exe"
        );
    }

    #[test]
    fn sanitize_replaces_illegal_chars() {
        assert_eq!(sanitize_filename("a:b*c?.txt"), "a_b_c_.txt");
    }

    #[test]
    fn sanitize_handles_windows_reserved_names() {
        assert_eq!(sanitize_filename("CON"), "CON_");
        assert_eq!(sanitize_filename("con.txt"), "con.txt_");
        assert_eq!(sanitize_filename("COM1"), "COM1_");
        assert_eq!(sanitize_filename("LPT9.log"), "LPT9.log_");
        // COM0 不是保留名
        assert_eq!(sanitize_filename("COM0"), "COM0");
    }

    #[test]
    fn sanitize_handles_empty_and_dots() {
        assert_eq!(sanitize_filename(""), "received-file");
        assert_eq!(sanitize_filename(".."), "received-file");
        assert_eq!(sanitize_filename("   "), "received-file");
    }

    #[test]
    fn sanitize_strips_control_and_invisible_chars() {
        assert_eq!(sanitize_filename("a\u{0007}b.txt"), "ab.txt");
        assert_eq!(sanitize_filename("a\u{202E}b.txt"), "ab.txt");
        assert_eq!(sanitize_filename("a\u{200B}b\u{FEFF}.txt"), "ab.txt");
        assert_eq!(
            sanitize_filename("\u{2066}invoice\u{2069}.pdf"),
            "invoice.pdf"
        );
    }

    #[test]
    fn sanitize_truncates_long_names_but_keeps_extension() {
        let long = format!("{}.txt", "很".repeat(300));
        let out = sanitize_filename(&long);
        assert_eq!(out.chars().count(), MAX_FILENAME_CHARS);
        assert!(out.ends_with(".txt"));
    }

    #[test]
    fn transfer_suffix_is_stable_and_distinct() {
        let a = transfer_suffix("d1-abc");
        assert_eq!(a, transfer_suffix("d1-abc"));
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, transfer_suffix("d1-abd"));
        assert_ne!(a, transfer_suffix("d2-abc"));
    }

    #[test]
    fn claim_unique_path_is_atomic_and_adds_suffix() {
        let dir = std::env::temp_dir().join(format!("ft-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = claim_unique_path(&dir, "a.txt").unwrap();
        assert_eq!(first.file_name().unwrap(), "a.txt");
        let second = claim_unique_path(&dir, "a.txt").unwrap();
        assert_eq!(second.file_name().unwrap(), "a (1).txt");
        let third = claim_unique_path(&dir, "a.txt").unwrap();
        assert_eq!(third.file_name().unwrap(), "a (2).txt");
        // create_new 抢占后文件应真实存在（空文件也走这条路径）。
        assert!(third.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_replaces_claimed_placeholder() {
        // finalize_receive 依赖「rename 覆盖已存在的占位文件」；
        // 各平台语义不同，必须实际跑一遍，否则非空文件保存会整体失败。
        let dir = std::env::temp_dir().join(format!("ft-test-rename-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join(".upload.part");
        std::fs::write(&tmp, b"hello").unwrap();
        let target = claim_unique_path(&dir, "photo.jpg").unwrap();
        assert!(target.exists());
        std::fs::rename(&tmp, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"hello");
        assert!(!tmp.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
