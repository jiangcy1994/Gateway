#!/usr/bin/env python3
"""演示二：流式转发 —— chunked upstream mock（无 Content-Length）。

网关行为：
  - 响应体流式透传，不缓冲：上游分块、有间隔地写，客户端应逐块及时收到
  - 大响应体（2 MiB）完整到达，total bytes 不受面板 1 MiB 捕获上限影响

面板测量：
  - 每块到达的时间戳证明「流式」而非「先收齐再转发」
  - 超过捕获上限时会话标记 truncated，captured_* 计数与总字节计数分离
"""

import json
import os
import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _common as C

CHUNK = b"streaming-chunk-" * 64           # 1 KiB 块
N_CHUNKS_SMALL = 5                          # 小响应：5 块、每块间隔 0.4s
N_CHUNKS_BIG = 2048                         # 大响应：2 MiB


class ChunkedUpstream(BaseHTTPRequestHandler):
    """chunked 流式 mock：不带 Content-Length，逐块写入。"""

    protocol_version = "HTTP/1.1"

    def handle(self) -> None:
        try:
            super().handle()
        except (ConnectionResetError, BrokenPipeError):
            pass

    def _start(self) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

    def _write_chunk(self, data: bytes) -> None:
        self.wfile.write(b"%x\r\n" % len(data))
        self.wfile.write(data)
        self.wfile.write(b"\r\n")

    def _end(self) -> None:
        self.wfile.write(b"0\r\n\r\n")

    def do_GET(self) -> None:
        if self.path == "/health":
            body = b'{"ok": true}'
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        if self.path == "/slow":
            self._start()
            for i in range(N_CHUNKS_SMALL):
                self._write_chunk(b"[%d] %s" % (i, CHUNK))
                time.sleep(0.4)  # 有间隔地写，客户端若逐块收到即证明流式
            self._end()
        elif self.path == "/big":
            self._start()
            for _ in range(N_CHUNKS_BIG):
                self._write_chunk(CHUNK)
            self._end()
        else:
            body = b"unknown path"
            self.send_response(404)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    def log_message(self, fmt, *args) -> None:
        pass


def main() -> int:
    token = C.load_token()
    upstream = ThreadingHTTPServer(("127.0.0.1", C.free_port()), ChunkedUpstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    with C.Gateway() as gw, C.Listener(token, "127.0.0.1", int(gw.bind.rsplit(":", 1)[1]),
                                       name="demo-stream") as ln:
        C.step(1, "环境：chunked upstream + 网关 + 面板监听器")
        print(f"  mock upstream : http://127.0.0.1:{upstream.server_address[1]}/slow, /big")
        print(f"  gateway       : http://{gw.bind}")
        print(f"  panel listener: 127.0.0.1:{ln.listen_port} -> {ln.obj['target']}")
        baseline = C.session_baseline(token, ln.listen_port)

        C.add_route(gw, "demo", "/demo", f"http://127.0.0.1:{upstream.server_address[1]}")

        # ---------------------------------------------------------- 2
        C.step(2, "/demo/slow：5 块 × 0.4s —— 客户端逐块读取并打时间戳")
        import http.client

        conn = http.client.HTTPConnection("127.0.0.1", ln.listen_port, timeout=15)
        t0 = time.time()
        conn.request("GET", "/demo/slow")
        resp = conn.getresponse()
        arrivals = []
        total = 0
        while chunk := resp.read(1024):
            arrivals.append((time.time() - t0, len(chunk)))
            total += len(chunk)
        conn.close()
        for i, (t, n) in enumerate(arrivals):
            print(f"    块 {i}: t={t:5.2f}s  {n} B")
        print(f"  共 {total} B，{len(arrivals)} 次读取")
        gaps = [arrivals[i + 1][0] - arrivals[i][0] for i in range(len(arrivals) - 1)]
        C.note("若网关先缓冲再转发，客户端会在最后一次性收到全部块（间隔≈0）。"
               f"实测相邻到达间隔：{[f'{g:.2f}s' for g in gaps]}")

        # ---------------------------------------------------------- 3
        C.step(3, "/demo/big：2 MiB chunked —— 大体流式完整性")
        status, headers, data = C.http_request("127.0.0.1", ln.listen_port, "GET", "/demo/big")
        expected_bytes = N_CHUNKS_BIG * (len(CHUNK) + len(b"%x\r\n" % len(CHUNK)) + 2)
        print(f"  客户端收到 {len(data)} B，状态 {status}，Transfer-Encoding 头被网关按 hop-by-hop 剥除"
              f"（客户端侧 {[h for h in headers if 'transfer-encoding' in h[0]] or '无'}）")
        print(f"  响应链路上的 chunked 开销合计约 {expected_bytes} B")

        # ---------------------------------------------------------- 4
        C.step(4, "面板测量：两条新会话")
        sessions = C.wait_for_new_sessions(token, ln.listen_port, baseline, 2)
        C.print_session_rows(sessions)
        for s in sessions:
            det = C.session_detail(token, s["id"])
            print(f"\n  会话 {det['id'][:12]}（{'/big' if det['bytes_rx'] > 100000 else '/slow'}）"
                  f" tx={det['bytes_tx']} B rx={det['bytes_rx']} B")
            print(f"    captured_tx={det.get('captured_tx')} captured_rx={det.get('captured_rx')}"
                  f" truncated={det.get('truncated')} capture_limit={det.get('capture_limit')}")
            C.print_interpret(det, "rx", only=("text",))

        # ---------------------------------------------------------- 5
        C.step(5, "metrics 对照")
        for m in gw.admin("/metrics")["routes"]:
            print(f"  route {m['route']['id']}: {json.dumps(m['stats'], ensure_ascii=False)}")

    # -------------------------------------------------------------- 6
    C.step(6, "清理")
    upstream.shutdown()
    print("  网关、监听器、mock upstream 已停止/删除")
    C.maybe_reset(token)
    print("\n演示二完成。")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except C.ApiError as e:
        print(f"\nAPI 调用失败：{e}", file=sys.stderr)
        sys.exit(1)
