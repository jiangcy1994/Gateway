#!/usr/bin/env python3
"""QQ Bot Mock × top-gateway 演示公共库。

各演示脚本共享：mock 管理端封装、网关子进程拉起（控制台模式 `run`）、
事件投递接收器（EventSink）、WS supervisor 状态轮询、输出格式化。

与 observation/socket 根目录的 socket 层演示不同，这里的 upstream mock（QQ Bot Mock）
是**共享的外部服务**，不由脚本自起：默认在 http://127.0.0.1:8080，可用环境变量
QQBOTMOCK_BASE 覆盖。因此脚本遵守「不重置 mock 世界」的约定——详见
README 的「隔离与清理」。
"""

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
MOCK_BASE = os.environ.get("QQBOTMOCK_BASE", "http://127.0.0.1:8080")
GATEWAY_BIND = "127.0.0.1:18198"      # 与 socket 演示的 18099、既有部署的 18000 错开
GATEWAY_ADMIN_KEY = "demo-admin-key"  # 各演示统一的 admin key
MOCK_APP_ID = "1020012345"            # mock 默认凭据（docs/reference/qq-bot-mock-api.md §0）
MOCK_APP_SECRET = "mock_client_secret"
DEFAULT_INTENTS = (1 << 25) | (1 << 26)  # GROUP_AND_C2C_EVENT | INTERACTION

# 种子世界（mock §11）：读书分享会已加入，技术交流群未加入
GROUP_READER = "3E5D8A1F7B2C9E4D6A0F1B3C5D7E9F2A"   # 读书分享会
GROUP_TECH = "4F6E8B2A9C1D3E5F7A9B1C3D5E7F9A1B"      # 技术交流群（机器人未入群）
USER_XIAOMING = "A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4"   # 小明

# 必失败的回调地址（本机无监听）→ 用来把平台 webhook 推送从演示中摘掉，
# 同时演示 mock 的「推送失败不重试、只记 webhook_ok=false」。
DEAD_WEBHOOK = "http://127.0.0.1:1/qqbot/callback"

_REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(
    os.path.abspath(__file__)))))   # observation/socket/qqbot/_common.py -> 仓库根
GATEWAY_BIN = os.path.join(_REPO_ROOT, "target", "debug", "top-gateway.exe")


# ---------------------------------------------------------------- 输出
def step(n, title: str) -> None:
    print(f"\n{'=' * 68}\n步骤 {n}：{title}\n{'=' * 68}")


def note(text: str) -> None:
    print(f"  注：{text}")


def show(label: str, obj) -> None:
    """单行打印 JSON 片段（中文原样）。"""
    print(f"  {label}：{json.dumps(obj, ensure_ascii=False)}")


# ---------------------------------------------------------------- HTTP 封装
class ApiError(Exception):
    def __init__(self, status: int, message: str, source: str = "mock"):
        self.status = status
        self.message = message
        super().__init__(f"[{source}] HTTP {status}: {message}")


def _request(base: str, path: str, method: str, headers: dict, obj, source: str):
    data = json.dumps(obj).encode() if obj is not None else None
    req = urllib.request.Request(base + path, method=method, data=data, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            return json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        body = e.read().decode(errors="replace")
        try:
            body = json.loads(body).get("error", body)
        except json.JSONDecodeError:
            pass
        raise ApiError(e.code, body, source) from e


def mock_api(path: str, method: str = "GET", obj: dict | None = None):
    """QQ Bot Mock 的 /_mock/* 管理端调用（自动补 /_mock 前缀）。"""
    if not path.startswith("/_mock/"):
        path = "/_mock" + path
    return _request(MOCK_BASE, path, method, {"Content-Type": "application/json"}, obj, "mock")


def gateway_api(path: str, method: str = "GET", obj: dict | None = None,
                admin_key: str = GATEWAY_ADMIN_KEY):
    """网关 /__admin/* 调用（带 X-Admin-Key）。"""
    return _request(f"http://{GATEWAY_BIND}", f"/__admin{path}", method,
                    {"X-Admin-Key": admin_key, "Content-Type": "application/json"}, obj, "gateway")


def http_request(host: str, port: int, method: str, path: str,
                 body: bytes | None = None, headers: dict | None = None) -> tuple[int, dict, bytes]:
    """单次 HTTP 请求（每次新建连接）。返回 (状态码, 响应头, 响应体)。"""
    import http.client

    conn = http.client.HTTPConnection(host, port, timeout=15)
    try:
        hdrs = dict(headers or {})
        if body is not None and "Content-Type" not in hdrs:
            hdrs["Content-Type"] = "application/json"
        conn.request(method, path, body=body, headers=hdrs)
        resp = conn.getresponse()
        return resp.status, dict(resp.getheaders()), resp.read()
    finally:
        conn.close()


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


# ---------------------------------------------------------------- 网关子进程
def _sources_newer_than(binary: str) -> bool:
    """src/ 下任一 .rs 比 binary 新（或 binary 不存在）→ 需要重新构建。"""
    if not os.path.exists(binary):
        return True
    stamp = os.path.getmtime(binary)
    for root, _, files in os.walk(os.path.join(_REPO_ROOT, "src")):
        for f in files:
            if f.endswith(".rs") and os.path.getmtime(os.path.join(root, f)) > stamp:
                return True
    return False


class Gateway:
    """top-gateway 控制台模式子进程（`run` 子命令，非 Windows 服务模式）。

    routes/providers/bots 文件放在系统临时目录，互不干扰；停止后不残留。
    """

    def __init__(self, bind: str = GATEWAY_BIND, admin_key: str = GATEWAY_ADMIN_KEY):
        self.bind = bind
        self.admin_key = admin_key
        if _sources_newer_than(GATEWAY_BIN):
            print(f"网关二进制 {GATEWAY_BIN} 缺失或比 src 新，执行 cargo build …")
            subprocess.run(["cargo", "build"], cwd=_REPO_ROOT, check=True)
        tag = bind.replace(":", "-")
        self._files = {
            "GATEWAY_ROUTES_FILE": os.path.join(tempfile.gettempdir(), f"demo-qqbot-routes-{tag}.toml"),
            "GATEWAY_PROVIDERS_FILE": os.path.join(tempfile.gettempdir(), f"demo-qqbot-providers-{tag}.toml"),
            "GATEWAY_BOTS_FILE": os.path.join(tempfile.gettempdir(), f"demo-qqbot-bots-{tag}.toml"),
        }
        for f in self._files.values():
            if os.path.exists(f):
                os.remove(f)
        env = dict(os.environ)
        env.update({"GATEWAY_BIND": bind, "GATEWAY_ADMIN_KEY": admin_key, **self._files})
        # Windows 上无参数启动会尝试进入 SCM 服务模式（虚报 1063），
        # 必须显式带 `run` 走控制台模式（src/service.rs::dispatch）。
        self.proc = subprocess.Popen(
            [GATEWAY_BIN, "run"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
        )
        deadline = time.time() + 20
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
        def call(path: str, method: str = "GET", obj: dict | None = None):
            return gateway_api(path, method, obj, self.admin_key)
        return call

    def openapi(self, method: str, path: str, body: dict | None = None,
                headers: dict | None = None) -> tuple[int, dict, bytes]:
        """经网关调用平台 OpenAPI：/qqbot/{app_id}/..."""
        port = int(self.bind.rsplit(":", 1)[1])
        data = json.dumps(body).encode() if body is not None else None
        return http_request("127.0.0.1", port, method, path, data, headers)

    def callback_url(self, app_id: str = MOCK_APP_ID) -> str:
        return f"http://{self.bind}/qqbot/callback/{app_id}"

    def close(self) -> None:
        if getattr(self, "proc", None) is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()

    def __enter__(self) -> "Gateway":
        return self

    def __exit__(self, *exc) -> None:
        self.close()


# ---------------------------------------------------------------- 事件投递接收器
class _QuietHTTPServer(ThreadingHTTPServer):
    """接收器配套 server：客户端在 keep-alive 连接上断开（reqwest 连接池回收）
    会抛 ConnectionResetError——预期噪声，静默丢弃；其余异常照常打印。"""

    def handle_error(self, request, client_address):
        exc = sys.exc_info()[1]
        if isinstance(exc, (ConnectionResetError, BrokenPipeError, ConnectionAbortedError)):
            return
        super().handle_error(request, client_address)


class EventSink:
    """扮演机器人的业务后端：接收网关投递到 event_url 的事件。

    mode:
      "ok"   —— 恒 200（默认）
      "fail" —— 恒 500（演示投递失败：网关只计错误、不重试、不向平台暴露）
      "slow" —— 延迟 delay 秒再 200（演示「先 ACK 再异步投递」）
    收到的每条 POST 记录为 {"ts", "done_ts", "path", "auth", "ctype", "body"}：
    ts 是请求到达，done_ts 是响应发出的时刻（slow 模式下两者隔一个 delay）。
    """

    def __init__(self, mode: str = "ok", delay: float = 0.0, name: str = "event-sink"):
        self.mode = mode
        self.delay = delay
        self.port = free_port()
        self.url = f"http://127.0.0.1:{self.port}/events"
        self.events: list[dict] = []
        sink = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def _respond(self) -> None:
                code = 200 if sink.mode == "ok" or sink.mode == "slow" else 500
                body = b'{"ok":true}' if code == 200 else b'{"ok":false}'
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self) -> None:
                n = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(n)
                rec = {
                    "ts": time.time(),
                    "path": self.path,
                    "auth": self.headers.get("Authorization"),
                    "ctype": self.headers.get("Content-Type"),
                    "body": raw.decode("utf-8", "replace"),
                }
                sink.events.append(rec)
                if sink.mode == "slow":
                    time.sleep(sink.delay)
                rec["done_ts"] = time.time()
                self._respond()

            def do_GET(self) -> None:
                body = json.dumps({"mode": sink.mode, "events": len(sink.events)}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, fmt, *args) -> None:
                pass

        self._server = _QuietHTTPServer(("127.0.0.1", self.port), Handler)
        threading.Thread(target=self._server.serve_forever, daemon=True).start()

    def wait_for(self, count: int, timeout: float = 10.0) -> list[dict]:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if len(self.events) >= count:
                return list(self.events)
            time.sleep(0.1)
        return list(self.events)

    def done_count(self) -> int:
        """已发出响应的事件数（slow 模式下比到达数晚一个 delay）。"""
        return sum(1 for e in self.events if "done_ts" in e)

    def wait_for_done(self, count: int, timeout: float = 15.0) -> list[dict]:
        """等前 count 条事件的响应都已发出——比 wait_for 多过一个 delay。"""
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.done_count() >= count:
                return list(self.events)
            time.sleep(0.1)
        return list(self.events)

    def json_bodies(self) -> list[dict]:
        out = []
        for e in self.events:
            try:
                out.append(json.loads(e["body"]))
            except json.JSONDecodeError:
                out.append({})
        return out

    def close(self) -> None:
        self._server.shutdown()
        self._server.server_close()

    def __enter__(self) -> "EventSink":
        return self

    def __exit__(self, *exc) -> None:
        self.close()


# ---------------------------------------------------------------- mock 世界工具
def add_bot(gateway: Gateway, **overrides) -> dict:
    """注册机器人并回读校验。默认指向共享 mock，事件投递到 sink_url。"""
    body = {
        "id": "demo-bot",
        "name": "演示机器人",
        "app_id": MOCK_APP_ID,
        "app_secret": MOCK_APP_SECRET,
        "api_base": MOCK_BASE,
        "ws_url": MOCK_BASE.replace("http://", "ws://") + "/websocket",
        "intents": DEFAULT_INTENTS,
        "event_source": "webhook",
        "event_url": None,
        "event_secret": None,
        "enabled": True,
    }
    body.update(overrides)
    gateway.admin("/qqbot/bots", "POST", body)
    bots = gateway.admin("/qqbot/bots")["bots"]
    return next(b for b in bots if b["app_id"] == body["app_id"])


def dispatch_group_message(content: str = "你好，给我推荐一本书",
                           username: str = "小明", **extra) -> dict:
    """扮演用户在群里 @ 机器人发消息；返回含 records 与可被动回复的 msg_id。"""
    obj = {"group_openid": GROUP_READER, "username": username, "content": content}
    obj.update(extra)
    return mock_api("/dispatch/group_message", "POST", obj)


def token_fetch_count(limit: int = 500) -> int:
    """mock 明文调用历史中 /app/getAppAccessToken 的次数——token 缓存的观测点。"""
    history = mock_api(f"/history?limit={limit}")
    return sum(1 for r in history["records"] if r.get("path") == "/app/getAppAccessToken")


def webhook_state() -> dict:
    return mock_api("/state").get("webhook", {})


def set_webhook(url: str, secret: str = MOCK_APP_SECRET) -> dict:
    return mock_api("/webhook", "POST", {"url": url, "secret": secret})


def restore_webhook(previous: dict) -> None:
    """把 mock 的 webhook 注册还原为演示前的值（含空 url）。"""
    url = (previous or {}).get("url") or ""
    secret = (previous or {}).get("secret") or MOCK_APP_SECRET
    mock_api("/webhook", "POST", {"url": url, "secret": secret})


def validate_webhook() -> dict:
    """发起回调地址验证：mock 向回调地址 POST op 13，业务端回签。"""
    return mock_api("/webhook/validate", "POST", {})


def set_bot_joined(group_openid: str, joined: bool) -> dict:
    """机器人入群/退群（mock 管理端）。

    顺带向平台 webhook 推一条 GROUP_ADD_ROBOT / GROUP_DEL_ROBOT 事件——
    调用方需注意当时的 webhook 指向（本套演示一律先重定向或指向不可达地址）。
    """
    return mock_api(f"/groups/{group_openid}/bot", "POST", {"joined": joined})


def ws_row(bot_id: str) -> dict | None:
    """GET /__admin/qqbot/ws 中该 bot 的行；无 supervisor（未用 WS / 已停止）时为 None。"""
    bots = gateway_api("/qqbot/ws")["bots"]
    return next((s for s in bots if s["bot_id"] == bot_id), None)


def ws_status(bot_id: str | None = None) -> dict | list:
    """GET /__admin/qqbot/ws 的快照；给 bot_id 则取单条（缺失为 {}）。"""
    if bot_id is None:
        return gateway_api("/qqbot/ws")["bots"]
    return ws_row(bot_id) or {}


def wait_for_ws(bot_id: str, connected: bool = True, timeout: float = 15.0) -> dict | None:
    """等 supervisor 行进入目标连接状态；connected=False 时行消失也算满足
    （停止/删除后表内无该项，语义同「未连接」）。"""
    deadline = time.time() + timeout
    while time.time() < deadline:
        row = ws_row(bot_id)
        if connected and row and row.get("connected"):
            return row
        if not connected and (row is None or row.get("connected") is False):
            return row
        time.sleep(0.2)
    return ws_row(bot_id)


def bot_metrics(bot_id: str) -> dict:
    bots = gateway_api("/qqbot/metrics")["bots"]
    return next((b["stats"] for b in bots if b["bot"]["id"] == bot_id), {})


def check_mock_reachable() -> None:
    try:
        state = mock_api("/state")
    except (OSError, ApiError) as e:
        print(f"Mock 不可达（{MOCK_BASE}）：{e}\n请先启动 QQ Bot Mock，再运行本演示。", file=sys.stderr)
        sys.exit(2)
    bot = state.get("bot") or {}
    print(f"  mock 可达：{MOCK_BASE}（appId {bot.get('app_id')}，"
          f"bot {bot.get('username')}；webhook -> {(state.get('webhook') or {}).get('url') or '（未注册）'}）")


# ---------------------------------------------------------------- stdout 编码
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8")
    except (AttributeError, OSError):
        pass
