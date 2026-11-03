#!/usr/bin/env python3
"""演示 00：SocketMeasure 面板 API 全流程 —— 测量流经本项目 top-gateway 的流量。

本脚本刻意保持自包含（标准库，无第三方依赖、不依赖 _common.py），
作为「观察窗口」的入口：一条命令走完面板 API 的典型流程。
其余 demo_01~04 按网关行为组织，共享 _common.py。

拓扑（全部由本脚本自动搭建，仅 SocketMeasure 面板需事先运行在 :8099）：

    HTTP 客户端(本脚本)
        ↓ 127.0.0.1:<listen_port>
    SocketMeasure 转发监听器（计量 tx/rx 字节、包、速率）
        ↓ 127.0.0.1:18099
    top-gateway（本仓库 target/debug/top-gateway，子进程启动）
        ↓ prefix /demo/*（strip_prefix 后转发）
    mock upstream（本脚本自起的 HTTP 服务）

运行方式（用环境中 uv 的 Python）：
    # token 存在 llm/.token（已 gitignore，随时可改）：
    uv run python observation/socket/demo_00_api_overview.py
    # 也可用参数或环境变量覆盖：
    uv run python observation/socket/demo_00_api_overview.py sm_xxxxxxxx
    SOCKETMEASURE_TOKEN=sm_xxxxxxxx uv run python observation/socket/demo_00_api_overview.py
    # 清理后顺带重置面板全部会话（默认不清，避免影响其他监听器的测试）：
    uv run python observation/socket/demo_00_api_overview.py --reset

演示要点：
  - 面板侧（docs/reference/observe_socket.md 第 3 节典型流程）：
    health -> 开监听器 -> 产生流量 -> state/sessions -> 会话详情与解读 ->
    下载负载 -> 自定义解读器试运行/注册 -> 导出 CSV -> 清理与重置
  - 网关侧（被测对象）：
    动态挂载路由 /demo、最长前缀匹配、strip_prefix、上游 404 透传、
    禁用路由返回 503、/__admin/metrics 指标读取
  - 对照：网关健康探针（gateway -> upstream，不经面板监听器）不会出现在计量里，
    只有经监听器端口的流量才被 SocketMeasure 计量。
"""

import base64
import http.client
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# ---------------------------------------------------------------- 配置
PANEL_BASE = "http://127.0.0.1:8099"
GATEWAY_BIND = "127.0.0.1:18099"      # 网关绑定地址（脚本子进程启动）
GATEWAY_ADMIN_KEY = "demo-admin-key"  # 演示用 admin key；脚本会带上 X-Admin-Key
LISTEN_PORT = 19190                   # 面板监听器端口，冲突时自动 +1 重试
ROUTE_PREFIX = "/demo"
ROUTE_ID = "demo"

_HERE = os.path.dirname(os.path.abspath(__file__))
# .token 已随 LLM 演示移到 observation/socket/llm/，根目录演示仍要能找到它
_TOKEN_CANDIDATES = [
    os.path.join(_HERE, ".token"),
    os.path.join(_HERE, "llm", ".token"),
    os.path.join(os.path.dirname(_HERE), ".token"),
]
REPO_ROOT = os.path.dirname(os.path.dirname(_HERE))
GATEWAY_BIN = os.path.join(REPO_ROOT, "target", "debug", "top-gateway.exe")

# 是否在清理后调用 /api/reset 清空面板全部会话（默认不清：面板可能另有他人
# 在用，例如自测用的 echo 监听器）。显式加 --reset 才清。
RESET = "--reset" in sys.argv


# ---------------------------------------------------------------- token
def load_token() -> str:
    """优先级：命令行参数 > 环境变量 > .token 文件（本目录或 llm/ 下）。"""
    if len(sys.argv) > 1 and not sys.argv[1].startswith("--"):
        return sys.argv[1].strip()
    env = os.environ.get("SOCKETMEASURE_TOKEN")
    if env:
        return env.strip()
    for path in _TOKEN_CANDIDATES:
        try:
            with open(path, encoding="utf-8") as f:
                return f.read().strip()
        except FileNotFoundError:
            continue
    return ""


TOKEN = load_token()
if not TOKEN:
    print(
        "未找到 SocketMeasure token。任选其一：\n"
        "  1) 写入 observation/socket/llm/.token（已 gitignore）\n"
        "  2) uv run python observation/socket/demo_00_api_overview.py <token>\n"
        "  3) 设置环境变量 SOCKETMEASURE_TOKEN",
        file=sys.stderr,
    )
    sys.exit(2)

# Windows 控制台默认按系统代码页（如 GBK）编码，中文会乱码；统一输出 UTF-8。
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8")
    except (AttributeError, OSError):
        pass


# ---------------------------------------------------------------- HTTP 小封装
class ApiError(Exception):
    def __init__(self, status: int, message: str):
        self.status = status
        self.message = message
        super().__init__(f"HTTP {status}: {message}")


def panel_api(path: str, method: str = "GET", obj: dict | None = None, raw: bool = False):
    """调用 SocketMeasure 面板 /api/* 接口。"""
    data = json.dumps(obj).encode() if obj is not None else None
    req = urllib.request.Request(
        PANEL_BASE + path,
        method=method,
        data=data,
        headers={"X-API-Token": TOKEN, "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.read() if raw else json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        body = e.read().decode(errors="replace")
        try:
            body = json.loads(body).get("error", body)
        except json.JSONDecodeError:
            pass
        raise ApiError(e.code, body) from e


def gateway_admin(path: str, method: str = "GET", obj: dict | None = None):
    """调用网关 /__admin/* 接口（带 X-Admin-Key）。"""
    data = json.dumps(obj).encode() if obj is not None else None
    req = urllib.request.Request(
        f"http://{GATEWAY_BIND}/__admin{path}",
        method=method,
        data=data,
        headers={"X-Admin-Key": GATEWAY_ADMIN_KEY, "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        body = e.read().decode(errors="replace")
        try:
            body = json.loads(body).get("error", body)
        except json.JSONDecodeError:
            pass
        raise ApiError(e.code, body) from e


def step(n, title: str) -> None:
    print(f"\n{'=' * 68}\n步骤 {n}：{title}\n{'=' * 68}")


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def http_request(host: str, port: int, method: str, path: str, body: bytes | None = None) -> tuple[int, bytes]:
    """发起一次 HTTP 请求（每次新建连接，便于面板按连接计量会话）。"""
    conn = http.client.HTTPConnection(host, port, timeout=10)
    try:
        headers = {"Content-Type": "application/json"} if body else {}
        conn.request(method, path, body=body, headers=headers)
        resp = conn.getresponse()
        return resp.status, resp.read()
    finally:
        conn.close()


def wait_for_sessions(expected: int, listen_port: int, timeout: float = 6.0) -> list[dict]:
    """轮询会话列表，只统计本演示监听器名下的会话（面板列表是全局的，
    其他监听器（如面板自测用的 echo）的会话不计入）。"""
    deadline = time.time() + timeout
    while time.time() < deadline:
        sessions = [
            s for s in panel_api("/api/sessions")["sessions"] if s["listen_port"] == listen_port
        ]
        if len(sessions) >= expected:
            return sessions
        time.sleep(0.2)
    return [s for s in panel_api("/api/sessions")["sessions"] if s["listen_port"] == listen_port]


# ---------------------------------------------------------------- mock upstream
class UpstreamHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def handle(self) -> None:
        # keep-alive 连接被对端关闭时会抛 ConnectionResetError，吞掉不刷屏
        try:
            super().handle()
        except (ConnectionResetError, BrokenPipeError):
            pass

    def _json(self, code: int, obj: dict) -> None:
        body = json.dumps(obj, ensure_ascii=False).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        if self.path == "/health":
            self._json(200, {"ok": True, "service": "mock-upstream"})
        elif self.path == "/nope":
            self._json(404, {"error": {"code": 404, "message": "not found: /nope"}})
        else:
            self._json(200, {"method": "GET", "path": self.path, "echo": self.path})

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length)
        parsed = json.loads(body) if body else None
        self._json(200, {"method": "POST", "path": self.path, "received": parsed})

    def log_message(self, fmt, *args) -> None:
        pass  # 不刷屏


# ---------------------------------------------------------------- 网关子进程
def start_gateway() -> subprocess.Popen:
    if not os.path.exists(GATEWAY_BIN):
        print(f"未找到网关二进制 {GATEWAY_BIN}，执行 cargo build …")
        subprocess.run(["cargo", "build"], cwd=REPO_ROOT, check=True)

    routes_file = os.path.join(tempfile.gettempdir(), "demo-gateway-routes.toml")
    providers_file = os.path.join(tempfile.gettempdir(), "demo-gateway-providers.toml")
    for f in (routes_file, providers_file):
        if os.path.exists(f):
            os.remove(f)
    env = dict(os.environ)
    env.update(
        {
            "GATEWAY_BIND": GATEWAY_BIND,
            "GATEWAY_ADMIN_KEY": GATEWAY_ADMIN_KEY,
            "GATEWAY_ROUTES_FILE": routes_file,
            "GATEWAY_PROVIDERS_FILE": providers_file,
        }
    )
    proc = subprocess.Popen(
        # Windows 上无参数启动会尝试进入 SCM 服务模式（虚报 1063），
        # 必须显式带 `run` 走控制台模式（src/service.rs::dispatch）。
        [GATEWAY_BIN, "run"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    deadline = time.time() + 15
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(f"http://{GATEWAY_BIND}/health", timeout=2) as r:
                if r.status == 200:
                    return proc
        except OSError:
            pass
        time.sleep(0.2)
    proc.terminate()
    raise RuntimeError(f"网关在 {GATEWAY_BIND} 启动超时")


def main() -> int:
    gateway_proc: subprocess.Popen | None = None
    upstream: ThreadingHTTPServer | None = None
    listener_id: str | None = None
    try:
        # -------------------------------------------------------------- 1
        step(1, "GET /api/health —— 验证面板可达与令牌")
        health = panel_api("/api/health")
        print(json.dumps(health, ensure_ascii=False, indent=2))
        print(f"\n令牌有效，title = {health['token_title']!r}")

        # -------------------------------------------------------------- 2
        step(2, "启动被测对象：top-gateway 子进程 + mock upstream")
        gateway_proc = start_gateway()
        print(f"网关已启动：http://{GATEWAY_BIND}")
        upstream_port = free_port()
        upstream = ThreadingHTTPServer(("127.0.0.1", upstream_port), UpstreamHandler)
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        print(f"mock upstream 已启动：http://127.0.0.1:{upstream_port}（/health 与任意路径回显）")

        # -------------------------------------------------------------- 3
        step(3, "POST /__admin/routes —— 通过管理 API 挂载路由 /demo")
        gateway_admin(
            "/routes",
            "POST",
            {
                "id": ROUTE_ID,
                "prefix": ROUTE_PREFIX,
                "upstream": f"http://127.0.0.1:{upstream_port}",
                "strip_prefix": True,
                "description": "demo route for socketmeasure",
            },
        )
        print(json.dumps(gateway_admin("/routes")["routes"], ensure_ascii=False, indent=2))

        # -------------------------------------------------------------- 4
        step(4, "POST /api/listeners —— 开转发监听器，目标指向网关")
        listener = None
        listen_port = LISTEN_PORT
        for _ in range(10):
            try:
                listener = panel_api(
                    "/api/listeners",
                    "POST",
                    {
                        "name": "gw-probe",
                        "listen_port": listen_port,
                        "target_host": "127.0.0.1",
                        "target_port": int(GATEWAY_BIND.rsplit(":", 1)[1]),
                    },
                )["listener"]
                break
            except ApiError as e:
                if e.status != 400:
                    raise
                print(f"  端口 {listen_port} 冲突，换一个重试")
                listen_port += 1
        if listener is None:
            print("无法分配监听端口")
            return 1
        listener_id = listener["id"]
        print(json.dumps(listener, ensure_ascii=False, indent=2))

        # -------------------------------------------------------------- 5
        step(5, "产生被测流量：经监听器端口访问网关 /demo/*")
        expected = 0

        def hit(tag: str, method: str, path: str, body: bytes | None = None) -> None:
            nonlocal expected
            status, data = http_request("127.0.0.1", listen_port, method, path, body)
            expected += 1
            snippet = data[:60].decode(errors="replace") + ("…" if len(data) > 60 else "")
            print(f"  {tag:12s} {method} {path:22s} -> {status} {snippet}")

        hit("正常 GET", "GET", f"{ROUTE_PREFIX}/hello")
        hit("带体 POST", "POST", f"{ROUTE_PREFIX}/echo", b'{"msg": "ping", "n": 42}')
        hit("上游 404", "GET", f"{ROUTE_PREFIX}/nope")

        print("\n  —— 禁用路由后再请求（演示网关 503），随后重新启用 ——")
        gateway_admin(f"/routes/{ROUTE_ID}", "PUT", {"enabled": False})
        hit("禁用路由", "GET", f"{ROUTE_PREFIX}/hello")
        gateway_admin(f"/routes/{ROUTE_ID}", "PUT", {"enabled": True})
        print(f"\n  共 {expected} 次请求 = {expected} 条 TCP 会话")

        # -------------------------------------------------------------- 6
        step(6, "GET /api/state + /api/sessions —— 读取计量结果")
        sessions = wait_for_sessions(expected, listen_port)
        print("state.totals（全局，含其他监听器）=")
        print(json.dumps(panel_api("/api/state")["totals"], ensure_ascii=False, indent=2))
        mine = next(
            (l for l in panel_api("/api/listeners")["listeners"] if l["id"] == listener_id), {}
        )
        print(f"\n本演示监听器 gw-probe（{listen_port}）自身计数 =")
        print(json.dumps({k: mine[k] for k in ("sessions", "bytes_tx", "bytes_rx", "pkts_tx", "pkts_rx") if k in mine},
                         ensure_ascii=False, indent=2))
        print("\n本监听器会话摘要（新在前）=")
        for s in sessions:
            print(
                f"  {s['id'][:12]}  state={s['state']:<7s} tx={s['bytes_tx']:>5d} B rx={s['bytes_rx']:>5d} B "
                f"pkts tx/rx={s['pkts_tx']}/{s['pkts_rx']}  dur={s['duration_s']:.2f}s"
            )

        # -------------------------------------------------------------- 7
        step(7, "GET /api/sessions/{id} —— 看会话负载样本与解读结果")
        for s in sessions[:2]:
            det = panel_api(f"/api/sessions/{s['id']}")["session"]
            print(f"\n会话 {det['id']}（tx {det['bytes_tx']} B / rx {det['bytes_rx']} B）")
            for direction in ("tx", "rx"):
                print(f"  方向 {direction}:")
                for r in det["interpret"][direction]:
                    res = r.get("result") or {}
                    print(f"    [{r['id']}] {r['title']}: {res.get('summary', '')}")
                    if r["id"] == "text":
                        for line in (res.get("text") or "").splitlines()[:2]:
                            print(f"        | {line[:72]}")
            tx_raw = base64.b64decode(det["payload_b64"]["tx"])
            rx_raw = base64.b64decode(det["payload_b64"]["rx"])
            print(f"  payload 解码：tx 首行 {tx_raw.splitlines()[0]!r}；rx 首行 {rx_raw.splitlines()[0]!r}")

        # -------------------------------------------------------------- 8
        step(8, "GET /api/sessions/{id}/payload —— 下载原始负载（hex / text）")
        sid = sessions[0]["id"]
        for enc in ("hex", "text"):
            data = panel_api(f"/api/sessions/{sid}/payload?direction=tx&encoding={enc}", raw=True)
            print(f"  encoding={enc}: {len(data)} B -> {data[:72]!r}")
        print("  （bin 编码为原始字节，响应带 Content-Disposition 附件头）")

        # -------------------------------------------------------------- 9
        step(9, "自定义解读器：试运行 -> 注册 -> 重解读（解析网关请求行）")
        gw_probe_code = (
            "def interpret(payload, meta):\n"
            "    text = payload.decode('utf-8', 'replace')\n"
            "    first = text.split('\\r\\n', 1)[0]\n"
            "    sep = text.find('\\r\\n\\r\\n')\n"
            "    body = text[sep + 4:] if sep >= 0 else ''\n"
            "    return {\n"
            '        "summary": "gw-line: %s" % first,\n'
            '        "rows": [{"label": "请求/状态行", "value": first},\n'
            '                {"label": "体长度", "value": str(len(body))},\n'
            '                {"label": "方向", "value": meta.get("direction", "?")}],\n'
            "    }"
        )
        trial = panel_api(
            "/api/interpreters-test",
            "POST",
            {"code": gw_probe_code, "session_id": sid, "direction": "tx"},
        )
        print("试运行（不注册）=")
        print(json.dumps(trial, ensure_ascii=False, indent=2))
        panel_api(
            "/api/interpreters",
            "POST",
            {"id": "gw-line", "title": "网关请求行", "enabled": True, "code": gw_probe_code},
        )
        panel_api(f"/api/sessions/{sid}/interpret", "POST")
        det = panel_api(f"/api/sessions/{sid}")["session"]
        print("\n注册并重解读后，tx 方向解读器：")
        for r in det["interpret"]["tx"]:
            rows = (r.get("result") or {}).get("rows") or []
            print(f"  [{r['id']}] {rows[0]['value'] if rows else ''}")

        # ------------------------------------------------------------- 10
        step(10, "GET /__admin/metrics —— 网关侧指标（与面板计量互为对照）")
        metrics = gateway_admin("/metrics")
        for m in metrics.get("routes", []):
            route, stats = m["route"], m["stats"]
            if route.get("id") == ROUTE_ID:
                print(json.dumps(stats, ensure_ascii=False, indent=2))
        print("  注：面板 totals 统计监听器端口的字节/包；网关 metrics 统计经路由的"
              "请求数/延迟/错误，两者口径不同。")

        # ------------------------------------------------------------- 11
        step(11, "GET /api/export?format=csv —— 导出全部会话")
        csv_bytes = panel_api("/api/export?format=csv", raw=True)
        print(f"导出 {len(csv_bytes)} B（UTF-8 BOM，Excel 友好），前 120 字节：")
        print(f"  {csv_bytes[:120]!r}")

        # ------------------------------------------------------------- 12
        step(12, "清理：删监听器、删路由、停网关（不影响面板其他监听器）")
        panel_api(f"/api/listeners/{listener_id}", "DELETE")
        print(f"已删除本演示的监听器 gw-probe（端口 {listen_port} 释放）")
        gateway_admin(f"/routes/{ROUTE_ID}", "DELETE")
        print(f"已删除网关路由 {ROUTE_ID}")
        remaining = [l["name"] for l in panel_api("/api/listeners")["listeners"]]
        print(f"面板剩余监听器（未受影响）：{remaining}")
        if RESET:
            reset = panel_api("/api/reset", "POST", {"sessions": True, "events": True})
            print(f"已按 --reset 重置面板基线：{json.dumps(reset, ensure_ascii=False)}")
        else:
            print("未执行 /api/reset（它会清空全部会话，包括其他监听器的测试数据；"
                  "如需重置请加 --reset 参数运行）")

        print("\n演示完成。可在面板「令牌与调用」页查看本次全部调用的日志。")
        return 0
    finally:
        # 无论成功或失败，都把子进程与线程服务清掉
        if gateway_proc is not None and gateway_proc.poll() is None:
            gateway_proc.terminate()
            try:
                gateway_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                gateway_proc.kill()
        if upstream is not None:
            upstream.shutdown()


if __name__ == "__main__":
    try:
        sys.exit(main())
    except ApiError as e:
        print(f"\nAPI 调用失败：{e}", file=sys.stderr)
        sys.exit(1)
