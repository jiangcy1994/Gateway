use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use crate::qqbot::bot::Bot;
use crate::qqbot::webhook::deliver_event;
use crate::state::AppState;

/// 重连退避基数 / 上限：指数退避 2s → 60s，READY / RESUMED 后归零。
const BACKOFF_BASE: Duration = Duration::from_secs(2);
const BACKOFF_CAP: Duration = Duration::from_secs(60);
/// Hello 的 `heartbeat_interval` 异常时的心跳周期兜底（平台文档示例值）。
const HEARTBEAT_FALLBACK: Duration = Duration::from_secs(30);
/// 连续多少个心跳周期未收到 op 11 ack 就主动断开重连。
const MISSED_ACK_LIMIT: u32 = 2;
/// 401 类关闭码：Identify 载荷被拒，疑似缓存的 token 已失效，停止前清掉
/// 缓存让下次连接强制刷新（`TokenStore::invalidate`，ADR-Q6 语义）。
const TOKEN_REJECTION_CODES: [u16; 2] = [4001, 4002];

/// 每 bot 一个的 WS 监督任务表。任务自行复核配置（记录消失 / 禁用 / 切回
/// webhook → 退出），`stop` 的 abort 只是加速，语义不依赖它。
#[derive(Clone, Default)]
pub struct WsTable {
    entries: Arc<std::sync::Mutex<HashMap<String, WsEntry>>>,
}

struct WsEntry {
    handle: JoinHandle<()>,
    status: Arc<RwLock<WsStatus>>,
}

/// 观测快照，`GET /__admin/qqbot/ws` 的响应面。`session_id` / `last_seq`
/// 跨重连保留（Resume 语义）。
#[derive(Clone, Serialize)]
pub struct WsStatus {
    pub bot_id: String,
    pub app_id: String,
    pub connected: bool,
    pub session_id: Option<String>,
    pub last_seq: u64,
    pub reconnects: u64,
    pub last_error: Option<String>,
}

/// 一轮连接结束后的处理：保留 / 清除 session 重连，或停止。
enum Disconnect {
    Reconnect { keep_session: bool, reason: String },
    Stop { reason: String },
}

/// 平台入站信封。`s` / `t` 只在部分 op 出现。
#[derive(Deserialize)]
struct OpPayload {
    op: u32,
    #[serde(default)]
    d: serde_json::Value,
    #[serde(default)]
    s: Option<u64>,
    #[serde(default)]
    t: Option<String>,
}

impl WsTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 形态需要 WS（`enabled && event_source == websocket`）且表内无该 bot id
    /// → spawn supervisor 并登记；已有 → 无操作；不需要 → 无操作（调用方
    /// 负责先 `stop`）。返回是否有 supervisor 在跑。
    pub fn ensure(&self, bot: &Bot, state: &AppState) -> Option<()> {
        if !bot.ws_needs_connection() {
            return None;
        }
        let mut entries = self.entries.lock().unwrap();
        if entries.contains_key(&bot.id) {
            return Some(());
        }
        let status = Arc::new(RwLock::new(WsStatus {
            bot_id: bot.id.clone(),
            app_id: bot.app_id.clone(),
            connected: false,
            session_id: None,
            last_seq: 0,
            reconnects: 0,
            last_error: None,
        }));
        let table = self.clone();
        let state = state.clone();
        let bot_id = bot.id.clone();
        let entry_status = status.clone();
        let handle = tokio::spawn(supervise(bot_id, state, table, status));
        entries.insert(
            bot.id.clone(),
            WsEntry {
                handle,
                status: entry_status,
            },
        );
        tracing::info!(bot = %bot.id, "websocket supervisor started");
        Some(())
    }

    /// abort handle 并移除表项；不存在 → 无操作。
    pub fn stop(&self, bot_id: &str) {
        if let Some(entry) = self.entries.lock().unwrap().remove(bot_id) {
            entry.handle.abort();
            tracing::info!(bot = %bot_id, "websocket supervisor stopped");
        }
    }

    /// 全量克隆（表内顺序）。
    pub fn snapshot(&self) -> Vec<WsStatus> {
        self.entries
            .lock()
            .unwrap()
            .values()
            .map(|entry| entry.status.read().unwrap().clone())
            .collect()
    }

    /// 只有当表项仍是本任务登记的那一条时才移除（`stop` 之后又被新
    /// `ensure` 重建时不误删新任务）。
    fn drop_entry_if(&self, bot_id: &str, status: &Arc<RwLock<WsStatus>>) {
        let mut entries = self.entries.lock().unwrap();
        if entries
            .get(bot_id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.status, status))
        {
            entries.remove(bot_id);
        }
    }
}

/// supervisor：循环重连 + 心跳 + 事件投递。每轮开始先复核配置（删除 /
/// 禁用 / 切回 webhook → 退出），重连指数退避（READY / RESUMED 归零）。
async fn supervise(bot_id: String, state: AppState, table: WsTable, status: Arc<RwLock<WsStatus>>) {
    let mut backoff = BACKOFF_BASE;
    loop {
        // 1. 复核配置：记录消失 / disabled / 切回 webhook → 退出任务。
        let Some(bot) = state.bots.get(&bot_id) else {
            table.drop_entry_if(&bot_id, &status);
            tracing::info!(bot = %bot_id, "websocket supervisor exiting: bot removed");
            return;
        };
        if !bot.ws_needs_connection() {
            table.drop_entry_if(&bot_id, &status);
            tracing::info!(bot = %bot_id, "websocket supervisor exiting: bot no longer uses websocket");
            return;
        }
        set_status(&status, |s| {
            s.app_id = bot.app_id.clone();
            s.connected = false;
        });

        // 2. 取接入点并连接：失败记错误、退避后下一轮。
        let url = bot.resolve_ws_url();
        let (mut ws, _response) = match tokio_tungstenite::connect_async(url.as_str()).await {
            Ok((ws, response)) => (ws, response),
            Err(err) => {
                reconnect(
                    &bot_id,
                    &status,
                    &mut backoff,
                    format!("connect failed: {err}"),
                )
                .await;
                continue;
            }
        };

        // 3. Hello：取 d.heartbeat_interval（ms，异常 → 30s 兜底）；连接后
        // 收不到有效帧按传输层失败处理（保留 session 重连）。
        let heartbeat = match ws.next().await {
            Some(Ok(Message::Text(text))) => hello_interval(text.as_str()),
            _ => {
                reconnect(&bot_id, &status, &mut backoff, "closed before Hello".into()).await;
                continue;
            }
        };

        // 4. token：失败记 last_error 并退出（凭证问题重连无用）。
        let timeout = Duration::from_secs(state.settings.default_timeout_secs);
        let token = match state
            .bot_tokens
            .get_or_fetch(&state.client, &bot, timeout)
            .await
        {
            Ok(token) => token,
            Err(err) => {
                set_status(&status, |s| {
                    s.last_error = Some(format!("token fetch failed: {err}"));
                    s.connected = false;
                });
                table.drop_entry_if(&bot_id, &status);
                tracing::error!(bot = %bot_id, error = %err, "websocket supervisor exiting: cannot obtain access token");
                return;
            }
        };

        // 有 session 且有 seq → Resume；否则 Identify。
        let session_id = status.read().unwrap().session_id.clone();
        let last_seq = status.read().unwrap().last_seq;
        let sent = if let Some(session) = session_id.filter(|_| last_seq > 0) {
            ws.send(Message::text(
                json!({
                    "op": 6,
                    "d": {
                        "token": format!("QQBot {token}"),
                        "session_id": session,
                        "seq": last_seq,
                    }
                })
                .to_string(),
            ))
            .await
            .is_ok()
        } else {
            ws.send(Message::text(
                json!({
                    "op": 2,
                    "d": {
                        "token": format!("QQBot {token}"),
                        "intents": bot.intents,
                        "shard": bot.shard,
                        "properties": identify_properties(),
                    }
                })
                .to_string(),
            ))
            .await
            .is_ok()
        };
        if !sent {
            reconnect(
                &bot_id,
                &status,
                &mut backoff,
                "identify/resume send failed".into(),
            )
            .await;
            continue;
        }

        // 5. 心跳 + 入站。
        let mut ticker = tokio::time::interval(heartbeat);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // interval 的首次即刻触发：心跳只在 Hello 之后发
        let mut missed_acks: u32 = 0;
        let mut latest_seq: Option<u64> = (last_seq > 0).then_some(last_seq);

        let outcome: Disconnect = loop {
            tokio::select! {
                _ = ticker.tick() => {
                    missed_acks += 1;
                    if missed_acks >= MISSED_ACK_LIMIT {
                        break Disconnect::Reconnect {
                            keep_session: true,
                            reason: format!("no heartbeat ack for {missed_acks} periods"),
                        };
                    }
                    let d = match latest_seq {
                        Some(seq) => json!(seq),
                        None => json!(null), // 首拍 null
                    };
                    if ws.send(Message::text(json!({"op": 1, "d": d}).to_string()))
                        .await
                        .is_err()
                    {
                        break Disconnect::Reconnect {
                            keep_session: true,
                            reason: "heartbeat send failed".into(),
                        };
                    }
                }
                message = ws.next() => {
                    match message {
                        Some(Ok(Message::Text(text))) => {
                            let Ok(payload) = serde_json::from_str::<OpPayload>(text.as_str()) else {
                                break Disconnect::Reconnect {
                                    keep_session: true,
                                    reason: "unreadable inbound frame".into(),
                                };
                            };
                            if let Some(seq) = payload.s {
                                latest_seq = Some(seq);
                            }
                            match payload.op {
                                0 => match payload.t.as_deref().unwrap_or("") {
                                    "READY" => {
                                        let session = payload
                                            .d
                                            .get("session_id")
                                            .and_then(|value| value.as_str())
                                            .map(str::to_string);
                                        set_status(&status, |s| {
                                            s.session_id = session;
                                            s.connected = true;
                                            if let Some(seq) = payload.s {
                                                s.last_seq = seq;
                                            }
                                        });
                                        backoff = BACKOFF_BASE;
                                    }
                                    "RESUMED" => {
                                        set_status(&status, |s| {
                                            s.connected = true;
                                            if let Some(seq) = payload.s {
                                                s.last_seq = seq;
                                            }
                                        });
                                        backoff = BACKOFF_BASE;
                                    }
                                    _ => {
                                        set_status(&status, |s| {
                                            if let Some(seq) = payload.s {
                                                s.last_seq = seq;
                                            }
                                        });
                                        // 事件字节原样投递（与 webhook 同一实现）
                                        tokio::spawn(deliver_event(
                                            state.clone(),
                                            bot.clone(),
                                            Bytes::copy_from_slice(text.as_str().as_bytes()),
                                            "application/json".into(),
                                        ));
                                    }
                                },
                                // Reconnect：断开重连（保留 session）
                                7 => {
                                    break Disconnect::Reconnect {
                                        keep_session: true,
                                        reason: "peer requested reconnect (op 7)".into(),
                                    };
                                }
                                // Invalid Session：清 session 后重连
                                9 => {
                                    break Disconnect::Reconnect {
                                        keep_session: false,
                                        reason: "invalid session (op 9)".into(),
                                    };
                                }
                                // ack
                                11 => missed_acks = 0,
                                _ => {}
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            let _ = ws.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(Message::Close(frame))) => {
                            let code = frame.as_ref().map(|f| u16::from(&f.code));
                            let action = dispatch_close_code(code);
                            if matches!(action, CloseAction::Stop)
                                && code.is_some_and(|c| TOKEN_REJECTION_CODES.contains(&c))
                            {
                                // 401 类关闭码：停止前清掉 token 缓存（ADR-Q6）。
                                state.bot_tokens.invalidate(&bot.app_id);
                            }
                            break match action {
                                CloseAction::ReconnectKeepSession => Disconnect::Reconnect {
                                    keep_session: true,
                                    reason: close_reason(code),
                                },
                                CloseAction::ReconnectClearSession => Disconnect::Reconnect {
                                    keep_session: false,
                                    reason: close_reason(code),
                                },
                                CloseAction::Stop => Disconnect::Stop {
                                    reason: close_reason(code),
                                },
                            };
                        }
                        Some(Err(err)) => {
                            // 传输层错误一律保留 session 重连（ADR-Q / R7）
                            break Disconnect::Reconnect {
                                keep_session: true,
                                reason: format!("read error: {err}"),
                            };
                        }
                        None => {
                            break Disconnect::Reconnect {
                                keep_session: true,
                                reason: "stream ended".into(),
                            };
                        }
                        _ => {} // Binary / Frame：平台协议不含
                    }
                }
            }
        };

        // 6. 分流：不可重试 → 停止；清 session → 重新 Identify；其余保留
        // session 重连。
        set_status(&status, |s| s.connected = false);
        match outcome {
            Disconnect::Stop { reason } => {
                set_status(&status, |s| s.last_error = Some(reason.clone()));
                table.drop_entry_if(&bot_id, &status);
                tracing::error!(bot = %bot_id, "websocket supervisor stopped (non-retryable): {reason}");
                return;
            }
            Disconnect::Reconnect {
                keep_session: false,
                reason,
            } => {
                set_status(&status, |s| s.session_id = None);
                reconnect(&bot_id, &status, &mut backoff, reason).await;
            }
            Disconnect::Reconnect {
                keep_session: true,
                reason,
            } => {
                reconnect(&bot_id, &status, &mut backoff, reason).await;
            }
        }
    }
}

/// 平台关闭码分流（参考文档错误码表）：
/// - 4008 / 4009 → 可 Resume 重连
/// - 4006 / 4007 / 4900~4913 → 清 session 重新 Identify
/// - 4001 / 4002 / 4010~4014 / 4914 / 4915 → 停止
/// - 其他（含 1006 等传输层码）→ 保留 session 重连
fn dispatch_close_code(code: Option<u16>) -> CloseAction {
    match code {
        Some(4008 | 4009) => CloseAction::ReconnectKeepSession,
        Some(4006 | 4007 | 4900..=4913) => CloseAction::ReconnectClearSession,
        Some(4001 | 4002 | 4010..=4014 | 4914 | 4915) => CloseAction::Stop,
        _ => CloseAction::ReconnectKeepSession,
    }
}

enum CloseAction {
    ReconnectKeepSession,
    ReconnectClearSession,
    Stop,
}

fn close_reason(code: Option<u16>) -> String {
    match code {
        Some(code) => format!("peer closed with code {code}"),
        None => "peer closed without code".into(),
    }
}

/// Identify 的 `properties`：`$os` 取编译期平台串，`$browser` / `$device`
/// 记本网关（平台文档：无实际作用，按实际情况填写）。
fn identify_properties() -> serde_json::Value {
    json!({
        "$os": std::env::consts::OS,
        "$browser": "top-gateway",
        "$device": "top-gateway",
    })
}

/// Hello 帧的 `heartbeat_interval`（ms）：op 非 10 / 无字段 / 非数字 → 30s
/// 兜底（平台文档示例值）。
fn hello_interval(text: &str) -> Duration {
    let Ok(payload) = serde_json::from_str::<OpPayload>(text) else {
        return HEARTBEAT_FALLBACK;
    };
    if payload.op != 10 {
        return HEARTBEAT_FALLBACK;
    }
    let Some(ms) = payload.d.get("heartbeat_interval").and_then(|v| v.as_u64()) else {
        return HEARTBEAT_FALLBACK;
    };
    if ms == 0 {
        HEARTBEAT_FALLBACK
    } else {
        Duration::from_millis(ms)
    }
}

/// 记一次失败重连：`reconnects += 1`、`last_error`、断开标记，睡退避后
/// 由调用方进入下一轮（退避在 READY / RESUMED 归零）。
async fn reconnect(
    bot_id: &str,
    status: &Arc<RwLock<WsStatus>>,
    backoff: &mut Duration,
    reason: String,
) {
    set_status(status, |s| {
        s.reconnects += 1;
        s.last_error = Some(reason.clone());
        s.connected = false;
    });
    tracing::warn!(bot = %bot_id, "reconnecting in {:?}", backoff);
    tokio::time::sleep(*backoff).await;
    *backoff = (*backoff * 2).min(BACKOFF_CAP);
}

fn set_status(status: &Arc<RwLock<WsStatus>>, update: impl FnOnce(&mut WsStatus)) {
    let mut guard = status.write().unwrap();
    update(&mut guard);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_code_dispatch_matches_platform_table() {
        // 可 Resume
        for code in [4008u16, 4009] {
            assert!(matches!(
                dispatch_close_code(Some(code)),
                CloseAction::ReconnectKeepSession
            ));
        }
        // 清 session 重新 Identify
        for code in [4006, 4007, 4900, 4905, 4913] {
            assert!(matches!(
                dispatch_close_code(Some(code)),
                CloseAction::ReconnectClearSession
            ));
        }
        // 停止（不可重试）
        for code in [4001, 4002, 4010, 4011, 4012, 4013, 4014, 4914, 4915] {
            assert!(matches!(dispatch_close_code(Some(code)), CloseAction::Stop));
        }
        // 其他（含传输层）保留 session 重连
        for code in [1006, 1000, 1001, 4999, 4916] {
            assert!(matches!(
                dispatch_close_code(Some(code)),
                CloseAction::ReconnectKeepSession
            ));
        }
        assert!(matches!(
            dispatch_close_code(None),
            CloseAction::ReconnectKeepSession
        ));
    }

    #[test]
    fn hello_interval_falls_back_on_bad_payload() {
        assert_eq!(
            hello_interval(r#"{"op":10,"d":{"heartbeat_interval":41250}}"#),
            Duration::from_millis(41250)
        );
        assert_eq!(hello_interval(r#"{"op":10,"d":{}}"#), HEARTBEAT_FALLBACK);
        assert_eq!(
            hello_interval(r#"{"op":0,"d":{"heartbeat_interval":500}}"#),
            HEARTBEAT_FALLBACK
        );
        assert_eq!(hello_interval("not json"), HEARTBEAT_FALLBACK);
        assert_eq!(
            hello_interval(r#"{"op":10,"d":{"heartbeat_interval":0}}"#),
            HEARTBEAT_FALLBACK
        );
    }

    #[test]
    fn identify_properties_shape() {
        let props = identify_properties();
        assert!(props["$os"].is_string());
        assert_eq!(props["$browser"], "top-gateway");
        assert_eq!(props["$device"], "top-gateway");
    }

    #[tokio::test]
    async fn ensure_then_snapshot_then_stop() {
        // A bot that needs a WS connection gets a supervisor entry; stop
        // removes it even though the supervisor (pointed at an unreachable
        // wss in this unit test) keeps its own bookkeeping honest.
        let state = AppState::new(test_settings()).unwrap();
        let bot = Bot {
            id: "b1".into(),
            name: "test".into(),
            app_id: "1020304050".into(),
            app_secret: "DG5g3B4j9X2KOErG".into(),
            sandbox: false,
            api_base: None,
            ws_url: Some("ws://127.0.0.1:1".into()),
            intents: 100663296,
            shard: [0, 1],
            event_source: crate::qqbot::bot::EventSource::Websocket,
            event_url: None,
            event_secret: None,
            enabled: true,
        };
        let table = WsTable::new();
        assert!(table.ensure(&bot, &state).is_some());
        let snapshot = table.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].bot_id, "b1");
        // A supervisor for the same bot is not spawned twice.
        assert!(table.ensure(&bot, &state).is_some());
        assert_eq!(table.snapshot().len(), 1);
        table.stop("b1");
        assert!(table.snapshot().is_empty());
        // Stopping an unknown bot is a no-op.
        table.stop("nope");
    }

    #[tokio::test]
    async fn ensure_ignores_webhook_or_disabled_bot() {
        let state = AppState::new(test_settings()).unwrap();
        let mut bot = Bot {
            id: "b1".into(),
            name: "test".into(),
            app_id: "1020304050".into(),
            app_secret: "DG5g3B4j9X2KOErG".into(),
            sandbox: false,
            api_base: None,
            // ws_url 留空也应当连接（resolve_ws_url 有平台默认接入点），
            // 这里只验证 webhook / disabled 两种不连接的形态。
            ws_url: None,
            intents: 100663296,
            shard: [0, 1],
            event_source: crate::qqbot::bot::EventSource::Webhook,
            event_url: None,
            event_secret: None,
            enabled: true,
        };
        let table = WsTable::new();
        assert!(table.ensure(&bot, &state).is_none());
        bot.event_source = crate::qqbot::bot::EventSource::Websocket;
        bot.enabled = false;
        assert!(table.ensure(&bot, &state).is_none());
        assert!(table.snapshot().is_empty());
    }

    fn test_settings() -> crate::config::Settings {
        crate::config::Settings {
            bind: "127.0.0.1:0".into(),
            admin_key: None,
            routes_file: "routes.toml".into(),
            providers_file: "providers.toml".into(),
            bots_file: "bots.toml".into(),
            health_interval_secs: 60,
            health_timeout_secs: 5,
            default_timeout_secs: 15,
        }
    }
}
