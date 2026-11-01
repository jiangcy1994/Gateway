# Socket Observatory —— 基于 socket 的观察窗口

这个目录用 [SocketMeasure](../../docs/reference/observe_socket.md) 面板（`http://127.0.0.1:8099`）
的 REST API，**在 socket 层观察流经本仓库 `top-gateway` 的真实流量**。

每个演示脚本自己搭建全部被测环境——mock upstream、网关子进程、面板转发监听器——
跑完即拆，互不残留。面板只做被动的透明转发与计量，不参与协议处理。

## 目录组织

| 脚本 | 主题 | 网关行为（被测） | upstream mock 协议 | 面板看点 |
| --- | --- | --- | --- | --- |
| `demo_00_api_overview.py` | 面板 API 全流程 | 挂路由、404/503 透传 | HTTP/1.1 keep-alive JSON | health→监听器→会话→详情→负载→解读器→CSV，12 步走完 API；自包含、不依赖 `_common.py` |
| `demo_01_route_and_path.py` | 路由与路径 | 最长前缀匹配、strip_prefix 开/关、query 透传、路由级头注入、禁用 503、无路由 404 | HTTP/1.1 keep-alive JSON | 内置 http/text 解读器还原请求行与状态行；8 条会话逐一对照 |
| `demo_02_streaming.py` | 流式转发 | chunked 透传、不缓冲 | chunked 无 Content-Length | 每块到达时间戳证明流式；2 MiB 体与 1 MiB 捕获上限（`truncated`） |
| `demo_03_failures.py` | 失败路径 | 502 不可达 / 504 超时 / 上游 500 | 死端口、挂起连接、恒 500 | 失败会话的 tx 请求行 + rx 状态行；`errors` 计数 |
| `demo_04_binary_protocol.py` | 二进制协议 | 体流透传（Content-Length 被 hop-by-hop 剥除后重定界） | 长度前缀二进制帧 over HTTP、raw TCP echo | 自定义解读器 `binframe` 从原始 L7 字节解帧；HTTP 包裹 vs 裸 TCP 两种会话签名 |
| `demo_05_llm_protocols.py` | LLM 协议透传 | `/llm` 保留前缀、协议段选供应商、凭证替换、尾包 usage 记 TPM、配额耗尽 429 | OpenAI Chat Completions/Responses、Anthropic Messages（非流式 JSON + 流式 SSE） | SSE 逐行到达间隔；tx 负载里的客户端假凭证 vs mock 实收的供应商凭证；`/__admin/llm/quota` 的 TPM 与响应 usage 逐项对账；RPM/TPM 耗尽 -> 429 |
| `_common.py` | 公共库 | — | — | token 读取、面板/网关 API 封装、网关子进程、监听器隔离约定 |

## 运行

前置条件：SocketMeasure 面板已在 `http://127.0.0.1:8099` 运行；网关二进制
（`cargo build` 出的 `target/debug/top-gateway.exe`）与 mock upstream 由脚本自动拉起。

```bash
# token 写在 observation/.token（已 gitignore，随时可改）：
uv run python observation/socket/demo_01_route_and_path.py

# 也可用参数或环境变量覆盖：
uv run python observation/socket/demo_01_route_and_path.py sm_xxxxxxxx
SOCKETMEASURE_TOKEN=sm_xxxxxxxx uv run python observation/socket/demo_02_streaming.py

# 清理后顺带重置面板全部会话（默认不做，见下文「隔离与清理」）：
uv run python observation/socket/demo_00_api_overview.py --reset
```

网关子进程固定绑定 `127.0.0.1:18099`（默认的 8000 端口本机另有应用占用），
admin key 为演示统一的 `demo-admin-key`，路由/providers 文件放在系统临时目录。

## 拓扑

```
HTTP 客户端(脚本) → 127.0.0.1:19190(SocketMeasure 监听器，计量 tx/rx 字节与包)
                 → 127.0.0.1:18099(top-gateway 子进程)
                 → 最长前缀匹配 → strip_prefix → mock upstream(脚本自起)
```

观察的本质：**面板按 TCP 连接计量会话，网关按路由计量请求数**，两套口径描述
不同层面，演示处处以两者互为对照。网关自己的健康探针（gateway → upstream）
不经面板监听器，不会出现在计量里。

## 沉淀的观察事实

- **面板 4 会话 vs metrics 3 请求**（demo_00）：禁用路由的 503 在 `src/proxy.rs`
  的 proxy 早期返回，早于指标记录点（`stats.guard()`），不计入路由 metrics；
  面板按 TCP 字节计量则看得到。
- **502 与 504 的延迟来源不同**（demo_03）：502 是本机 loopback 连接拒绝
  （此机器约 2s，属机器特性）；504 由路由 `timeout_secs` 决定。两者都计入
  `stats.record_error()`，上游 5xx 透传也计。
- **会话不随监听器删除而清除**（全部演示）：面板会话持久存在直到 reset 或满额，
  因此脚本用基线集合差分出「本次新增」的会话，避免混入上一次运行的残余。
- **捕获上限与总量分离**（demo_02）：单方向捕获上限 1 MiB，超限会话标记
  `truncated`，但 `bytes_rx` 等总量计数不受影响。
- **流式而非缓冲**（demo_02）：上游每块间隔 0.4s 写入，客户端实测相邻到达
  间隔约 0.4s——若网关先收齐再转发，间隔应为 0。
- **hop-by-hop 剥除后的重定界**（demo_04）：客户端发的 `Content-Length` 被剥除，
  上游侧改用 chunked 编码（mock 打印可见）；响应回程同理。二进制帧字节
  本身原样穿过。
- **LLM 凭证替换在面板两侧都可对照**（demo_05）：客户端的
  `Authorization: Bearer sk-client-fake` / `x-api-key: sk-ant-client-fake`
  原样留在面板会话的 tx 负载里，而 mock 实收的已被换成供应商 key
  （openai 注入 Bearer，anthropic 注入 `x-api-key` 并补默认
  `anthropic-version: 2023-06-01`）——网关剥凭证、面板看甲乙方，互不干扰。
- **TPM 由响应尾包回填**（demo_05）：`/__admin/llm/quota` 的 `tpm.used` 与
  响应里的 usage 逐项对账：非流式 JSON 直接取 usage 对象，SSE 取尾包
  （openai 汇总 data 块、anthropic 取首 `message_start` 的 input_tokens 加
  末 `message_delta` 的 output_tokens）。流式与非流式混跑后 openai 池
  58+43+58=159、anthropic 池 16+16=32，与 mock 写入的数值完全吻合。
- **协议池语义与路由表无关**（demo_05）：`/llm` 是保留前缀，`/llm/{protocol}`
  后的剩余路径原样透传——同一 `openai` 供应商既服务
  `/v1/chat/completions` 也服务 `/v1/responses`；未知协议段 404，
  协议池为空（全禁用）503。
- **配额极限的三种归宿**（demo_05）：权重 = min(RPM 剩余比, TPM 剩余比)，
  任一维度满额权重即 0；协议池内全部供应商权重 <= 0 时返回 429。用
  `rpm_limit:1` 的 systemone 供应商（其 usage 提取是 stub、TPM 恒 0，
  单独隔离 RPM 维度）第二次请求 -> 429；`tpm_limit:10` 的 anthropic
  供应商单次响应即用 16 > 10，第二次请求 -> 429。429 与池为空的 503
  是两个不同错误码。

## 隔离与清理

- 演示只建自己的监听器（默认 19190 起步，端口冲突自动 +1），不触碰面板已有
  监听器（如面板自测用的 `echo`）。`/api/sessions` 是全局列表，脚本按
  `listen_port` 过滤，只统计本监听器名下的会话。
- 演示注册的自定义解读器（如 `binframe`）在结束时删除。
- 清理只删除：本演示的监听器、网关路由、网关子进程、mock upstream 与解读器。
- `POST /api/reset` 会清空**全部**会话与事件流，影响其他监听器的测试数据，
  因此默认不调用；确需干净基线时加 `--reset`。

## token 存放

`observation/.token` 已在 `.gitignore` 中，改 token 直接编辑该文件即可；
命令行参数与环境变量 `SOCKETMEASURE_TOKEN` 优先级更高。
