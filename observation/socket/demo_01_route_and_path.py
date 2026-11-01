#!/usr/bin/env python3
"""演示一：路由与路径行为 —— HTTP/1.1 keep-alive JSON upstream mock。

网关行为：
  - 最长前缀匹配（/demo 与 /demo/sub 两层路由）
  - strip_prefix 开/关：/demo/hello -> upstream /hello 或 /demo/hello
  - query string 透传
  - 路由级 headers 注入（客户端头被覆盖）
  - 上游 404 透传、禁用路由 503

面板测量：每条会话的 tx（客户端请求行）/rx（状态行）由内置 http/text 解读器还原。
"""

import json
import os
import sys
import threading
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _common as C


class HttpJsonUpstream(BaseHTTPRequestHandler):
    """HTTP/1.1 keep-alive JSON mock：echo 方法/路径/查询/请求头/请求体。"""

    protocol_version = "HTTP/1.1"

    def handle(self) -> None:
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

    def _report(self, code: int = 200) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length)
        self._json(code, {
            "method": self.command,
            "path": self.path,
            "client_path_seen": self.path,
            "injected_header": self.headers.get("x-demo-injected"),
            "body_len": len(body),
        })

    def do_GET(self) -> None:
        if self.path == "/health":
            self._json(200, {"ok": True})
        elif self.path.startswith("/nope"):
            self._json(404, {"error": {"code": 404, "message": "not found: " + self.path}})
        else:
            self._report()

    do_POST = do_PUT = do_DELETE = _report

    def log_message(self, fmt, *args) -> None:
        pass


def main() -> int:
    token = C.load_token()
    upstream = ThreadingHTTPServer(("127.0.0.1", C.free_port()), HttpJsonUpstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    with C.Gateway() as gw, C.Listener(token, "127.0.0.1", int(gw.bind.rsplit(":", 1)[1]),
                                       name="demo-http") as ln:
        # ---------------------------------------------------------- 1
        C.step(1, "环境：HTTP/1.1 keep-alive upstream + 网关 + 面板监听器")
        print(f"  mock upstream : http://127.0.0.1:{upstream.server_address[1]}/")
        print(f"  gateway       : http://{gw.bind}")
        print(f"  panel listener: 127.0.0.1:{ln.listen_port} -> {ln.obj['target']}")

        # ---------------------------------------------------------- 2
        C.step(2, "挂两条前缀路由 /demo 与 /demo/sub（最长前缀匹配）")
        C.add_route(gw, "demo", "/demo", f"http://127.0.0.1:{upstream.server_address[1]}",
                    headers={"x-demo-injected": "from-route"})
        C.add_route(gw, "demo-sub", "/demo/sub", f"http://127.0.0.1:{upstream.server_address[1]}",
                    headers={"x-demo-injected": "from-sub-route"})
        print(json.dumps(gw.admin("/routes")["routes"], ensure_ascii=False, indent=2))

        # ---------------------------------------------------------- 3
        C.step(3, "经监听器端口产生流量")
        baseline = C.session_baseline(token, ln.listen_port)
        cases = []

        def hit(tag: str, method: str, path: str, body: bytes | None = None, hdrs: dict | None = None):
            # http.client 要求请求行为 ASCII，非 ASCII 查询参数先百分号编码
            if "?" in path:
                base, _, query = path.partition("?")
                path = base + "?" + urllib.parse.quote(query, safe="=&+%")
            status, _, data = C.http_request("127.0.0.1", ln.listen_port, method, path, body, hdrs)
            cases.append((tag, path, status))
            obj = json.loads(data)
            seen = obj.get("client_path_seen")
            print(f"  {tag:14s} {method:6s} {path:28s} -> {status}"
                  f"  upstream 看到路径 {seen!r}"
                  f"{' 注入头=' + repr(obj.get('injected_header')) if 'injected_header' in obj else ''}")

        hit("strip 开", "GET", "/demo/hello?a=1&b=两")
        hit("子路由命中", "GET", "/demo/sub/deep")
        hit("POST 带体", "POST", "/demo/echo", b'{"m": "hi"}')
        hit("上游 404", "GET", "/demo/nope/x")
        hit("客户端头被覆盖", "GET", "/demo/hello", None, {"x-demo-injected": "from-client"})

        gw.admin("/routes/demo", "PUT", {"enabled": False})
        hit("禁用 -> 503", "GET", "/demo/hello")
        gw.admin("/routes/demo", "PUT", {"enabled": True})

        hit("无路由 -> 404", "GET", "/other")

        # strip_prefix=false 对照：/keep/x 的完整路径转发到 upstream
        gw.admin("/routes/demo", "PUT", {"strip_prefix": False})
        hit("strip 关", "GET", "/demo/hello2")

        expected = len(cases)

        # ---------------------------------------------------------- 4
        C.step(4, "面板测量：本监听器会话（只看本次新增）")
        sessions = C.wait_for_new_sessions(token, ln.listen_port, baseline, len(cases))
        if len(sessions) < len(cases):
            print(f"  警告：只等到 {len(sessions)}/{len(cases)} 条新会话")
        C.print_session_rows(sessions)

        # ---------------------------------------------------------- 5
        C.step(5, "会话详情：http 解读器还原请求/响应")
        for s in sessions[:3]:
            det = C.session_detail(token, s["id"])
            print(f"\n  会话 {det['id'][:12]}：")
            C.print_interpret(det, "tx", only=("http",))
            C.print_interpret(det, "rx", only=("http",))

        # ---------------------------------------------------------- 6
        C.step(6, "metrics 对照")
        for m in gw.admin("/metrics")["routes"]:
            print(f"  route {m['route']['id']}: {json.dumps(m['stats'], ensure_ascii=False)}")
        C.note("503（禁用）/404（无路由）在 proxy 早期返回，不计入路由 metrics；"
               "面板按 TCP 字节计量两者都能看到。")

    # -------------------------------------------------------------- 7
    C.step(7, "清理")
    upstream.shutdown()
    print("  网关、监听器、mock upstream 已停止/删除")
    C.maybe_reset(token)
    print("\n演示一完成。")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except C.ApiError as e:
        print(f"\nAPI 调用失败：{e}", file=sys.stderr)
        sys.exit(1)
