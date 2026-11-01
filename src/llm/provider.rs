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
    pub protocol: Protocol,
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

/// POST body：`id`/`name` 可省略；`protocol`/`base_url`/`api_key` 必填；
/// 限额省略 = 不限；`enabled` 省略 = true。
#[derive(Debug, Deserialize)]
pub struct ProviderInput {
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    pub protocol: Protocol,
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
    #[serde(default)]
    pub protocol: Option<Protocol>,
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
            protocol: patch.protocol.unwrap_or(self.protocol),
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
            protocol: input.protocol,
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

    /// `enabled && protocol` 匹配，表内顺序——转发热路径的入口。
    pub fn list_enabled_by_protocol(&self, protocol: Protocol) -> Vec<Provider> {
        self.providers
            .read()
            .unwrap()
            .iter()
            .filter(|p| p.enabled && p.protocol == protocol)
            .cloned()
            .collect()
    }
}
