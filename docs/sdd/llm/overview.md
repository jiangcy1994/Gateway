# 概述（LLM 子网关）

> **SDD 第 1 步**：系统是什么、边界在哪里。后续所有步骤以此为范围。
> 本文档是 [`../overview.md`](../overview.md)（top-gateway）的子系统规格，只描述 LLM 子网关；路由转发、健康、指标等通用行为以 top-gateway 文档为准。

## 一句话定义

LLM 子网关是**内嵌于 top-gateway 的 LLM API 聚合层**：客户端按 OpenAI / Anthropic / SystemOne 三种协议的原生路径发到 `/llm/{protocol}/...`，或让两家 SDK 直接指 `http://gw:8000/llm/v1` 由网关**按端点推断协议**；供应商声明**模型表**（网关模型名 ↔ 上游模型名 + 优先级）后，网关按**协议 + 模型**筛选候选，按 (provider, model) **优先级分层**（高优先级先用，其配额耗尽才降层，层内仍是剩余配额加权随机），并把请求 body 里的 `model` 字段改写为上游真实名；入站凭证（`Bearer` / `x-api-key`）同时是**应用凭证**，每个 app 可限制可用模型组合；配额（**供应商维度、跨其名下所有模型与协议汇总**）全部耗尽时返回 429。

## 它解决什么

一个 LLM 应用同时对接多家供应商（OpenAI 官方、Azure OpenAI、Anthropic、兼容 OpenAI 协议的中转、内部 SystemOne 网关……）时，每家各有 RPM/TPM 限额，且限额是**供应商级**的——同一家供应商下所有模型共享一个计数。应用侧自己实现「选哪家还能打」既要维护各家的限额，又要跨请求做分钟级窗口计数。

LLM 子网关把这件事收进网关：

```
client (OpenAI SDK, base_url=http://gw:8000/llm/v1, api_key=app key)
        │  POST /llm/v1/chat/completions      {"model":"chat-pro", ...}
        │  POST /llm/v1/messages              {"model":"claude...", ...}
        ▼
top-gateway:8000
  ├─ /llm/v1/{端点}  ──► 按端点闭表推断协议（chat→openai，messages→anthropic）
  ├─ /llm/openai/*   ──► 显式选 openai 协议供应商（按剩余 RPM/TPM 加权）
  ├─ /llm/anthropic/*──► 显式选 anthropic 协议供应商
  ├─ /llm/systemone/*──► 显式选 systemone 协议供应商（桩：见范围外）
  ├─ /__admin/llm/*  ──► 供应商 / 应用配置 / 配额查看 / 指标
  └─ 其余路径        ──► 原有 routes.toml 前缀转发（不受影响）

  请求处理：入站凭证 → app（无凭证=默认策略：全部已映射模型）
            → 读 body 取 model → app 组合校验
            → 候选池 = 协议匹配 && (模型表为空 || 声明了该 model)，按优先级分层选供应商
            → 选中条目的 upstream 名与请求 model 不同则改写 body 的 model 字段
            → 注入供应商凭证、按供应商选 client（proxy_url 缺省=共享直连，ADR-L19）、
              按供应商 timeout（缺省=全局）转发
```

两种入站形态并存（ADR-L12）：**SDK 直接指 `/llm/v1` 最省事**（OpenAI 与 Anthropic 的 SDK 都把 base_url 拼成 `{base}/v1/{端点}`）；需要无歧义或有歧义端点（`models`、`files` 两协议都有）时用**显式协议段** `/llm/{protocol}/...`。

## 范围内

- 按**协议透传**转发：入站路径剥掉 `/llm/{protocol}`（或 `/llm/v1/{端点}` 推断出协议后）原样拼到供应商 `base_url`，body 与响应流式透传，不做协议转换。
- 按协议过滤启用的供应商（`protocols` **包含**该协议即入池，一个供应商可在多个协议池里），**剩余配额加权随机**选择；同一临界区内完成「选择 + RPM 计数」，并发请求不会同时选中只剩 1 RPM 的供应商。配额按**供应商**记一份，跨协议共用。
- RPM 派遣即计数；TPM 从上游响应的 `usage` 字段**事后记账**（非流式 JSON 与 SSE 末尾 usage 均支持，best-effort）。
- 供应商级 60 秒滚动窗口；RPM/TPM 限额均可选（`None` = 不限量）。
- **模型映射**（ADR-L14）：供应商声明 `[[providers.models]]` 表（`name` = 网关模型名，`upstream` = 上游真实名，缺省同名；`priority` 越大越优先）。请求体带 `model` 时候选池收窄为「声明了该模型的供应商」，选中的条目 `upstream` 与请求值不同则**改写请求 body 的 `model` 字段**（值相同时原始 bytes 零差异透传）。**空模型表 = 协议级兜底**：参与任意 model 请求的选择、不改写（effective priority = `i32::MIN`，显式声明永远优先）。
- **优先级分层选择**（ADR-L16，修订 L4 于有模型时）：从高到低取第一个仍存在可选用成员的层，层内保持剩余配额加权随机；层全耗尽降层；所有层全耗尽 → `429`。
- **应用组合**（ADR-L15）：入站 `Authorization: Bearer <k>` 或 `x-api-key: <k>` 即应用凭证（客户端把 app key 填在 SDK 原本填供应商 key 的位置）。无凭证 → 默认策略 = 全部已映射模型（继续放行）；凭证命中 app 且其 `models` 非空时，请求 model 不在组合内 → `403`；凭证未命中任何 app → `401`；命中的 app 被禁用 → `403`。
- 配额全部耗尽 → `429`；该协议一个供应商都没配 → `503`；请求带模型时：无任何供应商声明该模型 → `404`，模型有声明但池空（全禁 / 协议不匹配）→ `503`；未知协议段 → `404`；v1 推断失败（两协议共用的端点 / 未知端点 / v1 后无端点）→ `404` 并提示改用显式协议段。
- **按供应商超时**（ADR-L17）：provider 可设 `timeout_secs`（缺省回落 `GATEWAY_DEFAULT_TIMEOUT`）。
- **按供应商 HTTP 代理**（ADR-L19）：provider 可设 `proxy_url`（缺省 = 共享直连 client，与全局行为一致）。配了代理的供应商经**按代理 url 复用的 reqwest client**（自带连接池）转发；HTTP 与 HTTPS 代理均支持（HTTPS 目标走 CONNECT 隧道），socks5 不支持并由 `validate` 拒绝。代理不可达与上游不可达同处理（502）。
- 供应商 CRUD 管理 API（`/__admin/llm/providers*`，与路由 CRUD 同构、合并式更新）+ 应用 CRUD（`/__admin/llm/apps*`，同构）+ 配额查看（`/__admin/llm/quota`）+ 指标（`/__admin/llm/metrics`，复用 `RouteStats`）。
- 供应商配置原子持久化到 `GATEWAY_PROVIDERS_FILE`（默认 `providers.toml`）、应用到 `GATEWAY_APPS_FILE`（默认 `apps.toml`），重启不丢失。
- 供应商凭证按协议注入（OpenAI/SystemOne → `Authorization: Bearer`；Anthropic → `x-api-key` + 缺省 `anthropic-version`）。

## 范围外

- **不做协议转换**：OpenAI 格式的请求只会打到 openai 协议的供应商，不会翻译成 Anthropic 格式（ADR-L2）。
- **不做失败转移 / 重试**：上游传输错误直接 502，不在供应商间重试（LLM 计费非幂等，ADR-L7）。
- **凭证可选、不强制鉴权**（ADR-L15 修订 L11）：入站凭证的作用是**识别应用、套用模型组合**，不是准入控制——无凭证的请求按默认策略（全部已映射模型）放行。真正的网络层准入仍是部署侧（反代 / 绑定回环）的责任。
- **SystemOne 仅留接口桩**：路由、选择、转发与 Bearer 鉴权走通用路径，但 SystemOne 专有的 usage 字段解析**未实现**（记账 0 token），待规格明确后补（ADR-L8）。
- 不做令牌预估 / 预扣：TPM 只按上游实际返回的 usage 记账。
- 不做排队 / 退避管控：全满即 429，何时重试由客户端决定。
- 不做跨厂商模型名翻译：`model` 改写只在**同一供应商内部**（网关名 → 该供应商的上游名）；OpenAI 格式的请求仍只打 openai 协议池（ADR-L2 不变）。
- 不做组合校验的旁路补救：无 `model` 字段 / 非 JSON / 超出缓冲上限的请求**无法校验组合**，按协议级路由放行（风险 R11 记录此盲区）。
- **不支持 socks5 代理**：reqwest 的 `socks` feature 未启用，`proxy_url` 只接受 `http://` / `https://`（风险 R14）。HTTP/HTTPS 代理已覆盖主流本地代理形态。

## 用户与典型用法

| 用户 | 用法 |
|---|---|
| LLM 应用 / 本地工具 | OpenAI / Anthropic SDK 指向 `http://gw:8000/llm/v1`，`api_key` 填**应用 key**（`POST /__admin/llm/apps` 生成）；网关按端点推断协议、按 app 的模型组合放行、把 `model` 改写为选中供应商的上游名。需要无歧义场景改指 `http://gw:8000/llm/{openai\|anthropic}` |
| 服务运维 | `POST /__admin/llm/providers` 登记供应商（base_url、key、协议、模型表、rpm/tpm 限额、timeout），立即生效；`POST /__admin/llm/apps` 给每个本地 app 发 key 并圈定可用模型；`GET /__admin/llm/quota` 看各供应商分钟级余量 |
| 监控 | 轮询 `GET /__admin/llm/metrics`（按供应商的请求数 / 并发 / 错误 / 平均延迟）与 `GET /__admin/llm/quota` |

## 非功能约束

- 复用 top-gateway 进程与技术栈（axum / tokio / reqwest），**不新增进程、不新增端口**；`/llm` 前缀为网关保留。
- 响应 body **流式透传**：usage 提取靠流式尾部缓冲（上限 64 KiB），不缓存整个响应、不阻塞首字节。
- 请求 body **需缓冲才能改写**：读取上限 **16 MiB**（`MAX_REQUEST_BODY`，ADR-L14）；超限 → `413`，不做截断转发。响应流式不受影响。
- 单进程、无外部依赖；供应商表存内存，持久化靠本地 TOML 文件。
- 错误体沿用 top-gateway 统一格式 `{"error":{"code":<u16>,"message":...}}`（对 OpenAI/Anthropic 客户端而言非原生错误格式，ADR-L6/R4 已记录）。
