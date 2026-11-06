use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};

use crate::proxy::error_response;
use crate::qqbot::bot::Bot;
use crate::state::AppState;

/// 异步事件投递的超时：投递不在用户请求热路径上（先 ACK 再投递，ADR-Q2），
/// 但也不能让慢订阅者把投递任务堆死。
const DELIVER_TIMEOUT: Duration = Duration::from_secs(5);

/// `/qqbot/callback/{app_id}`：平台回调入口。op 13（回调地址验证）即时
/// 回签，其余 op 立即 ACK（op 12）再异步投递事件字节到 `event_url`
/// （ADR-Q2）。判定顺序见 `docs/sdd/qqbot/interfaces.md`。
pub async fn webhook(
    State(state): State<AppState>,
    Path(app_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // 1. X-Bot-Appid 头存在且与路径 app_id 不一致 → 401。
    if let Some(header) = headers.get("x-bot-appid").and_then(|v| v.to_str().ok()) {
        if header != app_id {
            return error_response(
                StatusCode::UNAUTHORIZED,
                &format!(
                    "X-Bot-Appid '{header}' does not match the app_id '{app_id}' in the callback path"
                ),
            );
        }
    }

    // 2. 查 bot：未找到 → 404；enabled=false 也走 404——回调地址不应再可
    // 达，与转发的 503 语义不同（ADR-Q8）。
    let Some(bot) = state.bots.find(&app_id) else {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("no QQ bot registered for app_id {app_id}"),
        );
    };
    if !bot.enabled {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("QQ bot '{}' (app_id {}) is disabled", bot.id, bot.app_id),
        );
    }

    // 3. body 解析为回调信封：JSON 解析失败 → 400。
    let payload: CallbackPayload = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(err) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("invalid callback payload: {err}"),
            )
        }
    };

    // 4. op 13：回调地址验证，即时回签（sign-only，ADR-Q7 / R4）。
    if payload.op == 13 {
        let data: ValidationData = match serde_json::from_value(payload.d) {
            Ok(data) => data,
            Err(err) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid validation payload for op 13: {err}"),
                )
            }
        };
        let signature = sign_validation(&bot.app_secret, &data.event_ts, &data.plain_token);
        return Json(ValidationResponse {
            plain_token: data.plain_token,
            signature,
        })
        .into_response();
    }

    // 5. 其他 op（含 op 0 事件）：先 ACK，再异步投递（先 ACK 再投递，避免
    // 平台侧超时，ADR-Q2）。投递任务自带 stats guard；ACK 本身不计 stats。
    let content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    tokio::spawn(deliver_event(state, bot, body, content_type));
    Json(serde_json::json!({"op": 12})).into_response()
}

/// 事件投递：`event_url` 未配置 → 计错误丢弃；`POST event_url` 原样字节，
/// `event_secret` 非空时附 `Authorization: Bearer <event_secret>`，
/// `Content-Type` 由调用方给定。5s 超时；非 2xx / 传输错误 / 超时 →
/// `record_error` + warn，**不重试、不向平台暴露**（ADR-Q1）。
pub(crate) async fn deliver_event(state: AppState, bot: Bot, payload: Bytes, content_type: String) {
    let stats = state.bot_stats_for(&bot.id);
    let _guard = stats.guard();

    let Some(event_url) = bot.event_url.as_deref().filter(|url| !url.is_empty()) else {
        stats.record_error();
        tracing::warn!(
            bot = %bot.id,
            app_id = %bot.app_id,
            "event dropped: bot has no event_url configured"
        );
        return;
    };

    let mut request = state
        .client
        .post(event_url)
        .body(payload)
        .header("content-type", &content_type);
    if let Some(secret) = bot.event_secret.as_deref().filter(|s| !s.is_empty()) {
        request = request.header("authorization", format!("Bearer {secret}"));
    }

    match tokio::time::timeout(DELIVER_TIMEOUT, request.send()).await {
        Ok(Ok(response)) if response.status().is_success() => {}
        Ok(Ok(response)) => {
            stats.record_error();
            tracing::warn!(
                bot = %bot.id,
                app_id = %bot.app_id,
                status = %response.status(),
                "event delivery to subscriber failed"
            );
        }
        Ok(Err(err)) => {
            stats.record_error();
            tracing::warn!(
                bot = %bot.id,
                app_id = %bot.app_id,
                error = %err,
                "event delivery to subscriber failed"
            );
        }
        Err(_) => {
            stats.record_error();
            tracing::warn!(
                bot = %bot.id,
                app_id = %bot.app_id,
                "event delivery to subscriber timed out after {}s",
                DELIVER_TIMEOUT.as_secs()
            );
        }
    }
}

/// 回调地址验证的 ed25519 签名，与参考文档 Go 示例等价：
/// `seed = secret` 字节重复直至 ≥ 32 字节取前 32；签名消息为
/// `event_ts + plain_token` 字节拼接（无分隔符）；输出 hex 小写。
/// **只签名，不验签**（ADR-Q7、R4）。
fn sign_validation(secret: &str, event_ts: &str, plain_token: &str) -> String {
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

/// 签名输出 hex 小写（自写，不引 hex crate）。
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// 平台回调信封。`s` / `t` 仅事件推送携带，回调路径不读。
#[derive(Deserialize)]
struct CallbackPayload {
    op: u32,
    d: serde_json::Value,
    #[serde(default)]
    #[allow(dead_code)]
    s: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    t: Option<String>,
}

/// op 13 的 `d`：平台要求回签的两个字段。
#[derive(Deserialize)]
struct ValidationData {
    plain_token: String,
    event_ts: String,
}

/// op 13 的响应体：`{"plain_token": ..., "signature": ...}`。
#[derive(Serialize)]
struct ValidationResponse {
    plain_token: String,
    signature: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qqbot::bot::{Bot, DEFAULT_INTENTS};

    fn bot() -> Bot {
        Bot {
            id: "b1".into(),
            name: "test".into(),
            app_id: "1020304050".into(),
            app_secret: "DG5g3B4j9X2KOErG".into(),
            sandbox: false,
            api_base: None,
            ws_url: None,
            intents: DEFAULT_INTENTS,
            shard: [0, 1],
            event_source: Default::default(),
            event_url: None,
            event_secret: None,
            enabled: true,
        }
    }

    #[test]
    fn signature_is_stable_and_sixty_four_hex_chars() {
        let a = sign_validation("DG5g3B4j9X2KOErG", "1719652200", "abcdef");
        let b = sign_validation("DG5g3B4j9X2KOErG", "1719652200", "abcdef");
        assert_eq!(a, b);
        assert_eq!(a.len(), 128);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn signature_depends_on_every_input() {
        let base = sign_validation("secret", "ts", "token");
        assert_ne!(sign_validation("other", "ts", "token"), base);
        assert_ne!(sign_validation("secret", "other", "token"), base);
        assert_ne!(sign_validation("secret", "ts", "other"), base);
    }

    #[test]
    fn seed_repeats_short_secret_to_32_bytes() {
        // A one-byte secret must extend by repetition rather than panic or
        // degenerate: signature still well-formed, and repeating "aaaa" over
        // a 32-byte seed differs from "aaab".
        let short = sign_validation("a", "ts", "token");
        assert_eq!(short.len(), 128);
        let repeated = sign_validation("ab", "ts", "token");
        let shifted = sign_validation("ba", "ts", "token");
        assert_ne!(repeated, shifted);
    }

    #[test]
    fn empty_secret_still_signs() {
        let sig = sign_validation("", "ts", "token");
        assert_eq!(sig.len(), 128);
    }

    #[test]
    fn to_hex_matches_lowercase_hex() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0x10, 0xff]), "000f10ff");
    }

    #[tokio::test]
    async fn deliver_event_without_event_url_records_error() {
        let state = AppState::new(test_settings()).unwrap();
        deliver_event(
            state.clone(),
            bot(),
            Bytes::from_static(b"{}"),
            "application/json".into(),
        )
        .await;
        let snapshot = state.bot_stats_for("b1").snapshot();
        assert_eq!(snapshot.errors, 1);
        assert_eq!(snapshot.total_requests, 1);
    }

    fn test_settings() -> crate::config::Settings {
        crate::config::Settings {
            bind: "127.0.0.1:0".into(),
            admin_key: None,
            routes_file: "routes.toml".into(),
            providers_file: "providers.toml".into(),
            bots_file: "bots.toml".into(),
            apps_file: "apps.toml".into(),
            usage_file: "usage.toml".into(),
            usage_flush_secs: 60,
            health_interval_secs: 60,
            health_timeout_secs: 5,
            default_timeout_secs: 15,
        }
    }
}
