# 概述（LLM 子网关）

> **SDD 第 1 步**：系统是什么、边界在哪里。后续所有步骤以此为范围。
> 本文档是 [`../overview.md`](../overview.md)（top-gateway）的子系统规格，只描述 LLM 子网关；路由转发、健康、指标等通用行为以 top-gateway 文档为准。

## 一句话定义

LLM 子网关是**内嵌于 top-gateway 的 LLM API 聚合层**：客户端按 OpenAI / Anthropic / SystemOne 三种协议的原生路径发到 `/llm/{protocol}/...`，网关按协议筛选供应商，依据各供应商的 RPM 与 TPM 限额（**供应商维度、跨其名下所有模型汇总**）做剩余配额加权的负载均衡，配额全部耗尽时返回 429。

## 它解决什么

一个 LLM 应用同时对接多家供应商（OpenAI 官方、Azure OpenAI、Anthropic、兼容 OpenAI 协议的中转、内部 SystemOne 网关……）时，每家各有 RPM/TPM 限额，且限额是**供应商级**的——同一家供应商下所有模型共享一个计数。应用侧自己实现「选哪家还能打」既要维护各家的限额，又要跨请求做分钟级窗口计数。

LLM 子网关把这件事收进网关：

```
client (OpenAI SDK, base_url=http://gw:8000/llm/openai)
        │  POST /llm/openai/v1/chat/completions  {"model":"gpt-4o", ...}
        ▼
top-gateway:8000
  ├─ /llm/openai/*   ──► 选 openai 协议供应商（按剩余 RPM/TPM 加权）
  ├─ /llm/anthropic/*──► 选 anthropic 协议供应商
  ├─ /llm/systemone/*──► 选 systemone 协议供应商（桩：见范围外）
  ├─ /__admin/llm/*  ──► 供应商配置 / 配额查看 / 指标
  └─ 其余路径        ──► 原有 routes.toml 前缀转发（不受影响）
```

## 范围内

- 按**协议透传**转发：入站路径剥掉 `/llm/{protocol}` 后原样拼到供应商 `base_url`，body 与响应流式透传，不做协议转换。
- 按协议过滤启用的供应商，**剩余配额加权随机**选择；同一临界区内完成「选择 + RPM 计数」，并发请求不会同时选中只剩 1 RPM 的供应商。
- RPM 派遣即计数；TPM 从上游响应的 `usage` 字段**事后记账**（非流式 JSON 与 SSE 末尾 usage 均支持，best-effort）。
- 供应商级 60 秒滚动窗口；RPM/TPM 限额均可选（`None` = 不限量）。
- 配额全部耗尽 → `429`；该协议一个供应商都没配 → `503`；未知协议段 → `404`。
- 供应商 CRUD 管理 API（`/__admin/llm/providers*`，与路由 CRUD 同构、合并式更新）+ 配额查看（`/__admin/llm/quota`）+ 指标（`/__admin/llm/metrics`，复用 `RouteStats`）。
- 供应商配置原子持久化到 `GATEWAY_PROVIDERS_FILE`（默认 `providers.toml`），重启不丢失。
- 供应商凭证按协议注入（OpenAI/SystemOne → `Authorization: Bearer`；Anthropic → `x-api-key` + 缺省 `anthropic-version`）。

## 范围外

- **不做协议转换**：OpenAI 格式的请求只会打到 openai 协议的供应商，不会翻译成 Anthropic 格式（ADR-L2）。
- **不做失败转移 / 重试**：上游传输错误直接 502，不在供应商间重试（LLM 计费非幂等，ADR-L7）。
- **不做客户端鉴权**：与 top-gateway 的转发哲学一致——`/llm/*` 端点本身不校验客户端 key；供应商 key 由网关注入。客户端鉴权属于部署侧（反代 / 绑定回环）的责任（ADR-L11）。
- **SystemOne 仅留接口桩**：路由、选择、转发与 Bearer 鉴权走通用路径，但 SystemOne 专有的 usage 字段解析**未实现**（记账 0 token），待规格明确后补（ADR-L8）。
- 不做令牌预估 / 预扣：TPM 只按上游实际返回的 usage 记账。
- 不做排队 / 退避管控：全满即 429，何时重试由客户端决定。
- 不做模型级路由、模型白名单：`model` 字段在 body 里原样透传，供应商自己决定可用模型。

## 用户与典型用法

| 用户 | 用法 |
|---|---|
| LLM 应用 | OpenAI SDK 指向 `http://gw:8000/llm/openai`；Anthropic SDK 指向 `http://gw:8000/llm/anthropic`，代码与直连官方一致 |
| 服务运维 | `POST /__admin/llm/providers` 登记供应商（base_url、key、协议、rpm/tpm 限额），立即生效；`GET /__admin/llm/quota` 看各供应商分钟级余量 |
| 监控 | 轮询 `GET /__admin/llm/metrics`（按供应商的请求数 / 并发 / 错误 / 平均延迟）与 `GET /__admin/llm/quota` |

## 非功能约束

- 复用 top-gateway 进程与技术栈（axum / tokio / reqwest），**不新增进程、不新增端口**；`/llm` 前缀为网关保留。
- 响应 body **流式透传**：usage 提取靠流式尾部缓冲（上限 64 KiB），不缓存整个响应、不阻塞首字节。
- 单进程、无外部依赖；供应商表存内存，持久化靠本地 TOML 文件。
- 错误体沿用 top-gateway 统一格式 `{"error":{"code":<u16>,"message":...}}`（对 OpenAI/Anthropic 客户端而言非原生错误格式，ADR-L6/R4 已记录）。
