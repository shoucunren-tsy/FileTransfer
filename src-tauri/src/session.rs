use rand::Rng;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::state::{AppState, ServerInfo};

/// 生成临时会话 ID（字母数字，避免 URL 转义问题）。
pub fn new_session_id() -> String {
    const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    (0..12)
        .map(|_| {
            let i = rng.gen_range(0..CHARS.len());
            CHARS[i] as char
        })
        .collect()
}

/// 获取局域网 IPv4 地址；找不到时回退到 127.0.0.1。
///
/// 优先通过 UDP“连接”让系统选出默认路由对应的网卡地址（UDP connect 不会真正发包），
/// 失败时再遍历网卡并跳过常见虚拟网卡（Hyper-V/WSL/VMware/VPN 等），
/// 避免 Windows 上虚拟网卡排在前面导致二维码显示手机无法访问的地址。
pub fn lan_ipv4() -> String {
    preferred_lan_ipv4()
        .unwrap_or_else(|| "127.0.0.1".into())
}

/// 让系统选择默认路由网卡并返回其 IPv4。
fn default_route_ipv4() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    sock.connect((Ipv4Addr::new(8, 8, 8, 8), 80)).ok()?;
    let IpAddr::V4(ip) = sock.local_addr().ok()?.ip() else {
        return None;
    };
    is_lan_ipv4(ip).then_some(ip)
}

/// 优先选择默认路由对应的真实局域网网卡，避免 VPN/WSL/虚拟机地址进入二维码。
fn preferred_lan_ipv4() -> Option<String> {
    let ifs = get_if_addrs::get_if_addrs().ok()?;
    let default_ip = default_route_ipv4();
    let mut candidates = Vec::new();
    for iface in &ifs {
        if iface.is_loopback() || looks_virtual(&iface.name) {
            continue;
        }
        if let IpAddr::V4(ip) = iface.ip() {
            if is_lan_ipv4(ip) {
                if Some(ip) == default_ip {
                    return Some(ip.to_string());
                }
                candidates.push(ip);
            }
        }
    }
    candidates.sort_by_key(|ip| lan_ip_priority(*ip));
    if let Some(ip) = candidates.first() {
        return Some(ip.to_string());
    }

    // 兜底仅接受非虚拟网卡，避免把 VPN 地址显示给手机。
    for iface in &ifs {
        if iface.is_loopback() || looks_virtual(&iface.name) {
            continue;
        }
        if let IpAddr::V4(ip) = iface.ip() {
            if !ip.is_loopback() && !ip.is_unspecified() {
                return Some(ip.to_string());
            }
        }
    }
    None
}

fn lan_ip_priority(ip: Ipv4Addr) -> u8 {
    let o = ip.octets();
    if o[0] == 192 && o[1] == 168 {
        0
    } else if o[0] == 172 && (16..=31).contains(&o[1]) {
        1
    } else if o[0] == 10 {
        2
    } else {
        3
    }
}

fn is_lan_ipv4(ip: Ipv4Addr) -> bool {
    !ip.is_loopback() && !ip.is_unspecified() && !ip.is_link_local() && is_private_ipv4(ip)
}

fn is_private_ipv4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    (o[0] == 10)
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
        || (o[0] == 100 && (64..=127).contains(&o[1]))
}

/// 常见虚拟网卡 / 隧道名称关键字。
fn looks_virtual(name: &str) -> bool {
    let n = name.to_lowercase();
    [
        "virtual", "vmware", "vmnet", "virtualbox", "vbox", "vethernet", "wsl",
        "docker", "vpn", "zerotier", "tailscale", "loopback", "bluetooth",
        "tap-", "tap ", "tun", "ppp", "teredo", "isatap",
    ]
    .iter()
    .any(|k| n.contains(k))
}

#[tauri::command]
pub async fn get_session_info(
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<ServerInfo, String> {
    let s = state.read().await;
    Ok(ServerInfo {
        session_id: s.session_id.clone(),
        ip: lan_ipv4(),
        port: s.port,
        connected: s.connected,
        download_dir: s.download_dir.display().to_string(),
    })
}

#[tauri::command]
pub async fn rotate_session(
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<ServerInfo, String> {
    let mut s = state.write().await;
    s.session_id = new_session_id();
    // 会话变更后旧连接立即失效。
    if let Some(sink) = s.sink.take() {
        let _ = sink.send(tokio_tungstenite::tungstenite::Message::Close(None));
    }
    s.connected = false;
    Ok(ServerInfo {
        session_id: s.session_id.clone(),
        ip: lan_ipv4(),
        port: s.port,
        connected: s.connected,
        download_dir: s.download_dir.display().to_string(),
    })
}
