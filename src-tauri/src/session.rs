use rand::Rng;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::state::{AppState, ServerInfo, MAX_DEVICES};

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
    preferred_lan_ipv4().unwrap_or_else(|| "127.0.0.1".into())
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
    // 只有带默认网关的网卡才是手机实际能访问的那张（虚拟网卡、Host-Only、WSL 都没有网关）。
    // 光看 192.168 前缀会被 VirtualBox Host-Only 的 192.168.56.1 骗到。
    #[cfg(target_os = "windows")]
    if let Some(ip) = gateway_backed_ipv4() {
        return Some(ip.to_string());
    }

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
        "virtual",
        "vmware",
        "vmnet",
        "virtualbox",
        "vbox",
        "vethernet",
        "wsl",
        "docker",
        "vpn",
        "zerotier",
        "tailscale",
        "loopback",
        "bluetooth",
        "tap-",
        "tap ",
        "tun",
        "ppp",
        "teredo",
        "isatap",
        "clash",
        "meta",
        "wireguard",
        "utun",
    ]
    .iter()
    .any(|k| n.contains(k))
}

/// 在带默认网关的物理网卡中挑选最合适的 IPv4。
/// 优先跟随系统默认路由；没有匹配时按私网网段优先级挑选。
#[cfg(target_os = "windows")]
fn gateway_backed_ipv4() -> Option<Ipv4Addr> {
    let mut candidates = gateway_adapters_ipv4();
    if candidates.is_empty() {
        return None;
    }
    if let Some(default_ip) = default_route_ipv4() {
        if let Some((_, ip)) = candidates.iter().find(|(_, ip)| *ip == default_ip) {
            return Some(*ip);
        }
    }
    candidates.sort_by_key(|(_, ip)| lan_ip_priority(*ip));
    candidates.first().map(|(_, ip)| *ip)
}

/// 枚举「已启用 + 有默认网关 + 有 IPv4 单播地址」的网卡，返回 (网卡名, IPv4)。
#[cfg(target_os = "windows")]
fn gateway_adapters_ipv4() -> Vec<(String, Ipv4Addr)> {
    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST,
        GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IF_TYPE_SOFTWARE_LOOPBACK,
        IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR_IN};

    let flags = GAA_FLAG_INCLUDE_GATEWAYS
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER;

    // 先申请一块缓冲，ERROR_BUFFER_OVERFLOW 时按实际大小重试。
    // 用 u64 对齐分配，保证结构体指针满足 8 字节对齐要求。
    let mut size = 16 * 1024u32;
    let mut buf = vec![0u64; size as usize / 8];
    let mut ret = unsafe {
        GetAdaptersAddresses(
            AF_UNSPEC as u32,
            flags,
            std::ptr::null(),
            buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
            &mut size,
        )
    };
    if ret == ERROR_BUFFER_OVERFLOW {
        buf.resize((size as usize).div_ceil(8), 0);
        ret = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC as u32,
                flags,
                std::ptr::null(),
                buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                &mut size,
            )
        };
    }
    if ret != NO_ERROR {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut cur = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !cur.is_null() {
        let adapter = unsafe { &*cur };
        if !adapter.FirstGatewayAddress.is_null()
            && adapter.OperStatus == IfOperStatusUp
            && adapter.IfType != IF_TYPE_SOFTWARE_LOOPBACK
        {
            let name = unsafe { wide_to_string(adapter.FriendlyName) };
            let mut unicast = adapter.FirstUnicastAddress;
            while !unicast.is_null() {
                let ua = unsafe { &*unicast };
                let sock = ua.Address.lpSockaddr as *const SOCKADDR_IN;
                if !sock.is_null() && unsafe { (*sock).sin_family } == AF_INET {
                    let b = unsafe { (*sock).sin_addr.S_un.S_un_b };
                    let ip = Ipv4Addr::new(b.s_b1, b.s_b2, b.s_b3, b.s_b4);
                    if is_lan_ipv4(ip) && !looks_virtual(&name) {
                        out.push((name, ip));
                    }
                    break;
                }
                unicast = ua.Next;
            }
        }
        cur = adapter.Next;
    }
    out
}

#[cfg(target_os = "windows")]
unsafe fn wide_to_string(p: *const u16) -> String {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    if p.is_null() {
        return String::new();
    }
    let mut len = 0isize;
    while *p.offset(len) != 0 {
        len += 1;
    }
    OsString::from_wide(std::slice::from_raw_parts(p, len as usize))
        .to_string_lossy()
        .into_owned()
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
        connected: !s.devices.is_empty(),
        download_dir: s.download_dir.display().to_string(),
        devices: s.device_list().devices,
        max_devices: MAX_DEVICES,
    })
}

#[tauri::command]
pub async fn rotate_session(
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<ServerInfo, String> {
    let mut s = state.write().await;
    s.session_id = new_session_id();
    // 递增连接代次，保证旧连接迟到的事件不会误伤新会话。
    s.conn_id = s.conn_id.wrapping_add(1);
    // 换会话让所有设备下线；旧连接的退出事件随后是 no-op。
    for (_, d) in std::mem::take(&mut s.devices) {
        let _ = d
            .sink
            .try_send(tokio_tungstenite::tungstenite::Message::Close(None));
    }
    // 旧会话未完成的接收传输全部作废，顺手删掉半截临时文件。
    let stale: Vec<_> = s.active_transfers.drain().map(|(_, t)| t.tmp).collect();
    s.emit_devices();
    let info = ServerInfo {
        session_id: s.session_id.clone(),
        ip: lan_ipv4(),
        port: s.port,
        connected: false,
        download_dir: s.download_dir.display().to_string(),
        devices: s.device_list().devices,
        max_devices: MAX_DEVICES,
    };
    drop(s);
    for path in stale {
        log::warn!("更换会话，清理未完成的传输临时文件：{}", path.display());
        let _ = tokio::fs::remove_file(path).await;
    }
    Ok(info)
}
