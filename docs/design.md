# top-gateway 设计文档（SDD 主文档）

> SDD（规格驱动开发）的入口：先写规格，再写代码；接口变更先改 `sdd/interfaces.md`，再改代码。

一句话架构：**top-gateway 是一个按路径前缀挂载子网关的流式反向代理，配套提供带鉴权的 REST 管理 API，路由变更即时生效并原子持久化到 TOML 文件。**

## SDD 子文档索引

| 步 | 子文档 | 回答 | 何时读 |
|---|---|---|---|
| 1 | [sdd/overview.md](sdd/overview.md) | 系统是什么、边界在哪 | 第一次接触本项目 |
| 2 | [sdd/architecture.md](sdd/architecture.md) | 模块怎么划分、请求怎么流动 | 改动请求路径或模块边界前 |
| 3 | [sdd/data-model.md](sdd/data-model.md) | 核心类型与不变式 | 新增/修改字段前 |
| 4 | [sdd/interfaces.md](sdd/interfaces.md) | 签名与契约（**实现的唯一权威**） | 写任何函数或端点前 |
| 5 | [sdd/decisions.md](sdd/decisions.md) | 关键选择为什么这么定 | 质疑某个设计、或想做相反的决定前 |
| 6 | [sdd/risks.md](sdd/risks.md) | 什么会坏、怎么验证不会 | 发布前、改动涉及风险条目时 |

顺序即依赖：2 依赖 1，3 依赖 2，4 依赖 3……允许回跳，不允许跳步。

## TDD 过程记录

| 文档 | 回答 |
|---|---|
| [tdd/cycles.md](tdd/cycles.md) | 每个功能是「先写测试、看到红、再写实现」走过来的吗 |

## 模块速览

| 模块 | 职责 |
|---|---|
| `src/main.rs` | 进程入口：日志初始化、读环境变量、调 `run` |
| `src/lib.rs` | 组装 Router（`/health` + `/__admin` + 代理 fallback）、`run` 生命周期 |
| `src/proxy.rs` | 反向代理 fallback 处理器：最长前缀匹配、剥前缀、流式转发、502/504 |
| `src/router.rs` | `Route` 值对象与 `RouteTable`（最长前缀匹配的读写锁路由表） |
| `src/admin.rs` | `/__admin/*` 管理 API：CRUD、健康查看、指标查看、立即探测 |
| `src/config.rs` | 环境变量 `Settings` 与 TOML 路由文件的加载/原子保存 |
| `src/health.rs` | 后台健康检查循环与 `HealthState` 视图 |
| `src/metrics.rs` | 每路由无锁计数器与 `InFlightGuard` |
| `src/state.rs` | `AppState`：共享状态的廉价 Clone 句柄 |
| `src/error.rs` | `ApiError` 枚举到 HTTP 状态码与 JSON 错误体的映射 |

## 接口速览

- 对外 HTTP：`GET /health`（无需鉴权）、`/__admin/*`（需 `X-Admin-Key`）、其余路径全部走代理。
- 管理 API：`GET/POST /__admin/routes`、`GET/PUT/DELETE /__admin/routes/{id}`、`GET /__admin/health`、`GET /__admin/metrics`、`POST /__admin/probe`。
- Rust 公共 API：`build_app(state) -> Router`、`run(settings)`、`AppState::new(settings)`、`RouteTable::{set_all, list, get, match_route, add, replace, remove}`。
- 完整签名与前后置条件见 [sdd/interfaces.md](sdd/interfaces.md)。

## 维护声明

- **接口变更先改 `sdd/interfaces.md`，再改代码**；测试以该文件为准。
- 新增设计决策追加到 `sdd/decisions.md`，不改写旧条目。
- 新增风险追加到 `sdd/risks.md`，不覆盖已有条目的历史评估。
- TDD 循环只记录真正失败过的红/绿循环，追加不改历史。
