mod files;
mod server;
mod session;
mod state;

use std::sync::Arc;

use state::AppState;
use tauri::Manager;
use tokio::sync::RwLock;

/// 收到手机侧消息（file-meta / file-chunk / file-complete / clipboard / hello 等）时
/// 转发给桌面前端的事件名。
pub const PHONE_EVENT: &str = "phone://message";

/// 手机连接状态变化事件（connected / disconnected）。
pub const PHONE_STATUS_EVENT: &str = "phone://status";

pub fn run() {
    let builder = tauri::Builder::default();

    // 单实例必须最先注册：重复启动时激活已有窗口，避免两套服务各占一个端口。
    // macOS 上需要 tauri 的 macos-private-api 特性，故该平台暂不启用。
    #[cfg(not(target_os = "macos"))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.set_focus();
        }
    }));

    builder
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("drop-app".into()),
                    }),
                ])
                .build(),
        )
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            let state = Arc::new(RwLock::new(AppState::new(app.handle().clone())));
            app.manage(state.clone());

            // Windows：便携运行时尽力补充防火墙端口规则；MSI 安装时会以管理员权限配置。
            #[cfg(target_os = "windows")]
            ensure_windows_firewall_rule();

            // 启动 WebSocket 服务（监听 0.0.0.0:8080，仅接受带正确 session 的连接）。
            let server_state = state.clone();
            std::thread::spawn(move || {
                server::run(server_state);
            });

            log::info!("文件互传启动，会话已生成，等待手机扫码连接");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            session::get_session_info,
            session::rotate_session,
            server::send_to_phone,
            server::disconnect_device,
            files::read_file_chunk,
            files::get_file_size,
            files::pick_files,
            files::register_files,
            files::get_clipboard_payload,
            files::pick_download_dir,
            files::reveal_path,
            files::receive_file_chunk,
            files::finalize_receive,
            files::get_recent_logs,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// MSI 已安装规则时不重复修改；便携运行且规则缺失时尝试放行服务端口范围。
#[cfg(target_os = "windows")]
fn ensure_windows_firewall_rule() {
    use std::process::Command;

    let existing = Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "show",
            "rule",
            "name=FileTransfer Local Network",
        ])
        .output();
    if matches!(existing, Ok(ref out) if out.status.success()) {
        log::info!("Windows 防火墙局域网规则已存在");
        return;
    }

    let add = Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "add",
            "rule",
            "name=FileTransfer Local Network",
            "dir=in",
            "action=allow",
            "protocol=TCP",
            "localport=8080-8099",
            "remoteip=LocalSubnet",
            "enable=yes",
            "profile=any",
        ])
        .output();
    match add {
        Ok(out) if out.status.success() => {
            log::info!("已配置 Windows 防火墙局域网端口规则：TCP 8080-8099");
        }
        Ok(out) => {
            log::warn!(
                "无法自动添加 Windows 防火墙规则。请重新安装最新版 MSI，或以管理员身份放行 TCP 8080-8099。netsh 输出：{}{}",
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&out.stderr).trim(),
            );
        }
        Err(err) => log::warn!("添加防火墙规则失败：{err}"),
    }
}
