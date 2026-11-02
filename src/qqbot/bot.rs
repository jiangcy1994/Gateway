use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;

/// 平台 API base（`sandbox` 在两者间切换，可被逐机器人 `api_base` 覆盖）。
pub const API_BASE_PRODUCTION: &str = "https://api.bot.qq.com";
pub const API_BASE_SANDBOX: &str = "https://sandbox.api.bot.qq.com";
/// WebSocket 接入点。沙箱值为按 HTTP base 的推断（风险 R10），可被
/// 逐机器人 `ws_url` 覆盖。
pub const WS_URL_PRODUCTION: &str = "wss://api.bot.qq.com/websocket/";
pub const WS_URL_SANDBOX: &str = "wss://sandbox.api.bot.qq.com/websocket/";

/// `intents` 默认值：`GROUP_AND_C2C_EVENT | INTERACTION`（群/私聊 + 互动）。
pub const DEFAULT_INTENTS: u32 = (1 << 25) | (1 << 26);

/// Identify 的 `properties` 字段（平台要求，值无业务含义），WS 客户端构造
/// Identify 包时反序列化进 `serde_json::Value`。
pub const WS_IDENTIFY_PROPERTIES: &str =
    r#"{"$os":"top-gateway","$browser":"top-gateway","$device":"top-gateway"}"#;

/// 事件通道（不变式 Q0）。`Webhook` 时平台往 `/qqbot/callback/{app_id}` 推、
/// 网关被动等；`Websocket` 时网关后台连 wss。回调端点始终存在（`Webhook`
/// 形态才真正会被平台调用），但只有 `Websocket` 形态 spawn supervisor。
/// 网关不校验 `event_source` 与平台权限组合——配错形态由平台用 op 9 /
/// 关闭码 4014 拒绝（ADR-Q10）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventSource {
    #[default]
    Webhook,
    Websocket,
}

impl EventSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventSource::Webhook => "webhook",
            EventSource::Websocket => "websocket",
        }
    }
}

impl std::fmt::Display for EventSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一个 QQ 机器人。TOML 文件、管理 API、内存表三处共用同一表示
/// （与 `Route` / `Provider` 同构的共享表示原则）。不变式 Q1–Q7 见
/// `docs/sdd/qqbot/data-model.md`。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Bot {
    /// 非空；表内唯一；`PUT` 不可改；POST 省略时自动生成 UUID v4
    /// （配置 id，与平台无关）。
    pub id: String,
    /// 展示名，默认空串。
    #[serde(default)]
    pub name: String,
    /// 非空；表内唯一（平台身份；`/qqbot/{app_id}/...` 与
    /// `/qqbot/callback/{app_id}` 用它路由）。
    pub app_id: String,
    /// 非空（换 access_token 的 clientSecret，也是 ed25519 签名种子来源）。
    pub app_secret: String,
    /// 默认 `false`；选择内置 base（正式 / 沙箱）。
    #[serde(default)]
    pub sandbox: bool,
    /// 覆盖 OpenAPI base；`Some` 时须为 `http(s)://host` 形式。
    #[serde(default)]
    pub api_base: Option<String>,
    /// 覆盖 WSS 接入点；`Some` 时须为 `ws(s)://host[:port][/path]` 形式。
    #[serde(default)]
    pub ws_url: Option<String>,
    /// 默认 `DEFAULT_INTENTS`；`0` 非法（至少订阅一类事件）。
    #[serde(default = "default_intents")]
    pub intents: u32,
    /// 默认 `[0, 1]`；必须 `shard[1] > 0` 且 `shard[0] < shard[1]`
    /// （`[id, total]`，无分片场景填 `[0,1]`）。
    #[serde(default = "default_shard")]
    pub shard: [u32; 2],
    /// 默认 `Webhook`。
    #[serde(default)]
    pub event_source: EventSource,
    /// 事件投递目标；`Some` 时须为 `http(s)://host` 形式；`None` 时事件
    /// 计 stats 后丢弃。
    #[serde(default)]
    pub event_url: Option<String>,
    /// 投递时附 `Authorization: Bearer <event_secret>`（空串等同不设）。
    #[serde(default)]
    pub event_secret: Option<String>,
    /// `false` 时转发返回 503、回调返回 404、不 spawn WS。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

fn default_intents() -> u32 {
    DEFAULT_INTENTS
}

fn default_shard() -> [u32; 2] {
    [0, 1]
}

/// POST body：`id`/`name` 可省略；`app_id`/`app_secret` 必填（缺 → 422，
/// 校验规则失败 → 400）；其余字段省略 = 各自默认。
#[derive(Debug, Deserialize)]
pub struct BotInput {
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    pub app_id: String,
    pub app_secret: String,
    #[serde(default)]
    pub sandbox: Option<bool>,
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default)]
    pub ws_url: Option<String>,
    #[serde(default)]
    pub intents: Option<u32>,
    #[serde(default)]
    pub shard: Option<[u32; 2]>,
    #[serde(default)]
    pub event_source: Option<EventSource>,
    #[serde(default)]
    pub event_url: Option<String>,
    #[serde(default)]
    pub event_secret: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// PUT body：全 Option 合并语义（与 `ProviderPatch` 同构）。URL 覆盖字段为
/// `Option<Option<String>>`——字段缺省 = 保留原值；显式 `null` = 清除覆盖
/// （用平台默认 / 不投递）；字符串 = 设新值（空串同 null，见 `Bot::normalize`）。
/// `id` 不可改。
#[derive(Debug, Default, Deserialize)]
pub struct BotPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub app_secret: Option<String>,
    #[serde(default)]
    pub sandbox: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_some")]
    pub api_base: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    pub ws_url: Option<Option<String>>,
    #[serde(default)]
    pub intents: Option<u32>,
    #[serde(default)]
    pub shard: Option<[u32; 2]>,
    #[serde(default)]
    pub event_source: Option<EventSource>,
    #[serde(default, deserialize_with = "deserialize_some")]
    pub event_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    pub event_secret: Option<Option<String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// serde 内置行为把 `null` 与缺省都映射成 `None`；包一层后 `null` → `Some(None)`，
/// 缺省（走 `#[serde(default)]`）仍是 `None`——合并语义才分得开。
fn deserialize_some<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

impl Bot {
    /// 顺序即错误优先级，与 `interfaces.md` 契约一致。
    pub fn validate(&self) -> Result<(), String> {
        if self.app_id.trim().is_empty() {
            return Err("app_id must not be empty".into());
        }
        if self.app_secret.trim().is_empty() {
            return Err("app_secret must not be empty".into());
        }
        if self.intents == 0 {
            return Err("intents must not be zero (subscribe to at least one event type)".into());
        }
        if self.shard[1] == 0 {
            return Err("shard total must be greater than 0".into());
        }
        if self.shard[0] >= self.shard[1] {
            return Err(format!(
                "shard id {} must be less than total {}",
                self.shard[0], self.shard[1]
            ));
        }
        if let Some(msg) = check_url(self.api_base.as_deref(), &["http", "https"], "api_base") {
            return Err(msg);
        }
        if let Some(msg) = check_url(self.event_url.as_deref(), &["http", "https"], "event_url") {
            return Err(msg);
        }
        if let Some(msg) = check_url(self.ws_url.as_deref(), &["ws", "wss"], "ws_url") {
            return Err(msg);
        }
        Ok(())
    }

    /// 归一化：trim 展示字符串，URL 覆盖字段去空白后空串 → `None`
    /// （TOML round-trip 的两种形态都收敛到 `None`）。不 trim
    /// `app_id` / `app_secret`，避免破坏可能含边缘空白凭证。
    pub fn normalize(mut self) -> Self {
        self.id = self.id.trim().to_string();
        self.name = self.name.trim().to_string();
        self.api_base = opt_trim(self.api_base);
        self.ws_url = opt_trim(self.ws_url);
        self.event_url = opt_trim(self.event_url);
        self.event_secret = opt_trim(self.event_secret);
        self
    }

    /// 有效 OpenAPI base：覆盖值优先，否则按 `sandbox` 取平台常量。
    /// 不 trim 尾部 `/`——拼接由 `forward.rs` 完成。
    pub fn resolve_api_base(&self) -> String {
        match opt_trim(self.api_base.clone()) {
            Some(s) => s,
            None => (if self.sandbox {
                API_BASE_SANDBOX
            } else {
                API_BASE_PRODUCTION
            })
            .to_string(),
        }
    }

    /// 有效 WSS 接入点：覆盖值优先，否则按 `sandbox` 取平台常量（ADR-Q12）。
    pub fn resolve_ws_url(&self) -> String {
        match opt_trim(self.ws_url.clone()) {
            Some(s) => s,
            None => (if self.sandbox {
                WS_URL_SANDBOX
            } else {
                WS_URL_PRODUCTION
            })
            .to_string(),
        }
    }

    /// 是否需要后台 WS 连接（启动 / 管理变更时维护 supervisor 生命周期用）。
    pub fn ws_needs_connection(&self) -> bool {
        self.enabled && self.event_source == EventSource::Websocket
    }

    /// `Some` 覆盖、`None` 保留；`id` 永远来自 `self`；结果经过 `normalize`。
    /// 不校验——调用方 `replace` 前自行调 `validate`。
    pub fn apply_patch(&self, patch: BotPatch) -> Bot {
        Bot {
            id: self.id.clone(),
            name: patch.name.unwrap_or_else(|| self.name.clone()),
            app_id: patch.app_id.unwrap_or_else(|| self.app_id.clone()),
            app_secret: patch.app_secret.unwrap_or_else(|| self.app_secret.clone()),
            sandbox: patch.sandbox.unwrap_or(self.sandbox),
            api_base: patch.api_base.unwrap_or_else(|| self.api_base.clone()),
            ws_url: patch.ws_url.unwrap_or_else(|| self.ws_url.clone()),
            intents: patch.intents.unwrap_or(self.intents),
            shard: patch.shard.unwrap_or(self.shard),
            event_source: patch.event_source.unwrap_or(self.event_source),
            event_url: patch.event_url.unwrap_or_else(|| self.event_url.clone()),
            event_secret: patch
                .event_secret
                .unwrap_or_else(|| self.event_secret.clone()),
            enabled: patch.enabled.unwrap_or(self.enabled),
        }
        .normalize()
    }
}

impl From<BotInput> for Bot {
    fn from(input: BotInput) -> Self {
        Bot {
            id: input
                .id
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            name: input.name.unwrap_or_default(),
            app_id: input.app_id,
            app_secret: input.app_secret,
            sandbox: input.sandbox.unwrap_or(false),
            api_base: input.api_base,
            ws_url: input.ws_url,
            intents: input.intents.unwrap_or(DEFAULT_INTENTS),
            shard: input.shard.unwrap_or([0, 1]),
            event_source: input.event_source.unwrap_or_default(),
            event_url: input.event_url,
            event_secret: input.event_secret,
            enabled: input.enabled.unwrap_or(true),
        }
        .normalize()
    }
}

fn opt_trim(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// URL 形态检查（Q6）：空值（`None` / 空串 / 纯空白）视为未设 → 通过；
/// 非空时须为指定 scheme 且含 host。返回 `Some(原因)` 即失败。
fn check_url(url: Option<&str>, schemes: &[&str], field: &str) -> Option<String> {
    let url = url.map(str::trim).filter(|s| !s.is_empty())?;
    let mut parts = url.split("://");
    let scheme = parts.next().unwrap_or_default().to_ascii_lowercase();
    if !schemes.iter().any(|s| *s == scheme) {
        return Some(format!(
            "{field} {url} must use {} scheme",
            schemes.join(" or ")
        ));
    }
    let host = parts
        .next()
        .unwrap_or_default()
        .split(['/', '?'])
        .next()
        .unwrap_or_default();
    if host.trim().is_empty() {
        return Some(format!("{field} {url} must have a host"));
    }
    None
}

/// On-disk layout of the bots file: one `[[bots]]` table per bot.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BotsFile {
    #[serde(default)]
    pub bots: Vec<Bot>,
}

/// 机器人线性表（与 `ProviderTable` 同构）。热路径入口是 `find(app_id)`
/// （转发路径再自行检查 `enabled`：未找到 404、禁用 503，ADR-Q8）。
#[derive(Debug, Default, Clone)]
pub struct BotTable {
    bots: Arc<RwLock<Vec<Bot>>>,
}

impl BotTable {
    /// 整表替换（启动加载用）。
    pub fn set_all(&self, bots: Vec<Bot>) {
        *self.bots.write().unwrap() = bots;
    }

    /// 全量克隆，表内顺序。
    pub fn list(&self) -> Vec<Bot> {
        self.bots.read().unwrap().clone()
    }

    /// 按**配置 id** 查（管理端点用）。
    pub fn get(&self, id: &str) -> Option<Bot> {
        self.bots
            .read()
            .unwrap()
            .iter()
            .find(|b| b.id == id)
            .cloned()
    }

    /// 按**平台 app_id** 查（含 disabled——`enabled` 语义由调用方区分）。
    pub fn find(&self, app_id: &str) -> Option<Bot> {
        self.bots
            .read()
            .unwrap()
            .iter()
            .find(|b| b.app_id == app_id)
            .cloned()
    }

    /// 追加表尾；id 重复或 app_id 重复 → 400。
    pub fn add(&self, bot: Bot) -> Result<(), ApiError> {
        let mut table = self.bots.write().unwrap();
        if table.iter().any(|b| b.id == bot.id) {
            return Err(ApiError::DuplicateBotId(bot.id));
        }
        if table.iter().any(|b| b.app_id == bot.app_id) {
            return Err(ApiError::DuplicateBotAppId(bot.app_id));
        }
        table.push(bot);
        Ok(())
    }

    /// 原地替换；未知 id → 404；与其他机器人 id 或 app_id 冲突
    /// （排除自身位置）→ 400。
    pub fn replace(&self, id: &str, bot: Bot) -> Result<(), ApiError> {
        let mut table = self.bots.write().unwrap();
        let Some(pos) = table.iter().position(|b| b.id == id) else {
            return Err(ApiError::BotNotFound(id.to_string()));
        };
        for (i, b) in table.iter().enumerate() {
            if i == pos {
                continue;
            }
            if b.id == bot.id {
                return Err(ApiError::DuplicateBotId(bot.id.clone()));
            }
            if b.app_id == bot.app_id {
                return Err(ApiError::DuplicateBotAppId(bot.app_id.clone()));
            }
        }
        table[pos] = bot;
        Ok(())
    }

    /// 移除并返回。
    pub fn remove(&self, id: &str) -> Option<Bot> {
        let mut table = self.bots.write().unwrap();
        let pos = table.iter().position(|b| b.id == id)?;
        Some(table.remove(pos))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            event_source: EventSource::Webhook,
            event_url: Some("http://127.0.0.1:9000/qq/events".into()),
            event_secret: None,
            enabled: true,
        }
    }

    #[test]
    fn validate_accepts_minimal_bot() {
        assert!(bot().validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_credentials() {
        let mut b = bot();
        b.app_id = "  ".into();
        assert_eq!(b.validate().unwrap_err(), "app_id must not be empty");
        b.app_id = "1020304050".into();
        b.app_secret = "".into();
        assert_eq!(b.validate().unwrap_err(), "app_secret must not be empty");
    }

    #[test]
    fn validate_rejects_zero_intents() {
        let mut b = bot();
        b.intents = 0;
        assert!(b.validate().unwrap_err().contains("intents"));
    }

    #[test]
    fn validate_rejects_bad_shard() {
        let mut b = bot();
        b.shard = [0, 0];
        assert!(b.validate().unwrap_err().contains("total"));
        b.shard = [1, 1];
        assert!(b.validate().unwrap_err().contains("less than total"));
        b.shard = [4, 4];
        assert!(b.validate().is_err());
        b.shard = [3, 4];
        assert!(b.validate().is_ok());
    }

    #[test]
    fn validate_checks_url_schemes_and_hosts() {
        let mut b = bot();
        b.api_base = Some("ftp://example.com".into());
        assert!(b.validate().unwrap_err().contains("api_base"));
        b.api_base = Some("http://".into());
        assert!(b.validate().unwrap_err().contains("host"));
        b.api_base = Some("http://mock.internal".into());
        assert!(b.validate().is_ok());
        b.ws_url = Some("https://example.com/ws".into());
        assert!(b.validate().unwrap_err().contains("ws_url"));
        b.ws_url = Some("ws://example.com/ws".into());
        assert!(b.validate().is_ok());
        b.event_url = Some("gopher://x".into());
        assert!(b.validate().unwrap_err().contains("event_url"));
    }

    #[test]
    fn empty_url_strings_are_none_after_normalize() {
        let b = Bot {
            api_base: Some("  ".into()),
            ws_url: Some("".into()),
            event_url: Some("http://127.0.0.1:9000/".into()),
            event_secret: Some("".into()),
            ..bot()
        }
        .normalize();
        assert_eq!(b.api_base, None);
        assert_eq!(b.ws_url, None);
        assert_eq!(b.event_secret, None);
        assert_eq!(b.event_url.as_deref(), Some("http://127.0.0.1:9000/"));
    }

    #[test]
    fn resolve_prefers_overrides_then_sandbox() {
        let mut b = bot();
        assert_eq!(b.resolve_api_base(), API_BASE_PRODUCTION);
        assert_eq!(b.resolve_ws_url(), WS_URL_PRODUCTION);
        b.sandbox = true;
        assert_eq!(b.resolve_api_base(), API_BASE_SANDBOX);
        assert_eq!(b.resolve_ws_url(), WS_URL_SANDBOX);
        b.api_base = Some("http://mock.internal".into());
        b.ws_url = Some("ws://mock.internal/ws".into());
        assert_eq!(b.resolve_api_base(), "http://mock.internal");
        assert_eq!(b.resolve_ws_url(), "ws://mock.internal/ws");
    }

    #[test]
    fn apply_patch_merges_and_freezes_id() {
        let current = bot();
        let updated = current.apply_patch(BotPatch {
            name: Some("renamed".into()),
            sandbox: Some(true),
            ..Default::default()
        });
        // `id` comes from `self` even if the patch tried to carry one.
        assert_eq!(updated.id, "b1");
        assert_eq!(updated.name, "renamed");
        assert!(updated.sandbox);
        assert_eq!(updated.app_id, current.app_id);
        assert_eq!(updated.intents, current.intents);
    }

    #[test]
    fn apply_patch_clears_url_override_with_explicit_null() {
        let current = Bot {
            api_base: Some("http://mock.internal".into()),
            ..bot()
        };
        let updated = current.apply_patch(BotPatch {
            api_base: Some(None),
            ..Default::default()
        });
        assert_eq!(updated.api_base, None);
        assert_eq!(updated.resolve_api_base(), API_BASE_PRODUCTION);
        // Absent field keeps the override.
        let updated = current.apply_patch(BotPatch::default());
        assert_eq!(updated.api_base.as_deref(), Some("http://mock.internal"));
    }

    #[test]
    fn input_defaults_and_generated_id() {
        let b: Bot = BotInput {
            id: None,
            name: None,
            app_id: "1020304050".into(),
            app_secret: "secret".into(),
            sandbox: None,
            api_base: None,
            ws_url: None,
            intents: None,
            shard: None,
            event_source: None,
            event_url: None,
            event_secret: None,
            enabled: None,
        }
        .into();
        assert!(!b.id.is_empty());
        assert_eq!(b.intents, DEFAULT_INTENTS);
        assert_eq!(b.shard, [0, 1]);
        assert_eq!(b.event_source, EventSource::Webhook);
        assert!(b.enabled);
        assert!(b.validate().is_ok());
    }

    #[test]
    fn toml_roundtrip_keeps_configuration_and_option_semantics() {
        let file = BotsFile {
            bots: vec![
                bot(),
                Bot {
                    id: "b2".into(),
                    name: "".into(),
                    app_id: "999".into(),
                    app_secret: "s2".into(),
                    sandbox: true,
                    api_base: None,
                    ws_url: Some("ws://mock.internal/ws".into()),
                    intents: 1 << 9,
                    shard: [1, 4],
                    event_source: EventSource::Websocket,
                    event_url: None,
                    event_secret: Some("bearer".into()),
                    enabled: false,
                },
            ],
        };
        let text = toml::to_string_pretty(&file).unwrap();
        let back: BotsFile = toml::from_str(&text).unwrap();
        assert_eq!(back.bots.len(), 2);
        assert_eq!(back.bots[0].id, "b1");
        assert_eq!(
            back.bots[0].event_url.as_deref(),
            Some("http://127.0.0.1:9000/qq/events")
        );
        // Option::None round-trips as None whether the serializer skips the key
        // or writes an empty string (normalize on load converges both).
        let b2 = back.bots[1].clone().normalize();
        assert_eq!(b2.api_base, None);
        assert_eq!(b2.event_url, None);
        assert_eq!(b2.ws_url.as_deref(), Some("ws://mock.internal/ws"));
        assert_eq!(b2.event_source, EventSource::Websocket);
        assert!(!b2.enabled);
    }

    #[test]
    fn table_enforces_id_and_app_id_uniqueness() {
        let table = BotTable::default();
        table.add(bot()).unwrap();
        // Same id, different app_id.
        let dup_id = Bot {
            app_id: "111".into(),
            ..bot()
        };
        assert!(matches!(
            table.add(dup_id),
            Err(ApiError::DuplicateBotId(_))
        ));
        // Same app_id, different id.
        let dup_app = Bot {
            id: "other".into(),
            ..bot()
        };
        assert!(matches!(
            table.add(dup_app),
            Err(ApiError::DuplicateBotAppId(_))
        ));
        // Replace by app_id segment is not allowed: replace targets the config id.
        let moved = Bot {
            id: "b1".into(),
            app_id: "222".into(),
            ..bot()
        };
        table.replace("b1", moved).unwrap();
        assert_eq!(table.find("1020304050"), None);
        assert!(table.find("222").is_some());
        // Replace conflicting with another row fails.
        let other = Bot {
            id: "b3".into(),
            app_id: "333".into(),
            ..bot()
        };
        table.add(other).unwrap();
        let clash = Bot {
            id: "b1".into(),
            app_id: "333".into(),
            ..bot()
        };
        assert!(matches!(
            table.replace("b1", clash),
            Err(ApiError::DuplicateBotAppId(_))
        ));
        assert!(matches!(
            table.replace("nope", bot()),
            Err(ApiError::BotNotFound(_))
        ));
        assert_eq!(table.remove("b3").unwrap().app_id, "333");
        assert!(table.remove("b3").is_none());
    }

    #[test]
    fn ws_needs_connection_requires_enabled_and_websocket() {
        let b = bot();
        assert!(!b.ws_needs_connection());
        let b = Bot {
            event_source: EventSource::Websocket,
            ..bot()
        };
        assert!(b.ws_needs_connection());
        let b = Bot {
            event_source: EventSource::Websocket,
            enabled: false,
            ..bot()
        };
        assert!(!b.ws_needs_connection());
    }

    #[test]
    fn event_source_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&EventSource::Websocket).unwrap(),
            "\"websocket\""
        );
        let b: Bot = serde_json::from_str(&serde_json::to_string(&bot()).unwrap()).unwrap();
        assert_eq!(b.event_source, EventSource::Webhook);
    }
}
