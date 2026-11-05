use std::sync::{Arc, RwLock};

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::admin::constant_time_eq;
use crate::error::ApiError;

/// 自动生成的 app key 前缀（`sk-gw-<uuid v4 simple>`，ADR-L15）。
const GENERATED_KEY_PREFIX: &str = "sk-gw-";

/// 一个本地 app：入站凭证 → 可用模型组合（ADR-L15）。`key` 是客户端填在
/// SDK 原本填供应商 key 位置的那个值；`models` 是**网关侧**模型名（provider
/// 声明的 `name`），空表 = 不限制。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct App {
    /// 非空；表内唯一；`PUT` 不可改；POST 省略时自动生成 UUID v4。
    pub id: String,
    /// 展示名，默认空串。
    #[serde(default)]
    pub name: String,
    /// 非空；表内唯一；POST 省略时自动生成 `sk-gw-<uuid v4 simple>`。
    pub key: String,
    /// 网关侧模型名列表；空表 = 不限制（默认策略仅由「无凭证」触发）。
    #[serde(default)]
    pub models: Vec<String>,
    /// `false` 时该 app 的凭证被拒（403）。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl App {
    /// 顺序即错误优先级，与 `interfaces.md` 契约一致。
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("id must not be empty".into());
        }
        if self.key.trim().is_empty() {
            return Err("key must not be empty".into());
        }
        Ok(())
    }

    /// `Some` 覆盖、`None` 保留；`id`/`key` 永远来自 `self`（不可改）。
    /// 不校验——调用方 `replace` 前自行调 `validate`。
    pub fn apply_patch(&self, patch: AppPatch) -> App {
        App {
            id: self.id.clone(),
            key: self.key.clone(),
            name: patch.name.unwrap_or_else(|| self.name.clone()),
            models: patch.models.unwrap_or_else(|| self.models.clone()),
            enabled: patch.enabled.unwrap_or(self.enabled),
        }
    }
}

/// POST body：全字段可省略——`{}` 即创建一个启用、空组合、自动生成 id/key
/// 的 app。`models` 省略 = 空表（不限制）。
#[derive(Debug, Default, Deserialize)]
pub struct AppInput {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// PUT body：全 Option 合并语义。`models` 为 `Option<Vec<String>>`——
/// 缺省（含显式 `null`）= 保留原值；给列表 = 整体替换。空列表 = 清空组合
/// （= 不限制），与 `null` 语义不同，这一点与 providers 的限额字段不同。
#[derive(Debug, Default, Deserialize)]
pub struct AppPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub models: Option<Vec<String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl From<AppInput> for App {
    fn from(input: AppInput) -> Self {
        // 省略（None）= 自动生成；显式空串 / 纯空白 = 非法，留给 `validate`
        // 判 400（与 `Provider` 的「空 id → 400」一致，也让 `InvalidApp` 可达）。
        let id = match input.id {
            Some(s) => s.trim().to_string(),
            None => Uuid::new_v4().to_string(),
        };
        let key = match input.key {
            Some(s) => s.trim().to_string(),
            None => format!("{GENERATED_KEY_PREFIX}{}", Uuid::new_v4().simple()),
        };
        App {
            id,
            name: input.name.unwrap_or_default(),
            key,
            models: input.models,
            enabled: input.enabled.unwrap_or(true),
        }
    }
}

/// On-disk layout of the apps file: one `[[apps]]` table per app.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppsFile {
    #[serde(default)]
    pub apps: Vec<App>,
}

/// app 线性表（与 `ProviderTable` 同构）。
#[derive(Debug, Default, Clone)]
pub struct AppTable {
    apps: Arc<RwLock<Vec<App>>>,
}

impl AppTable {
    /// 整表替换（启动加载用）。
    pub fn set_all(&self, apps: Vec<App>) {
        *self.apps.write().unwrap() = apps;
    }

    /// 全量克隆，表内顺序。
    pub fn list(&self) -> Vec<App> {
        self.apps.read().unwrap().clone()
    }

    /// 按 id 查。
    pub fn get(&self, id: &str) -> Option<App> {
        self.apps
            .read()
            .unwrap()
            .iter()
            .find(|a| a.id == id)
            .cloned()
    }

    /// 按入站凭证查 app（**常量时间**比较，ADR-L15）。命中即返回；找不到
    /// 返回 `None`，由调用方决定 401（fail-closed）。
    pub fn find_by_key(&self, key: &str) -> Option<App> {
        self.apps
            .read()
            .unwrap()
            .iter()
            .find(|app| constant_time_eq(key.as_bytes(), app.key.as_bytes()))
            .cloned()
    }

    /// 追加表尾；id 或 key 重复 → 400。
    pub fn add(&self, app: App) -> Result<(), ApiError> {
        let mut table = self.apps.write().unwrap();
        if table.iter().any(|a| a.id == app.id) {
            return Err(ApiError::DuplicateAppId(app.id));
        }
        if table.iter().any(|a| a.key == app.key) {
            // 错误消息只回 app id，不回 key 本身（key 即凭证）。
            return Err(ApiError::DuplicateAppKey(app.id));
        }
        table.push(app);
        Ok(())
    }

    /// 原地替换；未知 id → 404；与其他 app 的 id 或 key 冲突（排除自身
    /// 位置）→ 400。
    pub fn replace(&self, id: &str, app: App) -> Result<(), ApiError> {
        let mut table = self.apps.write().unwrap();
        let Some(pos) = table.iter().position(|a| a.id == id) else {
            return Err(ApiError::AppNotFound(id.to_string()));
        };
        if table
            .iter()
            .enumerate()
            .any(|(i, a)| i != pos && a.id == app.id)
        {
            return Err(ApiError::DuplicateAppId(app.id));
        }
        if table
            .iter()
            .enumerate()
            .any(|(i, a)| i != pos && a.key == app.key)
        {
            return Err(ApiError::DuplicateAppKey(app.id));
        }
        table[pos] = app;
        Ok(())
    }

    /// 移除并返回。
    pub fn remove(&self, id: &str) -> Option<App> {
        let mut table = self.apps.write().unwrap();
        let pos = table.iter().position(|a| a.id == id)?;
        Some(table.remove(pos))
    }
}

/// 从请求头解析 app 凭证（ADR-L15）：`Authorization: Bearer <key>` 优先，
/// 回落 `x-api-key`；两者都在时 `Authorization` 赢；**解析失败一律视为无
/// 凭证**（不是错误凭证）——格式错的头不触发 401，按默认策略放行。
pub fn credential_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|text| {
            // `Bearer` 大小写不敏感（RFC 7235 auth-scheme）；token 保留原大小写。
            let mut parts = text.splitn(2, ' ');
            let scheme = parts.next().unwrap_or_default().trim();
            let token = parts.next().unwrap_or_default().trim();
            scheme
                .eq_ignore_ascii_case("bearer")
                .then_some(token.to_string())
                .filter(|token| !token.is_empty())
        })
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
        })
}
