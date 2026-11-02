#!/usr/bin/env python3
"""SocketMeasure × top-gateway 演示公共库。

各演示脚本共享：token 读取、面板 API 封装、网关子进程拉起、
监听器管理（含与其他监听器的隔离约定）、输出格式化。
"""

import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

# ---------------------------------------------------------------- 配置
PANEL_BASE = "http://127.0.0.1:8099"
GATEWAY_ADMIN_KEY = "demo-admin-key"  # 各演示统一的 admin key
DEFAULT_LISTEN_PORT = 19190           # 面板监听器起始端口，冲突自动 +1

_REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
GATEWAY_BIN = os.path.join(_REPO_ROOT, "target", "debug", "top-gateway.exe")

# token 查找顺序：脚本所在目录 .token -> 上级 docs/reference/.token
_TOKEN_CANDIDATES = [
    os.path.join(os.path.dirname(os.path.abspath(__file__)), ".token"),
    os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), ".token"),
]


# ---------------------------------------------------------------- token
def load_token() -> str:
    """优先级：命令行参数 > 环境变量 > .token 文件（已 gitignore，可随时改）。"""
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


def reset_flag() -> bool:
    """--reset：清理后是否调用 /api/reset（会清空全部会话，默认不清）。"""
    return "--reset" in sys.argv


# ---------------------------------------------------------------- 输出
def step(n, title: str) -> None:
    print(f"\n{'=' * 68}\n步骤 {n}：{title}\n{'=' * 68}")


def note(text: str) -> None:
    print(f"  注：{text}")


# ---------------------------------------------------------------- HTTP 封装
class ApiError(Exception):
    def __init__(self, status: int, message: str, source: str = "panel"):
        self.status = status
        self.message = message
        super().__init__(f"[{source}] HTTP {status}: {message}")


def _request(base: str, path: str, method: str, headers: dict, obj, raw: bool, source: str):
    data = json.dumps(obj).encode() if obj is not None else None
    req = urllib.request.Request(base + path, method=method, data=data, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            return r.read() if raw else json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        body = e.read().decode(errors="replace")
        try:
            body = json.loads(body).get("error", body)
        except json.JSONDecodeError:
            pass
        raise ApiError(e.code, body, source) from e


def panel_api(path: str, method: str = "GET", obj: dict | None = None, raw: bool = False, token: str = ""):
    """SocketMeasure 面板 /api/* 调用。"""
    headers = {"X-API-Token": token, "Content-Type": "application/json"}
    return _request(PANEL_BASE, path, method, headers, obj, raw, "panel")


def gateway_api(bind: str, path: str, method: str = "GET", obj: dict | None = None,
                admin_key: str = GATEWAY_ADMIN_KEY, raw: bool = False):
    """网关 /__admin/* 调用（带 X-Admin-Key）。"""
    headers = {"X-Admin-Key": admin_key, "Content-Type": "application/json"}
    return _request(f"http://{bind}/__admin{path}", "", method, headers, obj, raw, "gateway")


def make_gateway_admin(bind: str):
    """返回绑定到指定网关地址的 admin 调用函数。"""
    def call(path: str, method: str = "GET", obj: dict | None = None):
        return gateway_api(bind, path, method, obj)
    return call


# ---------------------------------------------------------------- 网络工具
def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def http_request(host: str, port: int, method: str, path: str,
                 body: bytes | None = None, headers: dict | None = None) -> tuple[int, dict, bytes]:
    """单次 HTTP 请求（每次新建连接，便于面板按 TCP 连接计量会话）。"""
    import http.client

    conn = http.client.HTTPConnection(host, port, timeout=15)
    try:
        hdrs = dict(headers or {})
        if body is not None and "Content-Type" not in hdrs:
            hdrs["Content-Type"] = "application/octet-stream"
        conn.request(method, path, body=body, headers=hdrs)
        resp = conn.getresponse()
        return resp.status, dict(resp.getheaders()), resp.read()
    finally:
        conn.close()


# ---------------------------------------------------------------- 网关子进程
class Gateway:
    """top-gateway 子进程：构造时启动，context manager 退出或 close() 时停止。

    routes/providers 文件放在系统临时目录，互不干扰；停止后不残留。
    """

    def __init__(self, bind: str = "127.0.0.1:18099", admin_key: str = GATEWAY_ADMIN_KEY):
        self.bind = bind
        self.admin_key = admin_key
        if not os.path.exists(GATEWAY_BIN):
            print(f"未找到网关二进制 {GATEWAY_BIN}，执行 cargo build …")
            subprocess.run(["cargo", "build"], cwd=_REPO_ROOT, check=True)
        self._routes_file = os.path.join(tempfile.gettempdir(), f"demo-routes-{bind.replace(':', '-')}.toml")
        self._providers_file = os.path.join(tempfile.gettempdir(), f"demo-providers-{bind.replace(':', '-')}.toml")
        for f in (self._routes_file, self._providers_file):
            if os.path.exists(f):
                os.remove(f)
        env = dict(os.environ)
        env.update(
            {
                "GATEWAY_BIND": bind,
                "GATEWAY_ADMIN_KEY": admin_key,
                "GATEWAY_ROUTES_FILE": self._routes_file,
                "GATEWAY_PROVIDERS_FILE": self._providers_file,
            }
        )
        self.proc = subprocess.Popen(
            # Windows 上无参数启动会尝试进入 SCM 服务模式（虚报 1063），
            # 必须显式带 `run` 走控制台模式（src/service.rs::dispatch）。
            [GATEWAY_BIN, "run"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
        )
        deadline = time.time() + 15
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f"网关进程意外退出，code={self.proc.returncode}")
            try:
                with urllib.request.urlopen(f"http://{bind}/health", timeout=2) as r:
                    if r.status == 200:
                        return
            except OSError:
                pass
            time.sleep(0.2)
        self.close()
        raise RuntimeError(f"网关在 {bind} 启动超时")

    @property
    def admin(self):
        return make_gateway_admin(self.bind)

    def close(self) -> None:
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()

    def __enter__(self) -> "Gateway":
        return self

    def __exit__(self, *exc) -> None:
        self.close()


def add_route(gateway: Gateway, route_id: str, prefix: str, upstream: str, **extra) -> dict:
    """挂路由并回读校验。"""
    body = {"id": route_id, "prefix": prefix, "upstream": upstream, "strip_prefix": True}
    body.update(extra)
    gateway.admin("/routes", "POST", body)
    routes = gateway.admin("/routes")["routes"]
    return next(r for r in routes if r["id"] == route_id)


# ---------------------------------------------------------------- 面板监听器
class Listener:
    """面板转发监听器：构造时创建，close() 时删除。"""

    def __init__(self, token: str, target_host: str, target_port: int,
                 name: str = "demo", listen_port: int = DEFAULT_LISTEN_PORT):
        self.token = token
        self.obj = None
        port = listen_port
        for _ in range(10):
            try:
                self.obj = panel_api(
                    "/api/listeners", "POST",
                    {"name": name, "listen_port": port,
                     "target_host": target_host, "target_port": target_port},
                    token=token,
                )["listener"]
                return
            except ApiError as e:
                if e.status != 400:
                    raise
                print(f"  端口 {port} 冲突，换一个重试")
            port += 1
        raise RuntimeError("无法分配监听端口")

    @property
    def id(self) -> str:
        return self.obj["id"]

    @property
    def listen_port(self) -> int:
        return self.obj["listen_port"]

    def close(self) -> None:
        if self.obj is not None:
            panel_api(f"/api/listeners/{self.id}", "DELETE", token=self.token)

    def __enter__(self) -> "Listener":
        return self

    def __exit__(self, *exc) -> None:
        self.close()


def wait_for_sessions(token: str, expected: int, listen_port: int, timeout: float = 8.0) -> list[dict]:
    """轮询会话列表，只统计指定监听器端口的会话（面板列表是全局的，
    其他监听器如面板自测用的 echo 的会话不计入）。"""
    deadline = time.time() + timeout
    while True:
        sessions = [
            s for s in panel_api("/api/sessions", token=token)["sessions"]
            if s["listen_port"] == listen_port
        ]
        if len(sessions) >= expected or time.time() > deadline:
            return sessions
        time.sleep(0.2)


def sessions_of(token: str, listen_port: int) -> list[dict]:
    return [
        s for s in panel_api("/api/sessions", token=token)["sessions"]
        if s["listen_port"] == listen_port
    ]


def session_baseline(token: str, listen_port: int) -> set[str]:
    """记录当前会话 id 基线。面板会话不随监听器删除而清除，
    之前的运行残余仍在同一 listen_port 名下，必须与之区分。"""
    return {s["id"] for s in sessions_of(token, listen_port)}


def wait_for_new_sessions(token: str, listen_port: int, baseline: set[str],
                          expected: int, timeout: float = 8.0) -> list[dict]:
    """轮询直到本监听器新增会话数达到 expected（基线之外的新会话）。"""
    deadline = time.time() + timeout
    while True:
        sessions = [s for s in sessions_of(token, listen_port) if s["id"] not in baseline]
        if len(sessions) >= expected or time.time() > deadline:
            return sessions
        time.sleep(0.2)


def listener_stats(token: str, listener_id: str) -> dict:
    return next(
        (l for l in panel_api("/api/listeners", token=token)["listeners"] if l["id"] == listener_id), {}
    )


def register_interpreter(token: str, interp_id: str, title: str, code: str) -> None:
    panel_api("/api/interpreters", "POST",
              {"id": interp_id, "title": title, "enabled": True, "code": code}, token=token)


def delete_interpreter(token: str, interp_id: str) -> None:
    try:
        panel_api(f"/api/interpreters/{interp_id}", "DELETE", token=token)
    except ApiError as e:
        if e.status != 404:
            raise


def interpret_session(token: str, session_id: str) -> dict:
    return panel_api(f"/api/sessions/{session_id}/interpret", "POST", token=token)


def session_detail(token: str, session_id: str) -> dict:
    return panel_api(f"/api/sessions/{session_id}", token=token)["session"]


def print_interpret(det: dict, direction: str, only: tuple[str, ...] = (), indent: str = "    ") -> None:
    for r in det["interpret"][direction]:
        if only and r["id"] not in only:
            continue
        res = r.get("result") or {}
        summary = res.get("summary", "")
        rows = res.get("rows") or []
        print(f"{indent}[{r['id']}] {r['title']}: {summary}")
        for row in rows[:4]:
            print(f"{indent}    {row.get('label')}: {row.get('value')}")


def print_session_rows(sessions: list[dict]) -> None:
    for s in sessions:
        print(
            f"    {s['id'][:12]} state={s['state']:<7s} tx={s['bytes_tx']:>6d} B rx={s['bytes_rx']:>6d} B "
            f"pkts={s['pkts_tx']}/{s['pkts_rx']}"
        )


def maybe_reset(token: str) -> None:
    if reset_flag():
        reset = panel_api("/api/reset", "POST", {"sessions": True, "events": True}, token=token)
        print(f"  已按 --reset 重置面板基线：{json.dumps(reset, ensure_ascii=False)}")
    else:
        print("  未执行 /api/reset（会清空全部会话，含其他监听器的数据；需要请加 --reset）")


# ---------------------------------------------------------------- stdout 编码
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8")
    except (AttributeError, OSError):
        pass
