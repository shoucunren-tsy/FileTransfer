use std::fs::OpenOptions;
use std::io::{Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use tokio::sync::RwLock;

use crate::state::{AppState, MAX_FILE_SIZE};

pub const CHUNK_SIZE: u64 = 512 * 1024; // 与前端保持一致

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
                s.allowed_read_paths.insert(path.clone(), ());
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
        let mut encoder =
            png::Encoder::new(&mut encoded, image.width as u32, image.height as u32);
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
        cleanup_stale_clipboard_images();
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
        state.write().await.allowed_read_paths.insert(path.clone(), ());
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
    let meta = std::fs::metadata(&p).map_err(|e| format!("读取文件信息失败: {e}"))?;
    if meta.len() > MAX_FILE_SIZE {
        return Err("文件超过 5 GiB 限制".into());
    }
    let mut f = OpenOptions::new()
        .read(true)
        .open(&p)
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
                s.allowed_read_paths.insert(f.clone(), ());
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
            s.allowed_read_paths.insert(file.clone(), ());
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
        let dir = if select { target.parent().unwrap_or(target) } else { target };
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
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<u64, String> {
    if size > MAX_FILE_SIZE {
        return Err("文件超过 5 GiB 限制".into());
    }
    if seq > 10_000_000 {
        return Err("分块数量异常".into());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&data)
        .map_err(|e| format!("分块解码失败: {e}"))?;

    let mut s = state.write().await;
    let (tmp_path, written) = if let Some(entry) = s.active_transfers.get(&id) {
        entry.clone()
    } else {
        if s.active_transfers.len() >= 64 {
            return Err("并发接收的文件过多".into());
        }
        let sanitized = sanitize_filename(&name);
        // 使用传输 id 尾部做临时文件后缀，避免同名文件并发接收互相覆盖。
        let suffix: String = id.chars().rev().take(8).collect();
        let tmp = s.download_dir.join(format!(".{}.{}.part", sanitized, suffix));
        s.active_transfers.insert(id.clone(), (tmp.clone(), 0));
        (tmp, 0)
    };

    if written + bytes.len() as u64 > size {
        return Err("数据超过声明大小，传输已中止".into());
    }

    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&tmp_path)
        .map_err(|e| format!("打开临时文件失败: {e}"))?;
    f.write_all(&bytes).map_err(|e| format!("写入失败: {e}"))?;
    let new_written = written + bytes.len() as u64;
    s.active_transfers.insert(id.clone(), (tmp_path, new_written));
    Ok(new_written)
}

/// 手机上传完成：把临时文件改名为最终文件名。
#[tauri::command]
pub async fn finalize_receive(
    id: String,
    name: String,
    size: u64,
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<String, String> {
    let mut s = state.write().await;
    let Some((tmp, written)) = s.active_transfers.remove(&id) else {
        return Err("未找到对应传输".into());
    };
    if written != size {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("文件不完整：已接收 {written} 字节，应为 {size} 字节"));
    }
    let final_name = sanitize_filename(&name);
    let final_path = unique_path(&s.download_dir, &final_name);
    std::fs::rename(&tmp, &final_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("保存文件失败: {e}")
    })?;
    log::info!("已保存手机文件：{}", final_path.display());
    Ok(final_path.display().to_string())
}

fn sanitize_filename(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "received-file".into());
    let base = base.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\0'], "_");
    let base = base.trim();
    if base.is_empty() || base == "." || base == ".." {
        "received-file".into()
    } else {
        base.to_string()
    }
}

/// 清理本应用写到 %TEMP% 的旧剪贴板图片缓存（含历史 drop-app-clipboard-* 命名）。
fn cleanup_stale_clipboard_images() {
    let dir = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ours = (name.starts_with("clipboard-image-") || name.starts_with("drop-app-clipboard-"))
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
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let mut candidate = dir.join(name);
    let mut i = 1;
    while candidate.exists() {
        let stem = Path::new(name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let ext = Path::new(name)
            .extension()
            .map(|s| format!(".{}", s.to_string_lossy()))
            .unwrap_or_default();
        candidate = dir.join(format!("{stem} ({i}){ext}"));
        i += 1;
    }
    candidate
}
