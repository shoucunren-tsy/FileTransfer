use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tauri::Emitter;
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message;
use warp::Filter;

use crate::state::{AppState, DEFAULT_PORT};

const MOBILE_HTML: &str = include_str!("../mobile/index.html");
const MOBILE_APP_JS: &str = include_str!("../mobile/app.js");
const MOBILE_QRCODE_JS: &str = include_str!("../mobile/qrcode.js");

#[derive(Debug)]
struct InvalidSession;

impl warp::reject::Reject for InvalidSession {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PhoneMessage {
    Hello { session: String },
    FileMeta { id: String, name: String, size: u64 },
    FileChunk { id: String, seq: u64, data: String },
    FileComplete { id: String },
    FileAck { id: String, status: String },
    Clipboard { text: String },
    ClipboardRequest,
    Ping,
    Pong,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum ServerMessage {
    HelloAck { session: String, name: String },
    FileMeta { id: String, name: String, size: u64 },
    FileChunk { id: String, seq: u64, data: String },
    FileComplete { id: String },
    FileAck { id: String, status: String },
    Clipboard { text: String },
    Error { message: String },
    Ping,
    Pong,
}

fn emit_status(state: &AppState, connected: bool) {
    let _ = state.app.emit(crate::PHONE_STATUS_EVENT, connected);
}

fn static_response(content: &'static str, content_type: &'static str) -> http::Response<&'static str> {
    http::Response::builder()
        .header("Content-Type", content_type)
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
                    Ok::<_, warp::Rejection>(static_response(
                        MOBILE_HTML,
                        "text/html; charset=utf-8",
                    ))
            },
        );

    let qrcode_js = warp::path("qrcode.js")
        .map(|| {
            static_response(MOBILE_QRCODE_JS, "application/javascript; charset=utf-8")
        });

    let app_js = warp::path("app.js")
        .map(|| {
            static_response(MOBILE_APP_JS, "application/javascript; charset=utf-8")
        });

    let ws_route = warp::path("ws")
        .and(warp::ws())
        .and(with_state(state.clone()))
        .map(|ws: warp::ws::Ws, state: Arc<RwLock<AppState>>| {
            ws.on_upgrade(move |socket| handle_ws(socket, state.clone()))
        });

    let routes = page
        .or(qrcode_js)
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
    rt.block_on(serve_with_port_fallback(routes, state));
}

/// 从 DEFAULT_PORT 开始依次尝试绑定；默认端口被占用时自动顺延，
/// 并把实际端口写回状态，保证界面/二维码与真实监听端口一致。
async fn serve_with_port_fallback<F>(
    routes: F,
    state: Arc<RwLock<AppState>>,
) where
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

async fn handle_ws(ws: warp::ws::WebSocket, state: Arc<RwLock<AppState>>) {
    let (mut sink, mut stream) = ws.split();

    // 要求手机在 10 秒内发送 hello 确认会话。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut authed = false;
    while tokio::time::Instant::now() < deadline {
        let next = tokio::time::timeout(Duration::from_millis(500), stream.next()).await;
        match next {
            Ok(Some(Ok(msg))) => {
                if let Ok(text) = msg.to_str() {
                    if let Ok(parsed) = serde_json::from_str::<PhoneMessage>(text) {
                        if let PhoneMessage::Hello { session } = parsed {
                            let ok = state.read().await.session_id == session;
                            if ok {
                                authed = true;
                                let ack = serde_json::to_string(&ServerMessage::HelloAck {
                                    session,
                                    name: "文件互传".into(),
                                })
                                .unwrap_or_default();
                                let _ = sink.send(warp::filters::ws::Message::text(ack)).await;
                                break;
                            } else {
                                let _ = sink.send(warp::filters::ws::Message::close()).await;
                                return;
                            }
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

    // 认证成功：注册出站通道（同一时间仅一台手机）。
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    {
        let mut s = state.write().await;
        if let Some(old) = s.sink.take() {
            let _ = old.send(Message::Close(None));
        }
        s.sink = Some(tx);
        s.connected = true;
        emit_status(&s, true);
        log::info!("手机已连接");
    }

    // 出站循环：同时负责每 25 秒向手机发送 Ping 保活。
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(25));
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { break };
                    let out = match msg {
                        Message::Text(t) => warp::filters::ws::Message::text(t.to_string()),
                        Message::Close(_) => warp::filters::ws::Message::close(),
                        Message::Ping(p) => warp::filters::ws::Message::ping(p),
                        Message::Pong(p) => warp::filters::ws::Message::pong(p),
                        Message::Binary(b) => warp::filters::ws::Message::binary(b.to_vec()),
                        _ => continue,
                    };
                    if sink.send(out).await.is_err() {
                        break;
                    }
                }
                _ = interval.tick() => {
                    if sink.send(warp::filters::ws::Message::ping(vec![0, 1, 2, 3])).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    // 入站循环：转发手机消息到桌面端。
    while let Some(item) = stream.next().await {
        match item {
            Ok(message) if message.is_text() => {
                if let Ok(parsed) = message
                    .to_str()
                    .map_err(|_| ())
                    .and_then(|text| serde_json::from_str::<PhoneMessage>(text).map_err(|_| ()))
                {
                    match parsed {
                        PhoneMessage::Ping => {
                            let pong = serde_json::to_string(&ServerMessage::Pong).unwrap_or_default();
                            let tx = state.read().await.sink.clone();
                            if let Some(tx) = tx {
                                let _ = tx.send(Message::Text(pong.into()));
                            }
                        }
                        PhoneMessage::ClipboardRequest => {
                            let app = state.read().await.app.clone();
                            let _ = app.emit(crate::PHONE_EVENT, r#"{"type":"clipboard-request"}"#);
                        }
                        PhoneMessage::Hello { .. } => {}
                        other => {
                            if let Ok(json) = serde_json::to_string(&other) {
                                let app = state.read().await.app.clone();
                                let _ = app.emit(crate::PHONE_EVENT, &json);
                            }
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    {
        let mut s = state.write().await;
        s.sink = None;
        s.connected = false;
        emit_status(&s, false);
        log::info!("手机已断开");
    }
}

#[tauri::command]
pub async fn send_to_phone(
    payload: serde_json::Value,
    state: tauri::State<'_, Arc<RwLock<AppState>>>,
) -> Result<(), String> {
    let s = state.read().await;
    if let Some(sink) = s.sink.as_ref() {
        sink.send(Message::Text(payload.to_string().into()))
            .map_err(|e| e.to_string())
    } else {
        Err("手机未连接".into())
    }
}
