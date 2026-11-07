use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tauri::Emitter;
use tokio::sync::{mpsc, RwLock, Semaphore};
use tokio_tungstenite::tungstenite::Message;
use warp::Filter;

use crate::state::{
    AppState, Device, DEFAULT_PORT, MAX_CLIPBOARD_TEXT, MAX_DEVICES, SINK_BUFFER,
    TRANSFER_IDLE_TIMEOUT,
};

const MOBILE_HTML: &str = include_str!("../mobile/index.html");
const MOBILE_APP_JS: &str = include_str!("../mobile/app.js");
const MOBILE_QRCODE_JS: &str = include_str!("../mobile/qrcode.js");

/// 同一条 WebSocket 消息写入的超时；手机不读数据时不能无限占住发送任务。
const SEND_TIMEOUT: Duration = Duration::from_secs(20);
/// 保活 Ping 写入超时，比普通消息更短。
const PING_SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// 未通过 hello 认证的连接上限，防止局域网内批量开 socket 耗尽资源。
const MAX_PREAUTH_CONNECTIONS: usize = 32;

#[derive(Debug)]
struct InvalidSession;

impl warp::reject::Reject for InvalidSession {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PhoneMessage {
    Hello {
        session: String,
        #[serde(default)]
        name: Option<String>,
    },
    FileMeta {
        id: String,
        name: String,
        size: u64,
    },
    FileChunk {
        id: String,
        seq: u64,
        data: String,
    },
    FileComplete {
        id: String,
    },
    FileAck {
        id: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Clipboard {
        text: String,
    },
    ClipboardRequest,
    Ping,
    Pong,
}

/// 服务端主动下发的消息里，只有握手、错误和心跳需要类型化构造；
/// 文件与剪贴板消息由桌面端以原始 JSON 经 `send_to_phone` 发送。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum ServerMessage {
    HelloAck {
        session: String,
        name: String,
    },
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
    Pong,
}

/// 设备名只用于展示：去掉控制字符、截断 64 字符，空名回退默认值。
fn sanitize_device_name(raw: Option<&str>) -> String {
    let cleaned: String = raw
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "手机设备".into()
    } else {
        trimmed.chars().take(64).collect()
    }
}

/// 手机页与本应用的静态资源统一附带的安全响应头。
fn secure_builder() -> http::response::Builder {
    http::Response::builder()
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "no-referrer")
        .header("Cache-Control", "no-store")
}

fn static_response(
    content: &'static str,
    content_type: &'static str,
) -> http::Response<&'static str> {
    secure_builder()
        .header("Content-Type", content_type)
        .body(content)
        .unwrap()
}

/// 手机页面额外加 CSP：只允许加载同源脚本，禁止外链与内联脚本。
fn page_response(content: &'static str) -> http::Response<&'static str> {
    secure_builder()
        .header("Content-Type", "text/html; charset=utf-8")
        .header(
            "Content-Security-Policy",
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data: blob:; connect-src 'self' ws: wss:; \
             base-uri 'none'; form-action 'none'",
        )
        .body(content)
        .unwrap()
}

pub fn run(state: Arc<RwLock<AppState>>) {
    // 手机页面：带会话校验，无效会话返回 403。
    let page = warp::path::end()
        .and(warp::query::<HashMap<String, String>>())
        .and(with_state(state.clone()))
        .and_then(
            |q: HashMap<String, String>, state: Arc<RwLock<AppState>>| async move {
                let session = q.get("session").cloned().unwrap_or_default();
                let ok = {
                    let s = state.read().await;
                    !session.is_empty() && session == s.session_id
                };
                if !ok {
                    return Err(warp::reject::custom(InvalidSession));
                }
                Ok::<_, warp::Rejection>(page_response(MOBILE_HTML))
            },
        );

    let qrcode_js = warp::path("qrcode.js")
        .map(|| static_response(MOBILE_QRCODE_JS, "application/javascript; charset=utf-8"));

    let app_js = warp::path("app.js")
        .map(|| static_response(MOBILE_APP_JS, "application/javascript; charset=utf-8"));

    let ws_route = warp::path("ws")
        .and(warp::addr::remote())
        .and(warp::ws())
        .and(with_state(state.clone()))
        .map(
            |addr: Option<SocketAddr>, ws: warp::ws::Ws, state: Arc<RwLock<AppState>>| {
                // IP 仅用于设备列表展示，不参与鉴权。
                let ip = addr.map(|a| a.ip().to_string());
                ws.on_upgrade(move |socket| handle_ws(socket, state.clone(), ip))
            },
        );

    let routes =
        page.or(qrcode_js)
            .or(app_js)
            .or(ws_route)
            .recover(|err: warp::Rejection| async move {
                let code = if err.find::<InvalidSession>().is_some() {
                    http::StatusCode::FORBIDDEN
                } else {
                    http::StatusCode::NOT_FOUND
                };
                Ok::<_, Infallible>(
                    http::Response::builder()
                        .status(code)
                        .header("Content-Type", "text/plain; charset=utf-8")
                        .body("Not allowed")
                        .unwrap(),
                )
            });

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    rt.block_on(async move {
        spawn_cleanup(state.clone());
        serve_with_port_fallback(routes, state).await;
    });
}

/// 定期清理闲置传输与过期路径授权，防止长跑时临时文件和内存无限增长。
fn spawn_cleanup(state: Arc<RwLock<AppState>>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            cleanup_idle_transfers(&state).await;
            let files = {
                let mut s = state.write().await;
                s.prune_allowed_paths();
                let (changed, files) = s.prune_stale();
                if changed {
                    s.emit_devices();
                }
                files
            };
            remove_temp_files(files).await;
        }
    });
}

/// 删除归还的临时文件；失败只记日志，不影响后续清理。
async fn remove_temp_files(paths: Vec<PathBuf>) {
    for path in paths {
        log::warn!("清理未完成的传输临时文件：{}", path.display());
        let _ = tokio::fs::remove_file(path).await;
    }
}

async fn cleanup_idle_transfers(state: &Arc<RwLock<AppState>>) {
    let now = Instant::now();
    let expired = {
        let mut s = state.write().await;
        let mut expired = Vec::new();
        s.active_transfers.retain(|_, t| {
            if now.duration_since(t.last_activity) > TRANSFER_IDLE_TIMEOUT {
                expired.push(t.tmp.clone());
                false
            } else {
                true
            }
        });
        expired
    };
    for path in expired {
        log::warn!("清理超时未完成的传输临时文件：{}", path.display());
        let _ = tokio::fs::remove_file(path).await;
    }
}

/// 从 DEFAULT_PORT 开始依次尝试绑定；默认端口被占用时自动顺延，
/// 并把实际端口写回状态，保证界面/二维码与真实监听端口一致。
async fn serve_with_port_fallback<F>(routes: F, state: Arc<RwLock<AppState>>)
where
    F: warp::Filter + Clone + Send + Sync + 'static,
    F::Extract: warp::Reply,
{
    for port in DEFAULT_PORT..DEFAULT_PORT + 20 {
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port)).await {
            Ok(listener) => {
                log::info!("WebSocket 服务已监听 0.0.0.0:{}", port);
                state.write().await.port = port;
                warp::serve(routes.clone()).incoming(listener).run().await;
                return;
            }
            Err(err) => {
                log::warn!("端口 {} 绑定失败（{}），尝试下一个端口", port, err);
            }
        }
    }
    log::error!(
        "端口 {}-{} 均不可用，WebSocket 服务启动失败",
        DEFAULT_PORT,
        DEFAULT_PORT + 19
    );
}

fn with_state(
    state: Arc<RwLock<AppState>>,
) -> impl Filter<Extract = (Arc<RwLock<AppState>>,), Error = Infallible> + Clone {
    warp::any().map(move || state.clone())
}

/// 向 WebSocket 写一条消息；超时或发送失败都返回 false，调用方据此断开，
/// 防止手机不读数据时发送任务被永久占住。
async fn send_with_timeout<S>(
    sink: &mut S,
    msg: warp::filters::ws::Message,
    limit: Duration,
) -> bool
where
    S: futures_util::Sink<warp::filters::ws::Message> + Unpin,
{
    matches!(
        tokio::time::timeout(limit, sink.send(msg)).await,
        Ok(Ok(()))
    )
}

async fn handle_ws(ws: warp::ws::WebSocket, state: Arc<RwLock<AppState>>, ip: Option<String>) {
    // 认证前连接限流：超限直接断开，避免局域网内批量开 socket 耗尽资源。
    static PREAUTH: OnceLock<Semaphore> = OnceLock::new();
    let preauth_permit = match PREAUTH
        .get_or_init(|| Semaphore::new(MAX_PREAUTH_CONNECTIONS))
        .try_acquire()
    {
        Ok(permit) => permit,
        Err(_) => {
            log::warn!("未认证连接数已达上限 {MAX_PREAUTH_CONNECTIONS}，拒绝新连接");
            return;
        }
    };

    let (mut sink, mut stream) = ws.split();

    // 要求手机在 10 秒内发送 hello 确认会话。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut authed = false;
    let mut hello_session = String::new();
    let mut hello_name: Option<String> = None;
    while tokio::time::Instant::now() < deadline {
        let next = tokio::time::timeout(Duration::from_millis(500), stream.next()).await;
        match next {
            Ok(Some(Ok(msg))) => {
                if let Ok(text) = msg.to_str() {
                    if let Ok(PhoneMessage::Hello { session, name }) =
                        serde_json::from_str::<PhoneMessage>(text)
                    {
                        let ok = state.read().await.session_id == session;
                        if ok {
                            authed = true;
                            hello_session = session;
                            hello_name = name;
                            break;
                        } else {
                            // 明确告知会话失效，否则手机端会把它当成普通断网无限重连。
                            let msg = serde_json::to_string(&ServerMessage::Error {
                                message: "会话已失效，请重新扫码".into(),
                                code: Some("session-expired".into()),
                            })
                            .unwrap_or_default();
                            let _ = tokio::time::timeout(
                                SEND_TIMEOUT,
                                sink.send(warp::filters::ws::Message::text(msg)),
                            )
                            .await;
                            let _ = tokio::time::timeout(
                                SEND_TIMEOUT,
                                sink.send(warp::filters::ws::Message::close()),
                            )
                            .await;
                            return;
                        }
                    }
                }
            }
            Ok(Some(Err(_))) | Ok(None) => return,
            _ => {}
        }
    }
    if !authed {
        return;
    }
    // 已通过认证，让出认证前的限流名额；后续由设备上限约束。
    drop(preauth_permit);

    // 认证成功：注册出站通道。每台设备一条队列，最多 MAX_DEVICES 台。
    let name = sanitize_device_name(hello_name.as_deref());
    let (tx, mut rx) = mpsc::channel::<Message>(SINK_BUFFER);
    let my_conn_id;
    let mut cleanup_files: Vec<PathBuf> = Vec::new();
    {
        let mut s = state.write().await;
        let (pruned, files) = s.prune_stale();
        cleanup_files.extend(files);
        if pruned {
            s.emit_devices();
        }

        // 同一台手机刷新/重复标签页会短暂占两个连接：同 IP + 同名视为旧连接并替换。
        if let Some(ref my_ip) = ip {
            let duplicates: Vec<u64> = s
                .devices
                .iter()
                .filter(|(_, d)| d.ip.as_deref() == Some(my_ip.as_str()) && d.name == name)
                .map(|(id, _)| *id)
                .collect();
            for id in duplicates {
                if let Some((old, tmp)) = s.take_device(id) {
                    cleanup_files.extend(tmp);
                    let replaced = serde_json::to_string(&ServerMessage::Error {
                        message: "同一设备已在新页面连接，此页面停止重连".into(),
                        code: Some("device-replaced".into()),
                    })
                    .map(Message::Text)
                    .unwrap_or(Message::Close(None));
                    let _ = old.sink.try_send(replaced);
                    let _ = old.sink.try_send(Message::Close(None));
                    log::info!("替换重复设备连接：{} ({})", old.name, id);
                }
            }
        }

        if s.devices.len() >= MAX_DEVICES {
            drop(s);
            let msg = serde_json::to_string(&ServerMessage::Error {
                message: format!("最多连接 {MAX_DEVICES} 台设备，请先在电脑端断开一台"),
                code: Some("device-cap".into()),
            })
            .unwrap_or_default();
            let _ = tokio::time::timeout(
                SEND_TIMEOUT,
                sink.send(warp::filters::ws::Message::text(msg)),
            )
            .await;
            let _ =
                tokio::time::timeout(SEND_TIMEOUT, sink.send(warp::filters::ws::Message::close()))
                    .await;
            log::info!("拒绝连接：设备数已达上限 {MAX_DEVICES}");
            remove_temp_files(cleanup_files).await;
            return;
        }

        s.conn_id = s.conn_id.wrapping_add(1);
        my_conn_id = s.conn_id;
        s.devices.insert(
            my_conn_id,
            Device {
                id: my_conn_id,
                name: name.clone(),
                ip: ip.clone(),
                sink: tx.clone(),
                last_activity: Instant::now(),
            },
        );
        s.emit_devices();
    }
    remove_temp_files(cleanup_files).await;

    // 注册成功后才回 hello-ack；否则手机先看到「已连接」，紧接着又收到 device-cap。
    let ack = serde_json::to_string(&ServerMessage::HelloAck {
        session: hello_session,
        name: "文件互传".into(),
    })
    .unwrap_or_default();
    if !send_with_timeout(
        &mut sink,
        warp::filters::ws::Message::text(ack),
        SEND_TIMEOUT,
    )
    .await
    {
        retire_device(&state, my_conn_id).await;
        return;
    }
    log::info!("设备已连接：{} ({})", name, my_conn_id);

    // 出站循环：同时负责每 25 秒向手机发送 Ping 保活。
    let out_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(25));
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { break };
                    let closing = matches!(msg, Message::Close(_));
                    let out = match msg {
                        Message::Text(t) => warp::filters::ws::Message::text(t.to_string()),
                        Message::Close(_) => warp::filters::ws::Message::close(),
                        Message::Ping(p) => warp::filters::ws::Message::ping(p),
                        Message::Pong(p) => warp::filters::ws::Message::pong(p),
                        Message::Binary(b) => warp::filters::ws::Message::binary(b.to_vec()),
                        _ => continue,
                    };
                    if !send_with_timeout(&mut sink, out, SEND_TIMEOUT).await {
                        break;
                    }
                    if closing {
                        break;
                    }
                }
                _ = interval.tick() => {
                    if !send_with_timeout(
                        &mut sink,
                        warp::filters::ws::Message::ping(vec![0, 1, 2, 3]),
                        PING_SEND_TIMEOUT,
                    )
                    .await
                    {
                        break;
                    }
                }
            }
        }
        retire_device(&out_state, my_conn_id).await;
    });

    // 入站循环：给消息加上来源设备信息后转发给桌面端。
    let mut last_touch = Instant::now();
    while let Some(item) = stream.next().await {
        match item {
            Ok(message) if message.is_text() => {
                // 设备可能已被用户断开、被更换会话清空或判为僵尸；设备不在册就不再转发。
                if !state.read().await.devices.contains_key(&my_conn_id) {
                    break;
                }
                if let Ok(parsed) = message
                    .to_str()
                    .map_err(|_| ())
                    .and_then(|text| serde_json::from_str::<PhoneMessage>(text).map_err(|_| ()))
                {
                    // 心跳也走这里，刷新活跃时间即可判断僵尸连接；写入节流，避免每块都抢锁。
                    if last_touch.elapsed() >= Duration::from_secs(2) {
                        last_touch = Instant::now();
                        let mut s = state.write().await;
                        if let Some(d) = s.devices.get_mut(&my_conn_id) {
                            d.last_activity = Instant::now();
                        }
                    }

                    match parsed {
                        PhoneMessage::Ping => {
                            let pong =
                                serde_json::to_string(&ServerMessage::Pong).unwrap_or_default();
                            // 只回本连接，不能回全局设备；队列满说明手机已不读数据，直接断开。
                            if tx.try_send(Message::Text(pong)).is_err() {
                                break;
                            }
                        }
                        PhoneMessage::ClipboardRequest => {
                            let app = state.read().await.app.clone();
                            let envelope = serde_json::json!({
                                "deviceId": my_conn_id,
                                "deviceName": name,
                                "ip": ip,
                                "message": { "type": "clipboard-request" },
                            });
                            let _ = app.emit(crate::PHONE_EVENT, envelope.to_string());
                        }
                        PhoneMessage::Hello { .. } => {}
                        other => {
                            if let PhoneMessage::Clipboard { text } = &other {
                                if text.len() > MAX_CLIPBOARD_TEXT {
                                    let err = serde_json::to_string(&ServerMessage::Error {
                                        message: "剪贴板内容超过 1 MiB 上限".into(),
                                        code: None,
                                    })
                                    .unwrap_or_default();
                                    if tx.try_send(Message::Text(err)).is_err() {
                                        break;
                                    }
                                    continue;
                                }
                            }
                            let app = state.read().await.app.clone();
                            let envelope = serde_json::json!({
                                "deviceId": my_conn_id,
                                "deviceName": name,
                                "ip": ip,
                                "message": other,
                            });
                            let _ = app.emit(crate::PHONE_EVENT, envelope.to_string());
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    retire_device(&state, my_conn_id).await;
}

/// 移除一台设备并通知前端；重复调用是安全的。
/// 同时回收该设备未完成的接收传输，避免临时文件无人认领。
async fn retire_device(state: &Arc<RwLock<AppState>>, id: u64) {
    let removed = {
        let mut s = state.write().await;
        let removed = s.take_device(id);
        if removed.is_some() {
            s.emit_devices();
        }
        removed
    };
    if let Some((d, files)) = removed {
        // 某些退出路径（例如入站循环结束）通道仍开着，补一条 Close 让出站任务收尾。
        let _ = d.sink.try_send(Message::Close(None));
        log::info!("设备已断开：{} ({})", d.name, id);
        remove_temp_files(files).await;
    }
}

#[tauri::command]
pub async fn send_to_phone(
    payload: serde_json::Value,
    device_id: u64,
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<(), String> {
    if payload.get("type").and_then(|v| v.as_str()) == Some("clipboard") {
        if let Some(text) = payload.get("text").and_then(|v| v.as_str()) {
            if text.len() > MAX_CLIPBOARD_TEXT {
                return Err("剪贴板内容超过 1 MiB 上限".into());
            }
        }
    }
    let sink = {
        let s = state.read().await;
        s.devices.get(&device_id).map(|d| d.sink.clone())
    };
    match sink {
        Some(tx) => {
            match tokio::time::timeout(SEND_TIMEOUT, tx.send(Message::Text(payload.to_string())))
                .await
            {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("设备响应超时，发送已中止".into()),
            }
        }
        None => Err("目标设备已断开".into()),
    }
}

/// 主动断开一台设备；设备已不存在时视为成功（幂等）。
#[tauri::command]
pub async fn disconnect_device(
    device_id: u64,
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<(), String> {
    if let Some(name) = state
        .read()
        .await
        .devices
        .get(&device_id)
        .map(|d| d.name.clone())
    {
        log::info!("用户断开设备：{} ({})", name, device_id);
    }
    retire_device(state.inner(), device_id).await;
    Ok(())
}
