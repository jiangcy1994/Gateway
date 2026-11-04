use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;

/// LLM 协议段：`/llm/{protocol}/...`。`Systemone` 全程参与通用路径（ADR-L8），
/// 仅 `usage::extract_tokens` 未实现其 usage 解析（桩）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Openai,
    Anthropic,
    Systemone,
}

/// `/llm/v1/{端点}` 推断失败的原因（ADR-L12）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferError {
    /// 端点两协议族都提供（`models`、`files`），无法无歧义判定。
    Ambiguous,
    /// 端点不在推断闭表内。
    Unknown,
}

impl Protocol {
    /// URL 协议段匹配，大小写不敏感（`/llm/OpenAI` 归一化为小写后匹配）。
    pub fn from_url_segment(segment: &str) -> Option<Self> {
        match segment.to_ascii_lowercase().as_str() {
            "openai" => Some(Protocol::Openai),
            "anthropic" => Some(Protocol::Anthropic),
            "systemone" => Some(Protocol::Systemone),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::Openai => "openai",
            Protocol::Anthropic => "anthropic",
            Protocol::Systemone => "systemone",
        }
    }
}

/// `/llm/v1/{端点}` 的端点段 → 协议推断闭表（ADR-L12）。按 **v1 之后的首段**
/// 匹配，大小写不敏感；命中段后的剩余路径原样转发（`messages/count_tokens`
/// 归 `messages`）。
///
/// - `messages`、`complete` → anthropic
/// - `chat`、`completions`、`responses`、`embeddings`、`moderations`、`audio`、
///   `images`、`fine_tuning`、`batches`、`uploads`、`threads`、`assistants` → openai
/// - `models`、`files` 两协议族都有 → [`InferError::Ambiguous`]（404，要求显式协议段）
/// - 其余 → [`InferError::Unknown`]（404）
///
/// systemone 不参与推断：它只能经显式协议段 `/llm/systemone/...` 访问。
pub fn infer_protocol(endpoint: &str) -> Result<Protocol, InferError> {
    match endpoint.to_ascii_lowercase().as_str() {
        "messages" | "complete" => Ok(Protocol::Anthropic),
        "chat" | "completions" | "responses" | "embeddings" | "moderations" | "audio"
        | "images" | "fine_tuning" | "batches" | "uploads" | "threads" | "assistants" => {
            Ok(Protocol::Openai)
        }
        "models" | "files" => Err(InferError::Ambiguous),
        _ => Err(InferError::Unknown),
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一个 LLM 供应商。TOML 文件、管理 API、内存表三处共用同一表示
/// （与 `Route` 同构的共享表示原则）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Provider {
    /// 非空；表内唯一；`PUT` 不可改；POST 省略时自动生成 UUID v4。
    pub id: String,
    /// 展示名，默认空串。
    #[serde(default)]
    pub name: String,
    /// 该供应商说哪些协议（至少一个、不可重复；旧形态的单值
    /// `protocol = "openai"` 仍能读取，见 ADR-L13）。选择池 = 说该协议的
    /// 全部启用供应商；一个供应商可同时进多个协议池。
    #[serde(alias = "protocol", deserialize_with = "deserialize_protocols")]
    pub protocols: Vec<Protocol>,
    /// `http://` 或 `https://` 且含 host；不以 `/` 结尾（校验时 trim）。
    pub base_url: String,
    /// 非空。网关按协议注入到上游请求，客户端的凭证一律被替换。
    pub api_key: String,
    /// `Some(n)`（`n > 0`）= 每分钟请求数限额；`None` = 不限。
    #[serde(default)]
    pub rpm_limit: Option<u64>,
    /// `Some(n)`（`n > 0`）= 每分钟 token 数限额；`None` = 不限。
    #[serde(default)]
    pub tpm_limit: Option<u64>,
    /// `false` 时该供应商被选择排除（协议池全禁 → 503）。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// 兼容旧形态：单值 `protocol = "openai"` 与列表 `protocols = ["openai"]` 都能
/// 读进来；序列化只产出列表形态（字段重命名后，旧 TOML 也不该启动失败）。
fn deserialize_protocols<'de, D>(deserializer: D) -> Result<Vec<Protocol>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ProtocolsVisitor;

    impl<'de> serde::de::Visitor<'de> for ProtocolsVisitor {
        type Value = Vec<Protocol>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "an LLM protocol or a list of LLM protocols")
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Protocol::from_url_segment(v)
                .map(|p| vec![p])
                .ok_or_else(|| {
                    E::custom(format!(
                        "unknown protocol '{v}': expected openai, anthropic or systemone"
                    ))
                })
        }

        fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
            self.visit_str(&v)
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(protocol) = seq.next_element::<Protocol>()? {
                out.push(protocol);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_any(ProtocolsVisitor)
}

/// 与 [`deserialize_protocols`] 同样的单值/列表兼容，再包一层 Option 表达
/// PUT 的"缺省 = 保留"。显式 `null` 与缺省同义（保留原值）。
fn deserialize_protocols_opt<'de, D>(deserializer: D) -> Result<Option<Vec<Protocol>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptProtocolsVisitor;

    impl<'de> serde::de::Visitor<'de> for OptProtocolsVisitor {
        type Value = Option<Vec<Protocol>>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "null, an LLM protocol, or a list of LLM protocols")
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Protocol::from_url_segment(v)
                .map(|p| Some(vec![p]))
                .ok_or_else(|| {
                    E::custom(format!(
                        "unknown protocol '{v}': expected openai, anthropic or systemone"
                    ))
                })
        }

        fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
            self.visit_str(&v)
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(protocol) = seq.next_element::<Protocol>()? {
                out.push(protocol);
            }
            Ok(Some(out))
        }
    }

    deserializer.deserialize_any(OptProtocolsVisitor)
}

/// POST body：`id`/`name` 可省略；`protocols`/`base_url`/`api_key` 必填；
/// 限额省略 = 不限；`enabled` 省略 = true。
#[derive(Debug, Deserialize)]
pub struct ProviderInput {
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(alias = "protocol", deserialize_with = "deserialize_protocols")]
    pub protocols: Vec<Protocol>,
    pub base_url: String,
    pub api_key: String,
    #[serde(default)]
    pub rpm_limit: Option<u64>,
    #[serde(default)]
    pub tpm_limit: Option<u64>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// PUT body：全 Option 合并语义。`rpm_limit`/`tpm_limit` 为 `Option<Option<u64>>`——
/// 字段缺省 = 保留原值；显式 `null` = 设为不限；数字 = 设新限额。
#[derive(Debug, Default, Deserialize)]
pub struct ProviderPatch {
    #[serde(default)]
    pub name: Option<String>,
    /// 缺省（含显式 `null`）= 保留原值；给列表 = 整体替换。
    #[serde(
        default,
        alias = "protocol",
        deserialize_with = "deserialize_protocols_opt"
    )]
    pub protocols: Option<Vec<Protocol>>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default, deserialize_with = "deserialize_some")]
    pub rpm_limit: Option<Option<u64>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    pub tpm_limit: Option<Option<u64>>,
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

impl Provider {
    /// 顺序即错误优先级，与 `interfaces.md` 契约一致。
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("id must not be empty".into());
        }
        if self.protocols.is_empty() {
            return Err("protocols must not be empty".into());
        }
        let mut known = Vec::new();
        for protocol in &self.protocols {
            if known.contains(protocol) {
                return Err(format!(
                    "protocols must not contain duplicate '{}'",
                    protocol
                ));
            }
            known.push(*protocol);
        }
        let mut scheme_host = self.base_url.split("://");
        let scheme = scheme_host.next().unwrap_or_default().to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https") {
            return Err(format!("base_url {} must use http or https", self.base_url));
        }
        let rest = scheme_host.next().unwrap_or_default();
        let host = rest.split(['/', '?']).next().unwrap_or_default();
        if host.trim().is_empty() {
            return Err(format!("base_url {} must have a host", self.base_url));
        }
        if self.api_key.trim().is_empty() {
            return Err("api_key must not be empty".into());
        }
        for (name, limit) in [("rpm_limit", self.rpm_limit), ("tpm_limit", self.tpm_limit)] {
            if let Some(n) = limit {
                if n == 0 {
                    return Err(format!("{name} must be greater than 0"));
                }
            }
        }
        Ok(())
    }

    /// `Some` 覆盖、`None` 保留；`id` 永远来自 `self`。不校验——
    /// 调用方 `replace` 前自行调 `validate`。
    pub fn apply_patch(&self, patch: ProviderPatch) -> Provider {
        Provider {
            id: self.id.clone(),
            name: patch.name.unwrap_or_else(|| self.name.clone()),
            protocols: patch.protocols.unwrap_or_else(|| self.protocols.clone()),
            base_url: patch.base_url.unwrap_or_else(|| self.base_url.clone()),
            api_key: patch.api_key.unwrap_or_else(|| self.api_key.clone()),
            rpm_limit: patch.rpm_limit.unwrap_or(self.rpm_limit),
            tpm_limit: patch.tpm_limit.unwrap_or(self.tpm_limit),
            enabled: patch.enabled.unwrap_or(self.enabled),
        }
    }
}

impl From<ProviderInput> for Provider {
    fn from(input: ProviderInput) -> Self {
        Provider {
            id: input
                .id
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            name: input.name.unwrap_or_default(),
            protocols: input.protocols,
            base_url: input.base_url,
            api_key: input.api_key,
            rpm_limit: input.rpm_limit,
            tpm_limit: input.tpm_limit,
            enabled: input.enabled.unwrap_or(true),
        }
    }
}

/// On-disk layout of the providers file: one `[[providers]]` table per provider.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProvidersFile {
    #[serde(default)]
    pub providers: Vec<Provider>,
}

/// 供应商线性表（与 `RouteTable` 同构）。
#[derive(Debug, Default, Clone)]
pub struct ProviderTable {
    providers: Arc<RwLock<Vec<Provider>>>,
}

impl ProviderTable {
    /// 整表替换（启动加载用）。
    pub fn set_all(&self, providers: Vec<Provider>) {
        *self.providers.write().unwrap() = providers;
    }

    /// 全量克隆，表内顺序。
    pub fn list(&self) -> Vec<Provider> {
        self.providers.read().unwrap().clone()
    }

    /// 按 id 查。
    pub fn get(&self, id: &str) -> Option<Provider> {
        self.providers
            .read()
            .unwrap()
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }

    /// 追加表尾；id 重复 → `DuplicateProviderId`（400）。
    pub fn add(&self, provider: Provider) -> Result<(), ApiError> {
        let mut table = self.providers.write().unwrap();
        if table.iter().any(|p| p.id == provider.id) {
            return Err(ApiError::DuplicateProviderId(provider.id));
        }
        table.push(provider);
        Ok(())
    }

    /// 原地替换；未知 id → 404；与其他供应商 id 冲突（排除自身位置）→ 400。
    pub fn replace(&self, id: &str, provider: Provider) -> Result<(), ApiError> {
        let mut table = self.providers.write().unwrap();
        let Some(pos) = table.iter().position(|p| p.id == id) else {
            return Err(ApiError::ProviderNotFound(id.to_string()));
        };
        if table
            .iter()
            .enumerate()
            .any(|(i, p)| i != pos && p.id == provider.id)
        {
            return Err(ApiError::DuplicateProviderId(provider.id));
        }
        table[pos] = provider;
        Ok(())
    }

    /// 移除并返回。
    pub fn remove(&self, id: &str) -> Option<Provider> {
        let mut table = self.providers.write().unwrap();
        let pos = table.iter().position(|p| p.id == id)?;
        Some(table.remove(pos))
    }

    /// `enabled && protocols 含该协议` 匹配，表内顺序——转发热路径的入口。
    /// 一个供应商说多个协议时就同时出现在多个协议池里（ADR-L13）。
    pub fn list_enabled_by_protocol(&self, protocol: Protocol) -> Vec<Provider> {
        self.providers
            .read()
            .unwrap()
            .iter()
            .filter(|p| p.enabled && p.protocols.contains(&protocol))
            .cloned()
            .collect()
    }
}
