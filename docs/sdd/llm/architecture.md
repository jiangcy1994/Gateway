# 架构（LLM 子网关）

> **SDD 第 2 步**：模块划分与请求流程。只使用第 1 步（`overview.md`）定义过的概念。

## 模块划分

LLM 子网关是 top-gateway 内嵌模块，代码在 `src/llm/`，与既有模块的集成点只有三个：`state.rs`（共享状态）、`config.rs`（持久化）、`lib.rs`（路由组装）。

| 模块 | 职责 | 依赖 |
|---|---|---|
| `llm/provider.rs` | `Protocol` 枚举、`Provider` 值对象、`ProviderPatch`、`ProviderTable`（与 `router.rs` 的 `RouteTable` 同构） | `error.rs` |
| `llm/quota.rs` | `QuotaTracker`：供应商级 60s 滚动窗口的 RPM/TPM 计数与剩余量 | 无（纯内存 + `Mutex`） |
| `llm/balancer.rs` | `select_and_record`：按协议过滤供应商 → 加权随机 → **同临界区记 RPM** | `provider.rs`、`quota.rs`、`state.rs` |
| `llm/usage.rs` | `TailBuffer`（尾部缓冲）+ `extract_tokens`：从响应尾包按协议提取 token 数 | `provider.rs`（`Protocol`） |
| `llm/forward.rs` | `/llm/{protocol}/*` 转发处理器：鉴权头注入、流式转发、usage 记账 tap | 以上全部 + `proxy.rs` 的 `error_response` |
| `llm/admin.rs` | `/__admin/llm/*` 端点（供应商 CRUD / 配额 / 指标），复用 `require_admin` | `provider.rs`、`state.rs`、`metrics.rs` |

## 请求流程

### 客户端请求（`/llm/{protocol}/...`）

```
build_app 把 llm::llm_router() nest 在 "/llm"
  │  其余路径照旧落到 proxy fallback（routes.toml 前缀转发）
  ▼
forward handler（llm/forward.rs）
 1. 解 path：剥 "/llm"，取首段为协议；未知/缺失 → 404
 2. state.providers.list_enabled_by_protocol(protocol) 为空 → 503
 3. balancer::select_and_record(state, protocol)
      ├─ 逐供应商取 QuotaTracker（缺则建），按表序加锁（防死锁）
      ├─ 淘汰窗口外旧记录 → 算各供应商权重（见下）→ 全 0 → 429
      └─ 加权随机选中 → 同一锁内记一次 RPM → 返回 Provider（克隆）
 4. stats.guard()：RouteStats in-flight / 延迟 / 计数（RAII）
 5. 上游 URL = base_url.trim_end('/') + 剩余路径 + 原查询串
 6. 请求头：剥 hop-by-hop / content-length / host / authorization / x-api-key；
      按协议注入供应商凭证（openai|systemone → Bearer；anthropic → x-api-key，
      anthropic-version 缺省补 "2023-06-01"，客户端已传则保留）
 7. timeout 包裹 send()：传输错误 → 502；超时 → 504（均 record_error）
 8. 响应头：剥 hop-by-hop / content-length；附 x-llm-provider: <id>
 9. 响应 body：Body::from_stream(usage tap)
      └─ tap 每块复制进 TailBuffer（保留最后 64KiB），
         流结束 / 出错 / 消费者断开时 extract_tokens → quota.record_tokens
10. 上游 5xx → record_error 后原样透传
```

权重（第 3 步）：对每个有限额维度取剩余比例，取两者最小值；未设限额的维度按 1.0。

### 管理请求（`/__admin/llm/*`）

挂载在既有 `admin_router()` 内（`nest("/llm", llm::admin_router())`），鉴权复用 `require_admin`（同一下 `X-Admin-Key` / `Bearer` 密钥）。变更端点成功后调 `state.persist_providers()`——与路由持久化同构（同目录临时文件 + rename，托管头注释，重写丢注释）。

## 与 top-gateway 既有行为的关系

- `/llm` 前缀**保留**：`nest` 优先于 fallback，即使 `routes.toml` 挂了同名前缀也不会被代理转发（ADR-L5）。
- `Protocol`、`Provider` 是独立配置面，不与 `Route` 混用：`routes.toml` 管「前缀 → 上游」，`providers.toml` 管「协议 → 供应商池」。
- LLM 指标复用 `metrics::RouteStats`（按供应商 id 存在独立 map，与路由 stats 隔离，防 id 碰撞）。
- 健康检查不覆盖供应商：LLM 供应商的可用性由 502 状态码与 `/__admin/llm/metrics` 的错误数反映（ADR-L7 无失败转移的直接后果）。
