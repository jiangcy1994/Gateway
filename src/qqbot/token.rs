use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::error::ApiError;
use crate::qqbot::bot::Bot;

/// 剩余有效期不足此时长才触发后台刷新（判定顺序第 2 步的边界）。
const REFRESH_MARGIN: Duration = Duration::from_secs(300);
/// 两次刷新之间的最小间隔：临近过期时平台在最后 60s 才发新 token，
/// 短于冷静期的刷新只会拿到同一个字符串（ADR-Q6）。
const REFRESH_COOLDOWN: Duration = Duration::from_secs(30);
/// `expires_in` 缺失 / 不可解析时的兜底有效期（平台文档示例值）。
const DEFAULT_EXPIRY: Duration = Duration::from_secs(7200);
/// `expires_in` 上限保护：平台返回异常值时不能把缓存钉死。
const MAX_EXPIRY: Duration = Duration::from_secs(86400);

/// 按 **app_id** 缓存的 access_token。每个 app_id 一个 slot：同一 app_id
/// 的配置行被删除重建不丢失缓存，删除机器人后 slot 残留（惰性清理，
/// 与 `QuotaTracker` 同策略）。换 token 的接口 HTTP 恒为 200，成功与否
/// 看响应体有没有 `access_token`（ADR-Q3）。
#[derive(Clone, Default)]
pub struct TokenStore {
    slots: Arc<RwLock<HashMap<String, Arc<TokenSlot>>>>,
}

struct TokenSlot {
    /// 单飞锁：并发刷新只真正执行一次，其余等结果。
    fetch_lock: tokio::sync::Mutex<()>,
    /// 内层状态，快路径只在这里做无 await 的短锁读。
    inner: std::sync::Mutex<TokenInner>,
}

impl Default for TokenSlot {
    fn default() -> Self {
        Self {
            fetch_lock: tokio::sync::Mutex::new(()),
            inner: std::sync::Mutex::new(TokenInner {
                current: None,
                last_fetch: None,
            }),
        }
    }
}

struct TokenInner {
    current: Option<TokenEntry>,
    /// 最近一次真正打到平台的换 token 请求时间，用于刷新冷静期。
    last_fetch: Option<Instant>,
}

#[derive(Clone)]
struct TokenEntry {
    token: String,
    expires_at: Instant,
}

impl TokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn slot_for(&self, app_id: &str) -> Arc<TokenSlot> {
        if let Some(slot) = self.slots.read().unwrap().get(app_id) {
            return slot.clone();
        }
        let mut slots = self.slots.write().unwrap();
        slots
            .entry(app_id.to_string())
            .or_insert_with(|| Arc::new(TokenSlot::default()))
            .clone()
    }

    /// 取一个有效 token：快路径直接用缓存，慢路径持单飞锁后台刷新。
    /// 判定顺序见 `docs/sdd/qqbot/interfaces.md`——改这里之前先改文档。
    pub async fn get_or_fetch(
        &self,
        client: &reqwest::Client,
        bot: &Bot,
        timeout: Duration,
    ) -> Result<String, ApiError> {
        let slot = self.slot_for(&bot.app_id);

        // 快路径：纯内存读，不持 tokio 锁、不做 await。
        if let Some(token) = fresh_token(&slot) {
            return Ok(token);
        }

        let _fetch = slot.fetch_lock.lock().await;

        // 二次检查：等锁期间可能已被别的请求刷新过。
        if let Some(token) = fresh_token(&slot) {
            return Ok(token);
        }
        // 情形 2 的冷静期：旧 token 还在有效期内但临近过期，且刚打过平台。
        if let Some(token) = cooldown_token(&slot) {
            return Ok(token);
        }

        match fetch_token(client, bot, timeout).await {
            Ok(entry) => {
                let mut inner = slot.inner.lock().unwrap();
                let now = Instant::now();
                // 平台在临近过期 60s 窗口外仍返同一个 token 字符串：此时
                // 只更新 last_fetch、保留原 expires_at——回写会错误延长
                // 有效期（ADR-Q6）。
                let same_token = inner
                    .current
                    .as_ref()
                    .is_some_and(|old| old.token == entry.token);
                inner.last_fetch = Some(now);
                if same_token {
                    Ok(inner.current.as_ref().unwrap().token.clone())
                } else {
                    let token = entry.token.clone();
                    inner.current = Some(entry);
                    Ok(token)
                }
            }
            Err(err) => {
                // 刷新失败但旧 token 仍有效：告警并回退，不让一次平台抖动
                // 变成用户的 502（已经过期的则如实返回 Err）。
                let inner = slot.inner.lock().unwrap();
                if let Some(entry) = &inner.current {
                    if entry.expires_at > Instant::now() {
                        tracing::warn!(
                            app_id = %bot.app_id,
                            error = %err,
                            "token refresh failed; serving the previous token while it is still valid"
                        );
                        return Ok(entry.token.clone());
                    }
                }
                Err(err)
            }
        }
    }

    /// 清掉当前 token，下次 `get_or_fetch` 强制刷新（WS 收到 401 类关闭码后）。
    pub fn invalidate(&self, app_id: &str) {
        let slots = self.slots.read().unwrap();
        if let Some(slot) = slots.get(app_id) {
            slot.inner.lock().unwrap().current = None;
        }
    }
}

/// 情形 1：`remaining > 300s`，直接用。
fn fresh_token(slot: &TokenSlot) -> Option<String> {
    let inner = slot.inner.lock().unwrap();
    let entry = inner.current.as_ref()?;
    if entry.expires_at.saturating_duration_since(Instant::now()) > REFRESH_MARGIN {
        Some(entry.token.clone())
    } else {
        None
    }
}

/// 情形 2 的冷静期分支：`0 < remaining ≤ 300s` 且距 `last_fetch` < 30s，
/// 返回旧 token、不打平台（ADR-Q6）。
fn cooldown_token(slot: &TokenSlot) -> Option<String> {
    let inner = slot.inner.lock().unwrap();
    let entry = inner.current.as_ref()?;
    let remaining = entry.expires_at.saturating_duration_since(Instant::now());
    if remaining == Duration::ZERO {
        return None;
    }
    match inner.last_fetch {
        Some(last) if last.elapsed() < REFRESH_COOLDOWN => Some(entry.token.clone()),
        _ => None,
    }
}

/// `POST {api_base}/app/getAppAccessToken`。HTTP 恒为 200，以响应体
/// `access_token` 存在与否判成功/失败（ADR-Q3）。
async fn fetch_token(
    client: &reqwest::Client,
    bot: &Bot,
    timeout: Duration,
) -> Result<TokenEntry, ApiError> {
    let url = format!("{}/app/getAppAccessToken", bot.resolve_api_base());
    let request = client.post(&url).json(&serde_json::json!({
        "appId": bot.app_id,
        "clientSecret": bot.app_secret,
    }));
    // send 与 body 读取都包在超时里：换 token 在用户请求热路径上，不能被
    // 平台挂死。
    let fetched = tokio::time::timeout(timeout, async {
        let response = request
            .send()
            .await
            .map_err(|e| ApiError::TokenFetchFailed(format!("getAppAccessToken: {e}")))?;
        let status = response.status();
        let body: TokenResponse = response.json().await.map_err(|e| {
            ApiError::TokenFetchFailed(format!("getAppAccessToken returned {status}: {e}"))
        })?;
        Ok::<_, ApiError>(body)
    })
    .await;

    let body = match fetched {
        Ok(Ok(body)) => body,
        Ok(Err(err)) => return Err(err),
        Err(_) => {
            return Err(ApiError::TokenFetchFailed(format!(
                "getAppAccessToken timed out after {}s",
                timeout.as_secs()
            )))
        }
    };

    let Some(token) = body.access_token else {
        // 平台的错误 body：{"code": ..., "message": ...}（HTTP 仍是 200）
        let code = body.code.as_ref().map(value_plain).unwrap_or_default();
        let message = body.message.as_ref().map(value_plain).unwrap_or_default();
        return Err(if code.is_empty() && message.is_empty() {
            ApiError::TokenFetchFailed(
                "getAppAccessToken succeeded but the body has no access_token and no error code"
                    .to_string(),
            )
        } else {
            ApiError::TokenFetchFailed(format!(
                "getAppAccessToken rejected: code={code}, message={message}"
            ))
        });
    };

    let ttl = body
        .expires_in
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_EXPIRY)
        .min(MAX_EXPIRY);
    Ok(TokenEntry {
        token,
        expires_at: Instant::now() + ttl,
    })
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    /// number 或数字字符串两种都收（ADR-Q4）；解析失败丢弃并告警，
    /// 缺失时由 `fetch_token` 兜底 7200。
    #[serde(default, deserialize_with = "deserialize_expires_in")]
    expires_in: Option<u64>,
    #[serde(default)]
    code: Option<serde_json::Value>,
    #[serde(default)]
    message: Option<serde_json::Value>,
}

fn deserialize_expires_in<'de, D>(de: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(de)?;
    let Some(value) = value else {
        return Ok(None);
    };
    let parsed = match &value {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    };
    if parsed.is_none() {
        tracing::warn!(value = %value, "expires_in has unexpected form; using default TTL");
    }
    Ok(parsed)
}

/// JSON 值转可读字符串：字符串去引号，其余按 JSON 字面量。
fn value_plain(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
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
    fn expires_in_accepts_number_and_numeric_string() {
        let number: TokenResponse =
            serde_json::from_str(r#"{"access_token":"t","expires_in":7200}"#).unwrap();
        assert_eq!(number.expires_in, Some(7200));
        let string: TokenResponse =
            serde_json::from_str(r#"{"access_token":"t","expires_in":"7200"}"#).unwrap();
        assert_eq!(string.expires_in, Some(7200));
        let padded: TokenResponse =
            serde_json::from_str(r#"{"access_token":"t","expires_in":" 3600 "}"#).unwrap();
        assert_eq!(padded.expires_in, Some(3600));
    }

    #[test]
    fn expires_in_unparseable_becomes_none() {
        let bad: TokenResponse =
            serde_json::from_str(r#"{"access_token":"t","expires_in":"soon"}"#).unwrap();
        assert_eq!(bad.expires_in, None);
        let missing: TokenResponse = serde_json::from_str(r#"{"access_token":"t"}"#).unwrap();
        assert_eq!(missing.expires_in, None);
    }

    #[tokio::test]
    async fn store_returns_cached_token_without_client() {
        // A store seeded directly in the fast path must answer without any
        // HTTP client at all (client is only needed on a cache miss).
        let store = TokenStore::new();
        let slot = store.slot_for("1020304050");
        slot.inner.lock().unwrap().current = Some(TokenEntry {
            token: "cached".into(),
            expires_at: Instant::now() + Duration::from_secs(7200),
        });
        let token = store
            .get_or_fetch(&reqwest::Client::new(), &bot(), Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(token, "cached");
    }

    #[tokio::test]
    async fn cooldown_serves_old_token_near_expiry() {
        let store = TokenStore::new();
        let slot = store.slot_for("1020304050");
        {
            let mut inner = slot.inner.lock().unwrap();
            inner.current = Some(TokenEntry {
                token: "old".into(),
                expires_at: Instant::now() + Duration::from_secs(60),
            });
            inner.last_fetch = Some(Instant::now());
        }
        // Inside the cooldown the store must not even attempt a request:
        // a client pointed at a dead port with a 50 ms budget must not be
        // reachable, and the old token arrives regardless.
        let mut bot = bot();
        bot.api_base = Some("http://127.0.0.1:1".into());
        let token = store
            .get_or_fetch(&reqwest::Client::new(), &bot, Duration::from_millis(50))
            .await
            .unwrap();
        assert_eq!(token, "old");
    }

    #[tokio::test]
    async fn expired_token_is_not_served() {
        let store = TokenStore::new();
        let slot = store.slot_for("1020304050");
        slot.inner.lock().unwrap().current = Some(TokenEntry {
            token: "stale".into(),
            expires_at: Instant::now() - Duration::from_secs(1),
        });
        // Loopback refusal takes ~2 s on the dev machine; a 50 ms budget
        // turns any transport outcome into a failure before that matters.
        let mut bot = bot();
        bot.api_base = Some("http://127.0.0.1:1".into());
        let err = store
            .get_or_fetch(&reqwest::Client::new(), &bot, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::TokenFetchFailed(_)));
    }

    #[test]
    fn invalidate_clears_cached_token() {
        let store = TokenStore::new();
        let slot = store.slot_for("1020304050");
        slot.inner.lock().unwrap().current = Some(TokenEntry {
            token: "cached".into(),
            expires_at: Instant::now() + Duration::from_secs(7200),
        });
        store.invalidate("1020304050");
        assert!(fresh_token(&slot).is_none());
        // Unknown app_id is a no-op.
        store.invalidate("nope");
    }

    #[test]
    fn slots_keyed_by_app_id_not_config_id() {
        let store = TokenStore::new();
        let a = store.slot_for("1020304050");
        let b = store.slot_for("1020304050");
        let other = store.slot_for("999");
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &other));
    }
}
