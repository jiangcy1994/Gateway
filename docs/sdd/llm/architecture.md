# 架构（LLM 子网关）

> **SDD 第 2 步**：模块划分与请求流程。只使用第 1 步（`overview.md`）定义过的概念。

## 模块划分

LLM 子网关是 top-gateway 内嵌模块，代码在 `src/llm/`，与既有模块的集成点只有三个：`state.rs`（共享状态）、`config.rs`（持久化）、`lib.rs`（路由组装）。

| 模块 | 职责 | 依赖 |
|---|---|---|
| `llm/provider.rs` | `Protocol` 枚举、`infer_protocol`（v1 端点闭表推断）、`Model` 值对象、`Provider` 值对象（`protocols` 多协议、`models` 模型表、`timeout_secs`）、`ProviderPatch`、`ProviderTable`（与 `router.rs` 的 `RouteTable` 同构） | `error.rs` |
| `llm/app.rs` | `App` 值对象（应用凭证 → 模型组合）、`AppInput`/`AppPatch`、`AppTable`（与 `ProviderTable` 同构）；key 比较用常量时间 | `error.rs`（`constant_time_eq` 复用自 `admin.rs`） |
| `llm/quota.rs` | `QuotaTracker`：供应商级 60s 滚动窗口的 RPM/TPM 计数与剩余量 | 无（纯内存 + `Mutex`） |
| `llm/balancer.rs` | `select_and_record`：按协议 + 模型过滤候选 → **优先级分层** → 层内加权随机 → **同临界区记 RPM**，返回 `Selection { provider, upstream_model }` | `provider.rs`、`quota.rs`、`state.rs` |
| `llm/usage.rs` | `TailBuffer`（尾部缓冲）+ `extract_tokens`：从响应尾包按协议提取 token 数 | `provider.rs`（`Protocol`） |
| `llm/forward.rs` | `/llm/*` 转发处理器：解析两种入站形态、app 解析、请求 body 缓冲与 `model` 提取 / 改写、按供应商超时、鉴权头注入、流式转发、usage 记账 tap | 以上全部 + `proxy.rs` 的 `error_response` |
| `llm/admin.rs` | `/__admin/llm/*` 端点（供应商 CRUD / 应用 CRUD / 配额 / 指标），复用 `require_admin` | `provider.rs`、`app.rs`、`state.rs`、`metrics.rs` |

## 请求流程

### 客户端请求（`/llm/...`）

```
build_app 把 llm::llm_router() nest 在 "/llm"
  │  其余路径照旧落到 proxy fallback（routes.toml 前缀转发）
  ▼
forward handler（llm/forward.rs）
 1. 解 path：剥 "/llm"，按首段分两种入站形态（ADR-L12）：
      • 显式协议段 /llm/{protocol}/rest —— 首段非 v1
        首段空 / 非已知协议 → 404；剩余路径原样转发
      • v1 推断 /llm/v1/{端点}/rest —— 首段为 v1（大小写不敏感）
        端点首段经 infer_protocol 闭表判定：messages/complete → anthropic，
        chat/completions/... → openai；models/files（两协议都有）→ 404 提示
        显式段；未知端点 → 404；v1 后无端点 → 404
        推断只决定选池与凭证注入，转发路径恒保留客户端原写法（/v1/...）
 2. 解 app（ADR-L15）：Authorization: Bearer <key> 或 x-api-key: <key>（常量时间比较）
      • 无凭证 → app = None（默认策略：全部已映射模型）
      • 未命中任何 app → 401 UnknownAppKey（fail-closed）
      • 命中但 enabled=false → 403 AppDisabled
 3. 缓冲请求 body（上限 MAX_REQUEST_BODY = 16 MiB，ADR-L14）：
      超限 → 413；正常 → 解析 JSON 取 "model" 字符串字段，
      非 JSON / 无 model 字段 → model = None（组合校验盲区，风险 R11）
 4. 组合校验：app 已解析且其 models 非空且 model 存在但不在组合内
      → 403 ModelNotAllowedForApp
 5. balancer::select_and_record(state, protocol, model)（见下，返回 Selection）
      ├─ 候选 = enabled && protocols 含该协议 && (model 为 None 或模型表为空或声明了该 model)
      ├─ 候选为空时：model 存在且全表无人声明该 model → 404 ModelNotFound；
        否则（无 model 请求 / 模型有声明但池空）→ 503
      ├─ 逐供应商取 QuotaTracker（缺则建，按 provider id 存），按表序加锁（防死锁）
      ├─ 优先级分层（model 存在时，ADR-L16）：effective priority = 声明条目的
        priority（空模型表兜底供应商 = i32::MIN）；从高到低取第一个存在
        weight>0 成员的层，层内 min(rpm_ratio, tpm_ratio) 加权随机；
        层全耗尽降层；所有层全 0 → 429 QuotaExhausted
      └─ 同一锁内记一次 RPM → 返回 Selection { provider, upstream_model }
 6. stats.guard()：RouteStats in-flight / 延迟 / 计数（RAII）
 7. 上游 URL = base_url.trim_end('/') + 剩余路径 + 原查询串
 8. 请求体改写（ADR-L14）：upstream_model 与请求 model 不同 → 重新序列化 JSON
      （model 字段替换为 upstream 名）；相同 → 原始 bytes 零差异透传。
      无 model 请求 / 兜底供应商（upstream_model = None）→ 原样转发
 9. 请求头：剥 hop-by-hop / content-length / host / authorization / x-api-key；
      按协议注入供应商凭证（openai|systemone → Bearer；anthropic → x-api-key，
      anthropic-version 缺省补 "2023-06-01"，客户端已传则保留）
10. timeout 包裹 send()：时长 = provider.timeout_secs，缺省回落
      settings.default_timeout_secs（ADR-L17）；传输错误 → 502；超时 → 504（均 record_error）
11. 响应头：剥 hop-by-hop / content-length；附 x-llm-provider: <id>
12. 响应 body：Body::from_stream(usage tap)
      └─ tap 每块复制进 TailBuffer（保留最后 64KiB），
         流结束 / 出错 / 消费者断开时 extract_tokens → quota.record_tokens
13. 上游 5xx → record_error 后原样透传
```

权重（第 5 步）：对每个有限额维度取剩余比例，取两者最小值；未设限额的维度按 1.0。分层只在请求带 `model` 时生效；无 `model` 请求（含兜底场景）全员 effective priority = 0，即退化为此前的扁平加权随机（ADR-L4 语义保留）。

### 管理请求（`/__admin/llm/*`）

挂载在既有 `admin_router()` 内（`nest("/llm", llm::admin_router())`），鉴权复用 `require_admin`（同一下 `X-Admin-Key` / `Bearer` 密钥）。供应商变更端点成功后调 `state.persist_providers()`，应用变更端点调 `state.persist_apps()`——两者与路由持久化同构（同目录临时文件 + rename，托管头注释，重写丢注释）。

## 与 top-gateway 既有行为的关系

- `/llm` 前缀**保留**：`nest` 优先于 fallback，即使 `routes.toml` 挂了同名前缀也不会被代理转发（ADR-L5）。
- `Protocol`、`Provider`、`App` 是三个独立配置面，不与 `Route` 混用：`routes.toml` 管「前缀 → 上游」、`providers.toml` 管「协议 + 模型 → 供应商池」、`apps.toml` 管「应用凭证 → 模型组合」（ADR-L18）。
- LLM 指标复用 `metrics::RouteStats`（按供应商 id 存在独立 map，与路由 stats 隔离，防 id 碰撞）。
- 健康检查不覆盖供应商：LLM 供应商的可用性由 502 状态码与 `/__admin/llm/metrics` 的错误数反映（ADR-L7 无失败转移的直接后果）。
