# LLM Protocol Observatory —— LLM 协议透传的观察窗口

这个目录用 [SocketMeasure](../../../docs/reference/observe_socket.md) 面板
（`http://127.0.0.1:8099`）的 REST API，**在 socket 层观察 `top-gateway` 内嵌的
LLM 子网关（`/llm` 保留前缀）如何透传三种 LLM 协议**：OpenAI Chat Completions、
OpenAI Responses、Anthropic Messages（各含非流式 JSON 与流式 SSE 两种形态）。

与上级 `observation/socket` 根目录的演示一样，脚本自起 mock upstream、网关子进程
与面板监听器，跑完即拆；不同之处在于被测对象不是路由表，而是 `/llm` 协议池——
路由表里不需要任何条目。

## 目录组织

| 脚本 | 主题 | 网关行为（被测） | upstream mock 协议 | 面板看点 |
| --- | --- | --- | --- | --- |
| `demo_05_llm_protocols.py` | LLM 协议透传 | `/llm` 保留前缀、显式协议段与 `/llm/v1/{端点}` 推断两种入站形态、协议段选供应商、多协议共享供应商、凭证替换、尾包 usage 记 TPM、配额耗尽 429 | OpenAI Chat Completions/Responses、Anthropic Messages（非流式 JSON + 流式 SSE） | SSE 逐行到达间隔；tx 负载里的客户端假凭证 vs mock 实收的供应商凭证；`/__admin/llm/quota` 的 TPM 与响应 usage 逐项对账；共享供应商一条配额记录跨两个协议池；RPM/TPM 耗尽 -> 429 |

`_common.py` 在上级 `observation/socket/` 目录（token 读取、面板/网关 API 封装、
网关子进程、监听器隔离约定），本目录脚本把上级目录加入 `sys.path` 复用。

## 运行

前置条件：SocketMeasure 面板已在 `http://127.0.0.1:8099` 运行；网关二进制
（`cargo build` 出的 `target/debug/top-gateway.exe`）与 mock upstream 由脚本自动拉起。

```bash
uv run python observation/socket/llm/demo_05_llm_protocols.py

# token 也可用参数或环境变量覆盖：
uv run python observation/socket/llm/demo_05_llm_protocols.py sm_xxxxxxxx
SOCKETMEASURE_TOKEN=sm_xxxxxxxx uv run python observation/socket/llm/demo_05_llm_protocols.py
```

网关子进程固定绑定 `127.0.0.1:18099`、admin key 为演示统一的 `demo-admin-key`，
providers 文件放在系统临时目录——与 `qqbot/` 子套件的 18198、本机 18000 端口的
既有部署互不干扰。

## 拓扑

```
业务端(脚本，带假凭证)
  │
  ▼
127.0.0.1:19190 SocketMeasure 监听器（计量 tx/rx 字节与包）
  │
  ▼
127.0.0.1:18099 top-gateway(run)
  │ /llm/{protocol}/... 或 /llm/v1/{端点} —— 保留前缀，不经路由表：
  │   v1 按端点名闭表推断协议（chat->openai，messages->anthropic），
  │   或显式协议段直接定池（加权 = min(RPM 剩余比, TPM 剩余比)）
  │   剥客户端凭证，注入供应商凭证（openai: Bearer；anthropic: x-api-key + anthropic-version）
  ▼
127.0.0.1:<随机端口> LlmMockUpstream（/v1/chat/completions、/v1/responses、/v1/messages）
```

观察的本质：**面板会话的 tx 负载里留着客户端的假凭证，mock 实收的已是供应商
凭证**——网关剥凭证、面板看甲乙方，两者互不干扰，恰好构成凭证替换的两侧对照。

## 沉淀的观察事实

- **LLM 凭证替换在面板两侧都可对照**（demo_05）：客户端的
  `Authorization: Bearer sk-client-fake` / `x-api-key: sk-ant-client-fake`
  原样留在面板会话的 tx 负载里，而 mock 实收的已被换成供应商 key
  （openai 注入 Bearer，anthropic 注入 `x-api-key` 并补默认
  `anthropic-version: 2023-06-01`）。
- **TPM 由响应尾包回填**（demo_05）：`/__admin/llm/quota` 的 `tpm.used` 与
  响应里的 usage 逐项对账：非流式 JSON 直接取 usage 对象，SSE 取尾包
  （openai 汇总 data 块、anthropic 取首 `message_start` 的 input_tokens 加
  末 `message_delta` 的 output_tokens）。流式与非流式混跑后 openai 池
  58+43+58=159、anthropic 池 16+16=32，与 mock 写入的数值完全吻合。
- **协议池语义与路由表无关**（demo_05）：`/llm` 是保留前缀，`/llm/{protocol}`
  后的剩余路径原样透传——同一 `openai` 供应商既服务
  `/v1/chat/completions` 也服务 `/v1/responses`；未知协议段 404，
  协议池为空（全禁用）503。
- **v1 端点推断是闭表、不嗅探**（demo_05）：`/llm/v1/chat/completions` 与
  `/llm/v1/messages` 分别落到 openai / anthropic 池（SDK 只需把 base_url 指
  `/llm/v1`）；两协议共用的端点 `/llm/v1/models` 404 并提示改用显式协议段，
  未知端点同样 404。声明 `protocols: ["openai","anthropic"]` 的共享供应商
  同时进两个协议池，但 quota 视图里只有一条记录——配额按供应商记一份。
- **配额极限的三种归宿**（demo_05）：权重 = min(RPM 剩余比, TPM 剩余比)，
  任一维度满额权重即 0；协议池内全部供应商权重 <= 0 时返回 429。用
  `rpm_limit:1` 的 systemone 供应商（其 usage 提取是 stub、TPM 恒 0，
  单独隔离 RPM 维度）第二次请求 -> 429；`tpm_limit:10` 的 anthropic
  供应商单次响应即用 16 > 10，第二次请求 -> 429。429 与池为空的 503
  是两个不同错误码。
- **SSE 的流式在面板上是逐行到达**（demo_05）：mock 每写一行 sleep 0.3s，
  客户端 `readline` 的相邻到达间隔约 0.3s——若网关先收齐再转发，间隔应为 0。

## 隔离与清理

- 演示只建自己的监听器（默认 19190 起步，端口冲突自动 +1），不触碰面板已有
  监听器；`/api/sessions` 是全局列表，脚本按 `listen_port` 过滤并做基线差分。
- 演示注册的供应商（`demo-openai`、`demo-anthropic`、`demo-tiny-rpm`、
  `demo-tiny-tpm`）在结束时删除；路由表全程不需要任何条目。
- 清理只删除：本演示的监听器、供应商、网关子进程、mock upstream。
- `POST /api/reset` 会清空**全部**会话与事件流，影响其他监听器的测试数据，
  因此默认不调用；确需干净基线时加 `--reset`。

## token 存放

`.token`（本目录，即 `observation/socket/llm/.token`）已在 `.gitignore` 中，
改 token 直接编辑该文件即可；命令行参数与环境变量 `SOCKETMEASURE_TOKEN`
优先级更高。
