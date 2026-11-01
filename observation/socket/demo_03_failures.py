#!/usr/bin/env python3
"""演示三：失败路径 —— 三类 upstream mock 制造 502 / 504 / 上游 500。

网关行为：
  - 502：upstream 端口无监听（本机 loopback 拒绝连接约需 2s，属机器特性）
  - 504：upstream 接受连接但永不回包，路由 timeout_secs=2 提前掐断
  - 上游 500：upstream 直接返回 500，网关透传状态并计入 errors
三者的 metrics 计数与面板会话签名各不相同。

面板测量：失败会话同样有 tx 请求行与 rx 状态行，错误路径一览无余。
"""

import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _common as C


class HangingHandler(BaseHTTPRequestHandler):
    """接受连接后挂起：永远不会回包。"""

    protocol_version = "HTTP/1.1"

    def handle(self) -> None:
        try:
            self.rfile.read(65536)
        except (ConnectionResetError, BrokenPipeError, OSError):
            pass
        time.sleep(600)

    def log_message(self, fmt, *args) -> None:
        pass


class Error500Handler(BaseHTTPRequestHandler):
    """永远返回 500 的 HTTP/1.1 upstream。"""

    protocol_version = "HTTP/1.1"

    def handle(self) -> None:
        try:
            super().handle()
        except (ConnectionResetError, BrokenPipeError):
            pass

    def do_GET(self) -> None:
        if self.path == "/health":
            body = b'{"ok": true}'
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        body = b'{"error":{"code":500,"message":"upstream boom"}}'
        self.send_response(500)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args) -> None:
        pass


def main() -> int:
    token = C.load_token()

    hang_server = ThreadingHTTPServer(("127.0.0.1", C.free_port()), HangingHandler)
    threading.Thread(target=hang_server.serve_forever, daemon=True).start()
    err_server = ThreadingHTTPServer(("127.0.0.1", C.free_port()), Error500Handler)
    threading.Thread(target=err_server.serve_forever, daemon=True).start()
    dead_port = C.free_port()  # 仅占位取端口号，随后无监听 -> 连接被拒绝

    with C.Gateway() as gw, C.Listener(token, "127.0.0.1", int(gw.bind.rsplit(":", 1)[1]),
                                       name="demo-fail") as ln:
        C.step(1, "环境：三类失败 upstream mock")
        print(f"  hang upstream  : 127.0.0.1:{hang_server.server_address[1]}（接受连接不回包）")
        print(f"  500 upstream   : 127.0.0.1:{err_server.server_address[1]}（恒 500）")
        print(f"  dead upstream  : 127.0.0.1:{dead_port}（无监听 -> 连接拒绝）")
        print(f"  gateway        : http://{gw.bind}")
        print(f"  panel listener : 127.0.0.1:{ln.listen_port} -> {ln.obj['target']}")
        baseline = C.session_baseline(token, ln.listen_port)

        hang = hang_server.server_address[1]
        err = err_server.server_address[1]
        C.add_route(gw, "r-dead", "/dead", f"http://127.0.0.1:{dead_port}")
        C.add_route(gw, "r-hang", "/hang", f"http://127.0.0.1:{hang}", timeout_secs=2)
        C.add_route(gw, "r-500", "/boom", f"http://127.0.0.1:{err}")

        # ---------------------------------------------------------- 2
        C.step(2, "逐路请求，记录耗时")
        cases = []

        def hit(tag: str, path: str):
            t0 = time.time()
            status, _, _ = C.http_request("127.0.0.1", ln.listen_port, "GET", path)
            cases.append((tag, status))
            print(f"  {tag:12s} GET {path:12s} -> {status}  耗时 {time.time() - t0:.2f}s")

        hit("上游 500", "/boom/x")
        hit("超时 504", "/hang/x")
        hit("不可达 502", "/dead/x")
        C.note("本机 loopback 连接拒绝约需 2s，因此 502 的耗时≈2s；"
               "504 由路由 timeout_secs=2 决定，两者延迟来源不同。")

        # ---------------------------------------------------------- 3
        C.step(3, "面板测量：三条失败会话")
        sessions = C.wait_for_new_sessions(token, ln.listen_port, baseline, len(cases))
        C.print_session_rows(sessions)
        for s in sessions:
            det = C.session_detail(token, s["id"])
            rx_first = __import__("base64").b64decode(det["payload_b64"]["rx"]).splitlines()[0:1]
            print(f"  会话 {det['id'][:12]}: rx 首行 {rx_first}")

        # ---------------------------------------------------------- 4
        C.step(4, "metrics 对照：三条路径都计入 errors")
        for m in gw.admin("/metrics")["routes"]:
            print(f"  route {m['route']['id']}: {json.dumps(m['stats'], ensure_ascii=False)}")
        total_errors = sum(m["stats"]["errors"] for m in gw.admin("/metrics")["routes"])
        print(f"  errors 合计 = {total_errors}（500 透传 + 504 超时 + 502 不可达）")

    # -------------------------------------------------------------- 5
    C.step(5, "清理")
    hang_server.shutdown()
    err_server.shutdown()
    print("  网关、监听器、mock upstream 已停止/删除")
    C.maybe_reset(token)
    print("\n演示三完成。")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except C.ApiError as e:
        print(f"\nAPI 调用失败：{e}", file=sys.stderr)
        sys.exit(1)
