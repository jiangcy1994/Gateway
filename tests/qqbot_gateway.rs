//! QQ Bot 子网关集成测试。
//!
//! 每条测试对应 `docs/sdd/qqbot/interfaces.md` 的某条契约，映射见 `docs/tdd/mapping.md`。
//! 三类 mock：
//! - mock 平台（axum）：OpenAPI 上游 + `POST /app/getAppAccessToken`
//! - mock 订阅者（axum）：`event_url` 投递目标
//! - mock WSS：`tokio_tungstenite::accept_async` 跑平台侧 WS 对话
//!
//! 本机注意事项：loopback *connection refused* 约 2s，断言 502 的测试用
//! 充裕的 `default_timeout_secs` 与客户端超时，避免抢进 504。

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Method};
use axum::routing::{get, post};
use axum::{Json, Router};
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

use top_gateway::build_app;
use top_gateway::config::{self, Settings};
use top_gateway::qqbot::bot::{EventSource, DEFAULT_INTENTS};
use top_gateway::state::AppState;

const ADMIN_KEY: &str = "test-admin-key";
const APP_ID: &str = "1020304050";
const APP_SECRET: &str = "DG5g3B4j9X2KOErG";
const INTENTS: u64 = 100663296;

// ---------------------------------------------------------------- harness

struct QqGateway {
    addr: SocketAddr,
    _dir: TempDir,
    bots_path: PathBuf,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
}

fn gw_url(gw: &QqGateway, path: &str) -> String {
    format!("http://{}{}", gw.addr, path)
}

async fn spawn_qq_gateway(default_timeout_secs: u64) -> QqGateway {
    let dir = tempfile::tempdir().unwrap();
    let routes_path = dir.path().join("routes.toml");
    let providers_path = dir.path().join("providers.toml");
    let bots_path = dir.path().join("bots.toml");
    let apps_path = dir.path().join("apps.toml");
    config::save_routes(&routes_path, &[]).unwrap();
    config::save_providers(&providers_path, &[]).unwrap();
    config::save_bots(&bots_path, &[]).unwrap();
    let settings = Settings {
        bind: "127.0.0.1:0".to_string(),
        admin_key: Some(ADMIN_KEY.to_string()),
        routes_file: routes_path,
        providers_file: providers_path,
        bots_file: bots_path.clone(),
        apps_file: apps_path.clone(),
        health_interval_secs: 600,
        health_timeout_secs: 2,
        default_timeout_secs,
    };
    let state = AppState::new(settings).unwrap();
    state.routes.set_all(vec![]);
    state.providers.set_all(vec![]);
    state.bots.set_all(vec![]);
    let app = build_app(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    QqGateway {
        addr,
        _dir: dir,
        bots_path,
    }
}

async fn admin(
    gw: &QqGateway,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> reqwest::Response {
    let mut req = client()
        .request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
            gw_url(gw, path),
        )
        .header("X-Admin-Key", ADMIN_KEY);
    if let Some(body) = body {
        req = req.json(body);
    }
    req.send().await.unwrap()
}

/// 建 bot（显式 id / app_id，附加字段覆盖默认）。assert 201。
async fn create_bot(gw: &QqGateway, id: &str, app_id: &str, extra: &Value) -> Value {
    let mut body = json!({
        "id": id,
        "name": "test",
        "app_id": app_id,
        "app_secret": APP_SECRET,
    });
    if let (Value::Object(map), Value::Object(extra)) = (&mut body, extra) {
        for (key, value) in extra {
            map.insert(key.clone(), value.clone());
        }
    }
    let resp = admin(gw, "POST", "/__admin/qqbot/bots", Some(&body)).await;
    assert_eq!(resp.status().as_u16(), 201);
    resp.json().await.unwrap()
}

async fn error_body(resp: reqwest::Response) -> Value {
    let value: Value = resp.json().await.unwrap();
    value.get("error").cloned().unwrap_or(value)
}

/// 轮询条件成立（最多 10s）：异步投递 / supervisor 状态迁移都给它时间。
async fn soon<F>(cond: F) -> bool
where
    F: Fn() -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if cond() {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `soon` 的异步版：条件本身需要发请求（metrics / ws 快照）。
async fn soon_async<F, Fut>(cond: F) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if cond().await {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `GET /__admin/qqbot/ws` 中某个 bot 的 WsStatus。
async fn ws_view(gw: &QqGateway, bot_id: &str) -> Option<Value> {
    let value: Value = admin(gw, "GET", "/__admin/qqbot/ws", None)
        .await
        .json()
        .await
        .unwrap();
    value["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|bot| bot["bot_id"].as_str() == Some(bot_id))
        .cloned()
}

/// `GET /__admin/qqbot/metrics` 中某个 bot 的 stats 快照。
async fn metrics_view(gw: &QqGateway, bot_id: &str) -> Option<Value> {
    let value: Value = admin(gw, "GET", "/__admin/qqbot/metrics", None)
        .await
        .json()
        .await
        .unwrap();
    value["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|bot| bot["bot"]["id"].as_str() == Some(bot_id))
        .map(|bot| bot["stats"].clone())
}

// ---------------------------------------------------------------- mock 平台

#[derive(Default)]
struct Platform {
    token_calls: AtomicU32,
    calls: Mutex<Vec<PlatformCall>>,
}

#[derive(Clone, Debug)]
struct PlatformCall {
    method: String,
    path: String,
    query: Option<String>,
    authorization: Option<String>,
    x_forwarded_path: Option<String>,
    x_forwarded_host: Option<String>,
    x_custom: Option<String>,
}

fn mock_platform() -> (Router, Arc<Platform>) {
    let shared = Arc::new(Platform::default());
    let router = Router::new()
        .route("/app/getAppAccessToken", post(token_endpoint))
        .route("/slow", get(slow_endpoint))
        .fallback(echo)
        .with_state(shared.clone());
    (router, shared)
}

async fn token_endpoint(State(shared): State<Arc<Platform>>) -> Json<Value> {
    let n = shared.token_calls.fetch_add(1, Ordering::SeqCst) + 1;
    Json(json!({"access_token": format!("tok-{n}"), "expires_in": 7200}))
}

async fn slow_endpoint() -> Json<Value> {
    tokio::time::sleep(Duration::from_secs(5)).await;
    Json(json!({"slow": true}))
}

async fn echo(
    State(shared): State<Arc<Platform>>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Json<Value> {
    let call = PlatformCall {
        method: method.to_string(),
        path: uri.path().to_string(),
        query: uri.query().map(str::to_string),
        authorization: headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        x_forwarded_path: headers
            .get("x-forwarded-path")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        x_forwarded_host: headers
            .get("x-forwarded-host")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        x_custom: headers
            .get("x-custom")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    };
    let echoed = json!({
        "method": call.method,
        "path": call.path,
        "query": call.query,
        "authorization": call.authorization,
        "x-forwarded-path": call.x_forwarded_path,
        "x-forwarded-host": call.x_forwarded_host,
        "body": String::from_utf8_lossy(&body),
    });
    shared.calls.lock().unwrap().push(call);
    Json(echoed)
}

async fn spawn_router(router: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    addr
}

// ---------------------------------------------------------------- mock 订阅者

#[derive(Default)]
struct Subscriber {
    posts: Mutex<Vec<SubscriberPost>>,
}

#[derive(Clone, Debug)]
struct SubscriberPost {
    body: Value,
    authorization: Option<String>,
    content_type: Option<String>,
}

fn mock_subscriber() -> (Router, Arc<Subscriber>) {
    let shared = Arc::new(Subscriber::default());
    let router = Router::new()
        .route("/events", post(record_event))
        .with_state(shared.clone());
    (router, shared)
}

async fn record_event(
    State(shared): State<Arc<Subscriber>>,
    headers: HeaderMap,
    body: Bytes,
) -> Json<Value> {
    shared.posts.lock().unwrap().push(SubscriberPost {
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        authorization: headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        content_type: headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    });
    Json(json!({"ok": true}))
}

// ---------------------------------------------------------------- mock WSS

#[derive(Clone)]
struct WsScript {
    heartbeat_ms: u64,
    session_id: String,
    ready_seq: u64,
    after_identify: AfterIdentify,
    after_ready: AfterReady,
}

#[derive(Clone, Copy)]
enum AfterIdentify {
    /// 回 op 0 READY（session_id / seq 取脚本）。
    Ready,
    /// 回 Close(code) 后结束连接。
    Close(u16),
}

#[derive(Clone, Copy)]
enum AfterReady {
    Nothing,
    /// READY 之后立刻投递一个 op 0 事件帧。
    SendEvent,
    /// READY 之后直接断开 TCP（无 close 帧，模拟 1006）。
    Drop,
}

#[derive(Default)]
struct WsShared {
    /// 收到的每个文本帧原文（按连接顺序）。
    frames: Mutex<Vec<String>>,
    /// 接受过的 TCP 连接数（用于「有没有重连」断言）。
    conns: AtomicU32,
}

/// 脚本化平台 WS：每个连接先发 Hello（op 10），再按客户端的 op 应答。
async fn spawn_mock_wss(script: WsScript) -> (SocketAddr, Arc<WsShared>) {
    let shared = Arc::new(WsShared::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept_shared = shared.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let shared = accept_shared.clone();
            let script = script.clone();
            tokio::spawn(async move {
                let mut ws = match accept_async(stream).await {
                    Ok(ws) => ws,
                    Err(_) => return,
                };
                shared.conns.fetch_add(1, Ordering::SeqCst);
                let hello = json!({"op": 10, "d": {"heartbeat_interval": script.heartbeat_ms}});
                if ws.send(Message::text(hello.to_string())).await.is_err() {
                    return;
                }
                while let Some(Ok(msg)) = ws.next().await {
                    match msg {
                        Message::Text(text) => {
                            let frame = text.as_str().to_string();
                            shared.frames.lock().unwrap().push(frame.clone());
                            let Some(payload) = serde_json::from_str::<Value>(&frame).ok() else {
                                continue;
                            };
                            match payload.get("op").and_then(Value::as_u64) {
                                Some(2) => match script.after_identify {
                                    AfterIdentify::Ready => {
                                        let ready = json!({
                                            "op": 0, "s": script.ready_seq, "t": "READY",
                                            "d": {"session_id": script.session_id}
                                        });
                                        if ws.send(Message::text(ready.to_string())).await.is_err()
                                        {
                                            return;
                                        }
                                        match script.after_ready {
                                            AfterReady::Nothing => {}
                                            AfterReady::SendEvent => {
                                                let event = json!({
                                                    "op": 0, "s": 2,
                                                    "t": "GROUP_AT_MESSAGE_CREATE",
                                                    "d": {"group_openid": "g-1", "content": "hello"}
                                                });
                                                if ws
                                                    .send(Message::text(event.to_string()))
                                                    .await
                                                    .is_err()
                                                {
                                                    return;
                                                }
                                            }
                                            AfterReady::Drop => return,
                                        }
                                    }
                                    AfterIdentify::Close(code) => {
                                        let frame = CloseFrame {
                                            code: CloseCode::from(code),
                                            reason: "mock close".into(),
                                        };
                                        let _ = ws.send(Message::Close(Some(frame))).await;
                                        return;
                                    }
                                },
                                Some(1) => {
                                    if ws
                                        .send(Message::text(json!({"op": 11}).to_string()))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                Some(6) => {
                                    let resumed = json!({
                                        "op": 0, "s": script.ready_seq, "t": "RESUMED", "d": {}
                                    });
                                    if ws.send(Message::text(resumed.to_string())).await.is_err() {
                                        return;
                                    }
                                }
                                _ => {}
                            }
                        }
                        Message::Ping(payload) => {
                            let _ = ws.send(Message::Pong(payload)).await;
                        }
                        Message::Close(_) => {
                            let _ = ws.close(None).await;
                            return;
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    (addr, shared)
}

/// mock WSS 收到的帧（按 JSON 解析；非 JSON 帧跳过——平台协议不含）。
fn frame_jsons(shared: &WsShared) -> Vec<Value> {
    shared
        .frames
        .lock()
        .unwrap()
        .iter()
        .filter_map(|frame| serde_json::from_str::<Value>(frame).ok())
        .collect()
}

/// 独立复算 op 13 签名：seed = secret 字节重复至 32；消息 = event_ts +
/// plain_token 拼接；hex 小写。与实现各算一次，钉死算法（ADR-Q7 / R4）。
fn expected_signature(secret: &str, event_ts: &str, plain_token: &str) -> String {
    let bytes = secret.as_bytes();
    let mut seed = [0u8; 32];
    if !bytes.is_empty() {
        for (idx, byte) in seed.iter_mut().enumerate() {
            *byte = bytes[idx % bytes.len()];
        }
    }
    let signing_key = SigningKey::from_bytes(&seed);
    let message = format!("{event_ts}{plain_token}");
    to_hex(&signing_key.sign(message.as_bytes()).to_bytes())
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

// ---------------------------------------------------------------- OpenAPI 透传

#[tokio::test]
async fn forwards_openapi_with_qqbot_token() {
    let (platform, shared) = mock_platform();
    let upstream = spawn_router(platform).await;
    let gw = spawn_qq_gateway(30).await;
    create_bot(
        &gw,
        "fwd",
        APP_ID,
        &json!({"api_base": format!("http://{upstream}")}),
    )
    .await;

    let resp = client()
        .get(format!(
            "http://{}/qqbot/{}/users/@me?foo=bar",
            gw.addr, APP_ID
        ))
        .header("X-Custom", "hi")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()
            .get("x-qqbot-bot")
            .and_then(|value| value.to_str().ok()),
        Some("fwd")
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["method"], "GET");
    assert_eq!(body["path"], "/users/@me");
    assert_eq!(body["query"], "foo=bar");
    assert_eq!(body["authorization"], "QQBot tok-1");
    assert_eq!(
        body["x-forwarded-path"],
        format!("/qqbot/{APP_ID}/users/@me")
    );
    assert_eq!(body["x-forwarded-host"], gw.addr.to_string());

    let calls = shared.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].x_custom.as_deref(), Some("hi"));
    // 一次转发一次取 token（缓存命中不会重复打平台）
    assert_eq!(shared.token_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn forward_rejects_unknown_or_disabled_bots() {
    let gw = spawn_qq_gateway(30).await;
    create_bot(&gw, "dis", APP_ID, &json!({"enabled": false})).await;

    // 缺 app_id 段 → 404
    let resp = client()
        .get(format!("http://{}/qqbot", gw.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    // 未知 app_id → 404
    let resp = client()
        .get(format!("http://{}/qqbot/zzz/users", gw.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let msg = error_body(resp).await["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(msg.contains("app_id zzz"), "{msg}");

    // 禁用 bot → 503（与路由禁用语义一致，转发的 enabled 语义）
    let resp = client()
        .get(format!("http://{}/qqbot/{APP_ID}/users", gw.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 503);
}

#[tokio::test]
async fn forward_502_on_dead_upstream() {
    let gw = spawn_qq_gateway(30).await;
    create_bot(
        &gw,
        "dead",
        APP_ID,
        &json!({"api_base": "http://127.0.0.1:1"}),
    )
    .await;

    let resp = client()
        .get(format!("http://{}/qqbot/{APP_ID}/users/@me", gw.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
    let err = error_body(resp).await;
    assert_eq!(err["code"].as_u64(), Some(502));
}

#[tokio::test]
async fn forward_504_on_timeout() {
    let (platform, _shared) = mock_platform();
    let upstream = spawn_router(platform).await;
    let gw = spawn_qq_gateway(2).await;
    create_bot(
        &gw,
        "slow",
        APP_ID,
        &json!({"api_base": format!("http://{upstream}")}),
    )
    .await;

    let resp = client()
        .get(format!("http://{}/qqbot/{APP_ID}/slow", gw.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 504);
}

// ---------------------------------------------------------------- webhook

#[tokio::test]
async fn webhook_validation_and_error_paths() {
    let gw = spawn_qq_gateway(30).await;
    create_bot(&gw, "cb", APP_ID, &json!({})).await;
    create_bot(&gw, "cbdis", "999000", &json!({"enabled": false})).await;

    let url = format!("http://{}/qqbot/callback/{APP_ID}", gw.addr);

    // op 13 回调地址验证：即时回签（签名独立复算钉死算法）
    let resp = client()
        .post(&url)
        .header("X-Bot-Appid", APP_ID)
        .json(&json!({"op": 13, "d": {"plain_token": "plain-abc", "event_ts": "1719652200"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["plain_token"], "plain-abc");
    assert_eq!(
        body["signature"],
        expected_signature(APP_SECRET, "1719652200", "plain-abc")
    );

    // X-Bot-Appid 与路径 app_id 不一致 → 401
    let resp = client()
        .post(&url)
        .header("X-Bot-Appid", "other")
        .json(&json!({"op": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    // 未知 app_id → 404
    let resp = client()
        .post(format!("http://{}/qqbot/callback/zzz", gw.addr))
        .json(&json!({"op": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    // 禁用 bot → 404（回调语义与转发的 503 不同：回调地址不应再可达）
    let resp = client()
        .post(format!("http://{}/qqbot/callback/999000", gw.addr))
        .json(&json!({"op": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    // body 非 JSON → 400
    let resp = client()
        .post(&url)
        .header("content-type", "application/json")
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    // op 13 的 d 缺字段 → 400
    let resp = client()
        .post(&url)
        .json(&json!({"op": 13, "d": {"plain_token": "x"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    // /qqbot/callback（无 app_id 段）不匹配具名路由，落进 forward → 404
    let resp = client()
        .get(format!("http://{}/qqbot/callback", gw.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let msg = error_body(resp).await["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(msg.contains("app_id callback"), "{msg}");
}

#[tokio::test]
async fn webhook_acks_then_delivers_event() {
    let (sub, sub_shared) = mock_subscriber();
    let sub_addr = spawn_router(sub).await;
    let gw = spawn_qq_gateway(30).await;
    create_bot(
        &gw,
        "evt",
        APP_ID,
        &json!({
            "event_url": format!("http://{sub_addr}/events"),
            "event_secret": "sec-1",
        }),
    )
    .await;

    let event = json!({
        "op": 0,
        "s": 7,
        "t": "GROUP_AT_MESSAGE_CREATE",
        "d": {"group_openid": "g-1", "content": "hello"}
    });
    let resp = client()
        .post(format!("http://{}/qqbot/callback/{APP_ID}", gw.addr))
        .header("X-Bot-Appid", APP_ID)
        .json(&event)
        .send()
        .await
        .unwrap();
    // 立即 ACK（op 12），投递在后台
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, json!({"op": 12}));

    let posts = sub_shared.posts.lock().unwrap().clone();
    if posts.is_empty() {
        let sub_shared = sub_shared.clone();
        assert!(soon(move || !sub_shared.posts.lock().unwrap().is_empty()).await);
    }
    let posts = sub_shared.posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 1);
    // 字节原样投递
    assert_eq!(posts[0].body, event);
    assert_eq!(posts[0].authorization.as_deref(), Some("Bearer sec-1"));
    assert_eq!(posts[0].content_type.as_deref(), Some("application/json"));
}

#[tokio::test]
async fn event_without_event_url_counts_error() {
    let gw = spawn_qq_gateway(30).await;
    create_bot(&gw, "nodest", APP_ID, &json!({})).await;

    let resp = client()
        .post(format!("http://{}/qqbot/callback/{APP_ID}", gw.addr))
        .json(&json!({
            "op": 0, "s": 1, "t": "GROUP_AT_MESSAGE_CREATE",
            "d": {"content": "hello"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // 无 event_url：计错误后丢弃，不重试、不向平台暴露
    assert!(
        soon_async(|| async {
            metrics_view(&gw, "nodest").await.is_some_and(|m| {
                m["errors"].as_u64() >= Some(1) && m["total_requests"].as_u64() >= Some(1)
            })
        })
        .await
    );
}

// ---------------------------------------------------------------- 管理端点

#[tokio::test]
async fn admin_qqbot_endpoints_require_key() {
    let gw = spawn_qq_gateway(30).await;
    let url = format!("http://{}/__admin/qqbot/bots", gw.addr);

    // 无 key / 错 key → 401（R5：子网关管理面复用统一鉴权）
    let resp = client().get(&url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 401);
    let resp = client()
        .get(&url)
        .header("X-Admin-Key", "wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    // X-Admin-Key 或 Bearer → 200
    let resp = client()
        .get(&url)
        .header("X-Admin-Key", ADMIN_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let resp = client()
        .get(&url)
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn bot_admin_crud_and_persistence() {
    let gw = spawn_qq_gateway(30).await;

    // 创建：默认值钉死（契约：省略字段 = 各自默认）
    let bot = create_bot(&gw, "crud", APP_ID, &json!({})).await;
    assert_eq!(bot["name"], "test");
    assert_eq!(bot["sandbox"], false);
    assert_eq!(bot["api_base"], json!(null));
    assert_eq!(bot["ws_url"], json!(null));
    assert_eq!(bot["intents"].as_u64(), Some(INTENTS));
    assert_eq!(bot["shard"], json!([0, 1]));
    assert_eq!(bot["event_source"], "webhook");
    assert_eq!(bot["enabled"], true);

    // 持久化落盘（每次管理变更都原子写 bots.toml）
    let on_disk = std::fs::read_to_string(&gw.bots_path).unwrap();
    assert!(
        on_disk.contains(APP_SECRET),
        "bots.toml must persist app_secret"
    );

    // 列表 / 单查 / 未知 → 404
    let list: Value = admin(&gw, "GET", "/__admin/qqbot/bots", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list["bots"].as_array().unwrap().len(), 1);
    assert_eq!(list["bots"][0]["id"].as_str(), Some("crud"));
    let resp = admin(&gw, "GET", "/__admin/qqbot/bots/crud", None).await;
    assert_eq!(resp.status().as_u16(), 200);
    let resp = admin(&gw, "GET", "/__admin/qqbot/bots/nope", None).await;
    assert_eq!(resp.status().as_u16(), 404);

    // PUT 合并语义：未传字段保留原值
    let resp = admin(
        &gw,
        "PUT",
        "/__admin/qqbot/bots/crud",
        Some(&json!({"sandbox": true})),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let updated: Value = resp.json().await.unwrap();
    assert_eq!(updated["name"], "test");
    assert_eq!(updated["sandbox"], true);
    assert_eq!(updated["app_id"].as_str(), Some(APP_ID));
    let on_disk = std::fs::read_to_string(&gw.bots_path).unwrap();
    assert!(on_disk.contains("sandbox = true"));

    // PUT 未知 id → 404；PUT 校验失败 → 400
    let resp = admin(
        &gw,
        "PUT",
        "/__admin/qqbot/bots/nope",
        Some(&json!({"sandbox": true})),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 404);
    let resp = admin(
        &gw,
        "PUT",
        "/__admin/qqbot/bots/crud",
        Some(&json!({"intents": 0})),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);

    // app_id 重复 → 400
    let resp = admin(
        &gw,
        "POST",
        "/__admin/qqbot/bots",
        Some(&json!({
            "id": "dup", "name": "dup", "app_id": APP_ID, "app_secret": "other-secret"
        })),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
    assert!(error_body(resp).await["message"]
        .as_str()
        .unwrap()
        .contains("app_id"));

    // 缺必填字段 → 422（axum Json 拒绝，非 validate 失败）
    let resp = admin(
        &gw,
        "POST",
        "/__admin/qqbot/bots",
        Some(&json!({"id": "noapp", "name": "x", "app_secret": "s"})),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 422);

    // validate 规则失败 → 400
    let resp = admin(
        &gw,
        "POST",
        "/__admin/qqbot/bots",
        Some(&json!({"id": "bad", "app_id": "111", "app_secret": "s", "intents": 0})),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
    let resp = admin(
        &gw,
        "POST",
        "/__admin/qqbot/bots",
        Some(&json!({
            "id": "bad2", "app_id": "222", "app_secret": "s", "api_base": "ftp://x"
        })),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);

    // 删除：内存表 + 盘!都消失
    let resp = admin(&gw, "DELETE", "/__admin/qqbot/bots/crud", None).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, json!({"deleted": "crud"}));
    let resp = admin(&gw, "GET", "/__admin/qqbot/bots/crud", None).await;
    assert_eq!(resp.status().as_u16(), 404);
    let on_disk = std::fs::read_to_string(&gw.bots_path).unwrap();
    assert!(!on_disk.contains(APP_SECRET));

    // 删除未知 → 404
    let resp = admin(&gw, "DELETE", "/__admin/qqbot/bots/nope", None).await;
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn metrics_view_reports_forward_stats() {
    let (platform, _shared) = mock_platform();
    let upstream = spawn_router(platform).await;
    let gw = spawn_qq_gateway(30).await;
    create_bot(
        &gw,
        "m1",
        APP_ID,
        &json!({"api_base": format!("http://{upstream}")}),
    )
    .await;

    for _ in 0..2 {
        let resp = client()
            .get(format!("http://{}/qqbot/{APP_ID}/users/@me", gw.addr))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let _ = resp.json::<Value>().await;
    }

    assert!(
        soon_async(|| async {
            metrics_view(&gw, "m1").await.is_some_and(|m| {
                m["total_requests"].as_u64() >= Some(2)
                    && m["errors"].as_u64() == Some(0)
                    && m["in_flight"].as_u64() == Some(0)
            })
        })
        .await
    );
    let view = metrics_view(&gw, "m1").await.unwrap();
    assert!(view["avg_latency_ms"].as_f64().is_some_and(|ms| ms >= 0.0));
}

/// `/qqbot` 是保留前缀（ADR-Q5）：路由表里挂在 /qqbot/inner 的路由永远赢
/// 不了 nest，请求仍按 app_id 走 forward。
#[tokio::test]
async fn qqbot_prefix_is_reserved_from_route_table() {
    let (platform, shared) = mock_platform();
    let upstream = spawn_router(platform).await;
    let gw = spawn_qq_gateway(30).await;

    let resp = admin(
        &gw,
        "POST",
        "/__admin/routes",
        Some(&json!({
            "id": "inner",
            "prefix": "/qqbot/inner",
            "upstream": format!("http://{upstream}")
        })),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 201);

    let resp = client()
        .get(format!("http://{}/qqbot/inner/deep", gw.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let msg = error_body(resp).await["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(msg.contains("app_id inner"), "{msg}");
    // 平台没收到任何请求：路由表的 upstream 根本没被用到
    assert!(shared.calls.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------- WS 客户端

#[tokio::test]
async fn ws_supervisor_identifies_heartbeats_and_delivers() {
    let (platform, platform_shared) = mock_platform();
    let api_base = format!("http://{}", spawn_router(platform).await);
    let (sub, sub_shared) = mock_subscriber();
    let event_url = format!("http://{}/events", spawn_router(sub).await);
    let (wss_addr, wss_shared) = spawn_mock_wss(WsScript {
        heartbeat_ms: 500,
        session_id: "sess-1".to_string(),
        ready_seq: 1,
        after_identify: AfterIdentify::Ready,
        after_ready: AfterReady::SendEvent,
    })
    .await;
    let gw = spawn_qq_gateway(10).await;

    create_bot(
        &gw,
        "ws1",
        APP_ID,
        &json!({
            "api_base": api_base,
            "ws_url": format!("ws://{wss_addr}"),
            "event_source": "websocket",
            "event_url": event_url,
            "event_secret": "sec-1",
        }),
    )
    .await;

    // Identify 帧形状：token / intents / shard / properties
    let wss = wss_shared.clone();
    assert!(
        soon(move || frame_jsons(&wss)
            .iter()
            .any(|v| v["op"].as_u64() == Some(2)))
        .await
    );
    let frames = frame_jsons(&wss_shared);
    let identify = frames.iter().find(|v| v["op"].as_u64() == Some(2)).unwrap();
    assert_eq!(identify["d"]["token"], "QQBot tok-1");
    assert_eq!(identify["d"]["intents"].as_u64(), Some(INTENTS));
    assert_eq!(identify["d"]["shard"], json!([0, 1]));
    assert_eq!(identify["d"]["properties"]["$browser"], "top-gateway");

    // 平台事件原样投递到订阅者（先 ACK 再投递，Bearer + Content-Type）
    let sub_poll = sub_shared.clone();
    assert!(soon(move || !sub_poll.posts.lock().unwrap().is_empty()).await);
    let posts = sub_shared.posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["t"], "GROUP_AT_MESSAGE_CREATE");
    assert_eq!(posts[0].authorization.as_deref(), Some("Bearer sec-1"));
    assert_eq!(posts[0].content_type.as_deref(), Some("application/json"));
    assert_eq!(platform_shared.token_calls.load(Ordering::SeqCst), 1);

    // 心跳：d 为 null（首拍）或最新 seq（≥1 证明 seq 已跟踪）
    let wss = wss_shared.clone();
    assert!(
        soon(move || {
            frame_jsons(&wss)
                .iter()
                .any(|v| v["op"].as_u64() == Some(1) && v["d"].as_u64().is_some_and(|d| d >= 1))
        })
        .await
    );
    let all_frames = frame_jsons(&wss_shared);
    let beats: Vec<&Value> = all_frames
        .iter()
        .filter(|v| v["op"].as_u64() == Some(1))
        .collect();
    assert!(!beats.is_empty());
    assert!(beats
        .iter()
        .all(|b| b["d"].is_null() || b["d"].as_u64().is_some_and(|d| d >= 1)));

    // 快照：已连接、session / seq 记录在案、未重连
    let view = ws_view(&gw, "ws1").await.unwrap();
    assert_eq!(view["connected"].as_bool(), Some(true));
    assert_eq!(view["session_id"], "sess-1");
    assert_eq!(view["last_seq"].as_u64(), Some(2));
    assert_eq!(view["reconnects"].as_u64(), Some(0));
    assert!(view["last_error"].is_null());

    // 清理：删 bot → supervisor 停止、快照摘表
    let resp = admin(&gw, "DELETE", "/__admin/qqbot/bots/ws1", None).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert!(soon_async(|| async { ws_view(&gw, "ws1").await.is_none() }).await);
}

/// 关闭码 4014（Identify 被拒，不可重试）：supervisor 停止且不再重连。
#[tokio::test]
async fn ws_supervisor_stops_on_unretryable_close() {
    let (platform, _platform_shared) = mock_platform();
    let api_base = format!("http://{}", spawn_router(platform).await);
    let (wss_addr, wss_shared) = spawn_mock_wss(WsScript {
        heartbeat_ms: 500,
        session_id: "sess-1".to_string(),
        ready_seq: 1,
        after_identify: AfterIdentify::Close(4014),
        after_ready: AfterReady::Nothing,
    })
    .await;
    let gw = spawn_qq_gateway(10).await;

    create_bot(
        &gw,
        "wsstop",
        APP_ID,
        &json!({
            "api_base": api_base,
            "ws_url": format!("ws://{wss_addr}"),
            "event_source": "websocket",
        }),
    )
    .await;

    // 连接过一次（Identify 已到平台）
    let wss = wss_shared.clone();
    assert!(soon(move || !wss.frames.lock().unwrap().is_empty()).await);

    // 4014 → 停止 + 摘表
    assert!(soon_async(|| async { ws_view(&gw, "wsstop").await.is_none() }).await);

    // 一个退避窗口（基数 2s）后仍无重连：连接数恒为 1，无 Resume 帧
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(wss_shared.conns.load(Ordering::SeqCst), 1);
    assert!(!frame_jsons(&wss_shared)
        .iter()
        .any(|v| v["op"].as_u64() == Some(6)));
    assert!(ws_view(&gw, "wsstop").await.is_none());
}

/// READY 后 TCP 突断（1006 形态）：保留 session 重连，重连用 op 6 Resume
/// 带回 session_id 与 seq（R7）。
#[tokio::test]
async fn ws_supervisor_resumes_session_after_abrupt_drop() {
    let (platform, _platform_shared) = mock_platform();
    let api_base = format!("http://{}", spawn_router(platform).await);
    let (wss_addr, wss_shared) = spawn_mock_wss(WsScript {
        heartbeat_ms: 500,
        session_id: "sess-1".to_string(),
        ready_seq: 1,
        after_identify: AfterIdentify::Ready,
        after_ready: AfterReady::Drop,
    })
    .await;
    let gw = spawn_qq_gateway(10).await;

    create_bot(
        &gw,
        "wsresume",
        APP_ID,
        &json!({
            "api_base": api_base,
            "ws_url": format!("ws://{wss_addr}"),
            "event_source": "websocket",
        }),
    )
    .await;

    // 退避（基数 2s）后重连，第二连接先发 Resume
    let wss = wss_shared.clone();
    assert!(
        soon(move || frame_jsons(&wss)
            .iter()
            .any(|v| v["op"].as_u64() == Some(6)))
        .await
    );
    let frames = frame_jsons(&wss_shared);
    let resume = frames.iter().find(|v| v["op"].as_u64() == Some(6)).unwrap();
    assert_eq!(resume["d"]["token"], "QQBot tok-1");
    assert_eq!(resume["d"]["session_id"], "sess-1");
    assert_eq!(resume["d"]["seq"].as_u64(), Some(1));

    // session 跨重连保留，RESUMED 后重新连上
    let view = ws_view(&gw, "wsresume").await.unwrap();
    assert_eq!(view["connected"].as_bool(), Some(true));
    assert_eq!(view["session_id"], "sess-1");
    assert_eq!(view["reconnects"].as_u64(), Some(1));

    let resp = admin(&gw, "DELETE", "/__admin/qqbot/bots/wsresume", None).await;
    assert_eq!(resp.status().as_u16(), 200);
}

/// 示例配置维护条目（非 SDD 契约，见 `examples/README.md`）：钉住
/// `examples/bots.example.toml` 经真实 `load_bots` 解析与 `validate` 通过，
/// 且示例刻意演示的形态不变式成立——防示例文档随代码演进腐坏。
#[test]
fn qqbot_example_configs_parse_and_validate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let bots_path = root.join("examples").join("bots.example.toml");
    assert!(bots_path.exists(), "missing {}", bots_path.display());

    let bots = config::load_bots(&bots_path)
        .unwrap_or_else(|e| panic!("parse {}: {e}", bots_path.display()));

    assert_eq!(bots.len(), 3, "示例应含 3 个机器人");
    for bot in &bots {
        bot.validate()
            .unwrap_or_else(|e| panic!("bot '{}' 无效: {e}", bot.id));
        // 占位凭证必须保持占位形态——示例文件永远不携带真实 app_secret /
        // event_secret（examples/README.md 第 8 节：三个示例文件里的所有
        // api_key / key / app_secret / event_secret 都是占位符）
        assert!(
            bot.app_secret.contains("REPLACE-WITH"),
            "bot '{}' 的 app_secret 必须是占位符",
            bot.id
        );
        if let Some(secret) = &bot.event_secret {
            assert!(
                secret.contains("REPLACE-WITH"),
                "bot '{}' 的 event_secret 必须是占位符",
                bot.id
            );
        }
    }

    // id / app_id 全表唯一（load_bots 对重复只告警不拒绝，示例必须自己干净）
    let ids: HashSet<_> = bots.iter().map(|b| b.id.clone()).collect();
    let app_ids: HashSet<_> = bots.iter().map(|b| b.app_id.clone()).collect();
    assert_eq!(ids.len(), bots.len(), "示例的 id 必须互不相同");
    assert_eq!(app_ids.len(), bots.len(), "示例的 app_id 必须互不相同");

    // 两通道形态都覆盖（webhook 缺省 + websocket 主动连接）
    let sources: Vec<_> = bots.iter().map(|b| b.event_source).collect();
    assert!(
        sources.contains(&EventSource::Webhook),
        "缺 webhook 形态示例"
    );
    assert!(
        sources.contains(&EventSource::Websocket),
        "缺 websocket 形态示例"
    );
    // 只有 websocket 形态需要后台连接（enabled && websocket，联动 supervisor）
    assert_eq!(
        bots.iter().filter(|b| b.ws_needs_connection()).count(),
        1,
        "应只有一条机器人需要 WS 连接"
    );

    // 环境切换与逐机器人 URL 覆盖（ADR-Q12）：按 id 钉住形态归属——
    // 只数个数拴不住示例文件与 examples/README.md 宣称的对应关系
    // （sandbox-ws = 沙箱 websocket；internal-relay = api_base 覆盖），
    // sandbox=true 挪到别的机器人上会让两份文档悄悄变错。LLM 示例测试
    // 同样按 provider id 钉住分层组合，见 tests/llm_gateway.rs。
    assert_eq!(
        bots.iter()
            .filter(|b| b.sandbox)
            .map(|b| b.id.as_str())
            .collect::<Vec<_>>(),
        vec!["sandbox-ws"],
        "演示沙箱形态的应是 sandbox-ws"
    );
    assert_eq!(
        bots.iter()
            .filter(|b| b.sandbox && b.event_source == EventSource::Websocket)
            .count(),
        1,
        "sandbox-ws 应同时演示 websocket 形态"
    );
    assert_eq!(
        bots.iter()
            .filter(|b| b.api_base.is_some())
            .map(|b| b.id.as_str())
            .collect::<Vec<_>>(),
        vec!["internal-relay"],
        "演示 api_base 覆盖的应是 internal-relay"
    );
    assert!(
        bots.iter().all(|b| b.ws_url.is_none()),
        "示例不应依赖推断值之外的 ws_url（escape hatch 保持注释形态）"
    );

    // intents：显式写出的值与省略的缺省值都必须等于平台默认掩码
    for bot in &bots {
        assert_eq!(
            bot.intents, DEFAULT_INTENTS,
            "bot '{}' 的 intents 应为默认掩码",
            bot.id
        );
    }

    // 一条机器人不设 event_url：演示「事件计错误后丢弃」分支（R6）
    assert_eq!(
        bots.iter()
            .filter(|b| b.event_url.is_none())
            .map(|b| b.id.as_str())
            .collect::<Vec<_>>(),
        vec!["internal-relay"],
        "演示不投递事件的应是 internal-relay"
    );
    // 投递凭证钉在 group-qa 上，且必须与 event_url 同体才生效——
    // deliver_event 在无 event_url 时直接计错误丢弃、event_secret 根本
    // 不读（webhook.rs），secret 被挪到不投递的机器人上就是死配置
    assert_eq!(
        bots.iter()
            .filter(|b| b.event_url.is_some() && b.event_secret.is_some())
            .map(|b| b.id.as_str())
            .collect::<Vec<_>>(),
        vec!["group-qa"],
        "演示投递凭证的应是 group-qa（须与 event_url 同体才生效）"
    );
    assert!(bots.iter().all(|b| b.enabled), "示例机器人应全部启用");
}
