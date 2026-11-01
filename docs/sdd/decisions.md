# 设计决策记录（ADR）

> **SDD 第 5 步**：决策记录。只解释前 4 步中真实出现过的选择——每个决定都要追得到模块与具体子文档。

格式：每条记录背景、决策、后果；状态取 提议 / 接受 / 废弃。追加新决策，不在旧条目上改写历史。

---

## ADR-001 顶层网关只做路径前缀挂载，不做按 Host 路由

- 状态：接受（2026-10-31）
- 背景：多子网关收拢到单入口有两种主流路由依据：路径前缀、Host 头。需求是「同一台机器上把几个已有网关挂到同一入口」。
- 决策：按**路径前缀**匹配（`RouteTable::match_route` 最长前缀），不引入 Host/Authority 路由。
- 后果：子网关无需自有域名、本机/内网场景零 DNS 依赖；代价是子网关的页面内绝对链接（`/admin/...`）不会自动落在其前缀下——由 `strip_prefix` 与子网关自身basePath 约定承担。实现见 `interfaces.md` 的 `match_route` 契约。

## ADR-002 流式转发，双向不缓冲 body

- 状态：接受（2026-10-31）
- 背景：上游可能是大文件下载或 SSE/流式接口；缓冲 body 会带来无界内存与首字节延迟。
- 决策：请求体转 `reqwest::Body::wrap_stream`，响应体用 `Body::from_stream(bytes_stream())`，全程逐块透传（`proxy.rs`）。
- 后果：内存占用与 body 大小无关；代价是**没有 body 就无法改写 content-length**，故需在两个方向剥离 `content-length`（见 `interfaces.md` 第 6/10 步），上游必须自行处理 chunked。

## ADR-003 路由表用线性扫描而非前缀树/索引

- 状态：接受（2026-10-31）
- 背景：路由匹配是每个请求的热路径。前缀树（radix trie）在路由数上万时才显出优势。
- 决策：`Vec<Route>` + `RwLock`，`match_route` 一次线性扫描取最长前缀；匹配结果克隆后立即释放锁。
- 后果：挂载数十个网关时扫描数十次字符串比较，快于任何索引结构的额外分配；锁持有时间为微秒级，写操作（管理 API）极少。代价是路由数上千时 `O(n)`——本场景明确不做。

## ADR-004 管理前缀用 `/__admin` 而非 `/admin`

- 状态：接受（2026-10-31）
- 背景：挂在 top-gateway 下的子网关普遍自己占用 `/admin` 路径（`qqbot-api-gateway` 管理后台即如此）。
- 决策：管理 API 固定挂载在 `/__admin`，`axum::nest` 优先于 fallback，未知 `/__admin/*` 返回 404 而非代理。
- 后果：任何子网关都可以安全地挂在前缀下而路径不被管理 API 截胡；代价是管理路径不够语义化（可接受，且在 README 明确说明）。

## ADR-005 `PUT` 是合并语义，`id` 不可改

- 状态：接受（2026-10-31）
- 背景：管理面板「禁用某个路由」这类操作只应传 `{"enabled":false}`，全量替换会要求调用方先读再写，竞态窗口更大。
- 决策：`RoutePatch` 全 `Option` 字段，`apply_patch` 只改 `Some` 的字段；`RoutePatch` 干脆没有 `id` 字段，从结构上杜绝改 id。
- 后果：调用方可以做真正的部分更新；代价是清空 `description` 这类「改成默认值」的需求无法表达（`Option<String>` 的 `None` 与「不传」不可区分）——当前无此需求。

## ADR-006 持久化用原子替换的 TOML 文件，无数据库

- 状态：接受（2026-10-31）
- 背景：路由配置是低频变更、单机的小数据；引入数据库违反「无外部依赖」的非功能约束（`overview.md`）。
- 决策：每次管理变更把整表序列化成 TOML，写到同目录临时文件后 `persist`（rename）替换；启动时从文件加载（缺失不致命）。
- 后果：崩溃/重启不丢失配置，写入对并发读无阻塞；代价是 **rewrite 会丢弃文件中的手工注释**（已在文件头与 interfaces.md 声明），合并写之间存在「最后写入者赢」的语义——管理 API 是唯一写入方，无并发冲突。

## ADR-007 共享状态用「全 Arc 字段」结构体，而非 `Arc<AppState>`

- 状态：接受（2026-10-31）
- 背景：axum 处理器需要 `Clone` 的 state；最早实现用 `State<Arc<AppState>>`，但 `health::spawn`、测试、路由表各自持有的句柄类型不一致，编译期频发 `Arc<AppState>` vs `AppState` 不匹配。
- 决策：把 `AppState` 做成「每个字段都是 `Arc`（或 Arc 包锁）」的结构体并 `derive(Clone)`，处理器直接取 `State<AppState>`。
- 后果：任何持有 `AppState` 的地方都拿到同一份共享状态，类型上不再有两层包装；`RouteTable` 自身也是 `Default + Clone`，测试可以直接构造。代价是字段级粒度比单一 `Arc` 细——内存上无实质差别（每个 Arc 一次分配）。

## ADR-008 健康检查是建议性的，不参与转发决策

- 状态：接受（2026-10-31）
- 背景：健康探测结果有滞后（间隔默认 10s），用它来拦截转发会把「上游临时抖动」放大成「全部请求 503」。
- 决策：代理路径完全不读 `health` map；健康状态仅供 `GET /__admin/health` 观测与 `POST /__admin/probe` 主动验证；转发失败仍由代理自身的 502/504 报告。
- 后果：转发行为可预测、无额外故障面；运维想摘流量时用 `PUT {"enabled":false}`（503）手动操作，而不是依赖自动健康门禁。

## ADR-009 指标用原子计数器 + RAII guard，不用通道/线程局部累积

- 状态：接受（2026-10-31）
- 背景：每个请求都要计数，通道会把热路径变成跨任务通信；普通 `Drop` 之外的记账在早返回路径上容易漏。
- 决策：`RouteStats` 全原子量；`guard()` 返回的 `InFlightGuard` 在 Drop 时一次性完成 `in_flight-1`、`total+1`、`latency+=`。
- 后果：热路径无锁无分配（Arc clone 一次）；所有 return 路径（404/503/502/504/成功）记账一致；`avg_latency` 只在快照时算，运行期零维护成本。

## ADR-010 TDD + 过程记录可审计

- 状态：接受（2026-11-01）
- 背景：「测试先行」不可只靠声明，否则无法区分真先行与事后补测试。
- 决策：实现走红/绿/重构节奏，每个循环在 `../tdd/cycles.md` 留记录（意图/红/绿/重构），只记失败过的真循环。
- 后果：过程可审计，代价是文档维护量；集成测试自包含（同进程起 mock 上游 + 真实网关）使该成本很低。

## ADR-011 单一二进制双模式：SCM 无参启动 + 管理子命令

- 状态：接受（2026-11-01）
- 背景：需要可部署为 Windows 服务，同时保留原有控制台开发体验（`cargo run`、Ctrl+C、stdout 日志）。备选是拆 `top-gateway.exe` + `top-gateway-service.exe` 两个二进制。
- 决策：一个 `top-gateway.exe`：无参数启动 = 服务模式（`service_dispatcher::start` 连接 SCM）；`run` = 前台运行；`install [--autostart]` / `start` / `stop` / `uninstall` / `status` = 服务管理（`src/service.rs`，`cfg(windows)`）。
- 后果：部署只需拷一个 exe，服务与控制台必然同版本；`install` 以自身路径注册，不存在「注册的 exe 找不到」。代价是二进制变大（`windows-service`/`windows-sys` 依赖）且参数解析要区分「SCM 启动」与「双击启动」——后者以 1063 错误提示 `run`/`install`。非 Windows 平台 `main.rs` 仍是普通控制台程序，服务模块不编译。

## ADR-012 服务配置/日志落在 `%ProgramData%\top-gateway`，经 `gateway.env` 取环境，不用注册表

- 状态：接受（2026-11-01）
- 背景：服务进程的工作目录是 `C:\Windows\System32`，且不继承安装者的环境变量——相对路径的 `routes.toml` 与 `GATEWAY_ADMIN_KEY` 都会「丢」。备选是把配置写进服务的 `Parameters` 注册表键。
- 决策：基目录解析引入 `GATEWAY_BASE_DIR`（`config::base_dir_or`），服务默认 `%ProgramData%\top-gateway`，控制台默认 cwd（行为不变）；服务在基目录读 `gateway.env`（`KEY=VALUE`，缺失即 no-op）补齐 `GATEWAY_*`；日志用 `tracing-appender` 按日滚动写基目录（`logging::init_file`）。
- 后果：配置是明文可审计的普通文件，可用现成工具编辑/ diff/ 备份，跨平台路径逻辑共用；`install` 自动建目录并从 exe 同目录补种缺失的 `routes.toml`/`providers.toml` 与 `gateway.env` 模板（从不覆盖已有文件）。代价：`gateway.env` 明文存 `GATEWAY_ADMIN_KEY`（与 `providers.toml` 明文 api_key 同一既定策略，ADR-L11；靠 NTFS 权限与回环绑定缓解），且改基目录需设系统级环境变量。

## ADR-013 SCM 停止信号注入 `run_with_shutdown`，网关本体不感知 SCM

- 状态：接受（2026-11-01）
- 背景：优雅关闭（排空在途连接）的路径已经存在于 `axum::serve().with_graceful_shutdown(...)`；服务外壳不应重写一份服务器生命周期。
- 决策：`lib::run` 拆出 `run_with_shutdown(settings, shutdown: BoxFuture)`，控制台仍用 Ctrl+C/SIGTERM，服务外壳把 `SERVICE_CONTROL_STOP` 的 oneshot 接进去；状态机按 `StartPending → Running → Stopped` 上报，失败时以 win32 退出码 1 报 Stopped。系统关机/断电控制**不**接受（`controls_accepted` 仅 STOP）。
- 后果：关闭语义在两种模式下完全一致，测试可直接覆盖控制台路径；`Stop` 到 Stopped 之间允许 30s 排空（`stop` 子命令轮询），超时只是 Stopped 上报迟到、SCM 超时强杀。代价是系统关机时不做优雅排空——SCM 关机时限内排空被代理的请求只会拖延关机，进程被强杀是可接受的。
