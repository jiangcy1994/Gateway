#!/usr/bin/env python3
"""演示四：二进制协议 —— 长度前缀帧 over HTTP 体，以及 raw TCP 直连对照。

帧格式（自定义，3 字节头）：
    [type:1][len:2 BE][payload]

两条路径：
  1. HTTP 变体（经网关）：帧序列作为 POST 请求体 / 响应体传输。
     网关剥除 content-length（hop-by-hop），上游侧可能收到 chunked 编码，
     mock 两种编码都读；响应回程同理，客户端照常解帧。
  2. raw TCP 变体（不经网关）：面板监听器直接转发 TCP 字节流，
     tx/rx 负载里没有任何 HTTP 包裹，首字节就是帧头。

面板测量：注册自定义解读器 binframe，从原始负载解帧并展示
帧数 / 消耗字节数 / 每帧 type 与 payload。
"""

import base64
import json
import os
import socket
import sys
import textwrap
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _common as C

INTERP_ID = "binframe"

INTERP_CODE = textwrap.dedent(
    """
    METHODS = (b"GET ", b"POST ", b"PUT ", b"DELETE ", b"PATCH ",
               b"HEAD ", b"OPTIONS ")

    def _strip_http(payload):
        \"\"\"面板给解读器的是整个 L7 字节流：若有 HTTP 包裹
        （请求行/状态行 + 头），定位体起点；体若是 chunked，拼回纯净体。\"\"\"
        if not (payload.startswith(b"HTTP/1.") or payload.startswith(METHODS)):
            return b"", payload
        sep = payload.find(b"\\r\\n\\r\\n")
        if sep < 0:
            return b"", payload
        head = payload[:sep]
        data = payload[sep + 4:]
        first = data.split(b"\\r\\n", 1)[0]
        try:
            size = int(first, 16)
        except ValueError:
            return head, data  # 普通 Content-Length 体
        out = []
        rest = data
        if size <= 0 or len(rest) < len(first) + 2 + size:
            return head, data
        while True:
            line, _, rest = rest.partition(b"\\r\\n")
            n = int(line, 16)
            if n == 0:
                break
            out.append(rest[:n])
            rest = rest[n + 2:]
        return head, b"".join(out)

    def interpret(payload, meta):
        head, data = _strip_http(payload)
        frames, i = [], 0
        while i + 3 <= len(data):
            t = data[i]
            ln = int.from_bytes(data[i + 1:i + 3], "big")
            if i + 3 + ln > len(data):
                break
            frames.append((t, data[i + 3:i + 3 + ln]))
            i += 3 + ln
        first_line = head.split(b"\\r\\n", 1)[0].decode("ascii", "replace") if head else "(无 HTTP 包裹)"
        rows = [
            {"label": "HTTP 首行", "value": first_line},
            {"label": "帧数", "value": str(len(frames))},
            {"label": "消耗字节", "value": "%d / %d" % (i, len(data))},
        ]
        for j, (t, p) in enumerate(frames[:6]):
            rows.append({
                "label": "帧 %d" % j,
                "value": "type=%d len=%d payload=%r" % (t, len(p), p),
            })
        return {
            "summary": "binframe: %d 帧 / %d B（%s）" % (
                len(frames), len(data), meta.get("direction", "?")),
            "rows": rows,
        }
    """
)


# ---------------------------------------------------------------- 帧编解码
def encode_frame(t: int, payload: bytes) -> bytes:
    return bytes([t]) + len(payload).to_bytes(2, "big") + payload


def decode_frames(data: bytes) -> tuple[list[tuple[int, bytes]], int]:
    """返回 (帧列表, 消耗字节数)；尾部不完整帧不计。"""
    frames, i = [], 0
    while i + 3 <= len(data):
        ln = int.from_bytes(data[i + 1:i + 3], "big")
        if i + 3 + ln > len(data):
            break
        frames.append((data[i], data[i + 3:i + 3 + ln]))
        i += 3 + ln
    return frames, i


# ---------------------------------------------------------------- HTTP 二进制 upstream
class BinaryHttpUpstream(BaseHTTPRequestHandler):
    """HTTP/1.1 upstream：把请求体里的帧逐帧 type+1 回写（mock 的「应答」）。"""

    protocol_version = "HTTP/1.1"

    def handle(self) -> None:
        try:
            super().handle()
        except (ConnectionResetError, BrokenPipeError):
            pass

    def _read_body(self) -> bytes:
        """网关剥除 content-length（hop-by-hop）后，上游侧改由 chunked 定界；两种都读。"""
        if "chunked" in (self.headers.get("Transfer-Encoding") or "").lower():
            print(f"  [upstream] 请求体编码：chunked（Content-Length 被网关剥除）")
            chunks = []
            while True:
                size = int(self.rfile.readline(65536).strip(), 16)
                if size == 0:
                    self.rfile.readline()  # 尾部 CRLF
                    break
                chunks.append(self.rfile.read(size))
                self.rfile.readline()  # 块后 CRLF
            return b"".join(chunks)
        print(f"  [upstream] 请求体编码：Content-Length={self.headers.get('Content-Length')}")
        return self.rfile.read(int(self.headers.get("Content-Length") or 0))

    def do_POST(self) -> None:
        body = self._read_body()
        frames, _ = decode_frames(body)
        resp = b"".join(encode_frame(t + 1, p) for t, p in frames)
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(resp)))
        self.end_headers()
        self.wfile.write(resp)

    def do_GET(self) -> None:
        if self.path == "/health":
            body = b'{"ok": true}'
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        body = b"POST binary frames to /bin/echo"
        self.send_response(404)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args) -> None:
        pass


# ---------------------------------------------------------------- raw TCP echo
class RawEchoServer:
    """原始 TCP 回显：收什么回什么，用于不经网关的对照路径。"""

    def __init__(self):
        self.sock = socket.socket()
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(16)
        self.port = self.sock.getsockname()[1]
        threading.Thread(target=self._serve, daemon=True).start()

    def _serve(self) -> None:
        while True:
            try:
                conn, _ = self.sock.accept()
            except OSError:
                return
            threading.Thread(target=self._handle, args=(conn,), daemon=True).start()

    @staticmethod
    def _handle(conn: socket.socket) -> None:
        with conn:
            try:
                conn.settimeout(30)
                while True:
                    data = conn.recv(4096)
                    if not data:
                        break
                    conn.sendall(data)
            except OSError:
                pass

    def shutdown(self) -> None:
        self.sock.close()


def recv_exact(sock: socket.socket, n: int, timeout: float = 5.0) -> bytes:
    sock.settimeout(timeout)
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise RuntimeError("连接提前关闭")
        buf += chunk
    return buf


def main() -> int:
    token = C.load_token()

    upstream = ThreadingHTTPServer(("127.0.0.1", C.free_port()), BinaryHttpUpstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    raw = RawEchoServer()

    with C.Gateway() as gw, \
            C.Listener(token, "127.0.0.1", int(gw.bind.rsplit(":", 1)[1]),
                       name="demo-bin-http") as ln_http, \
            C.Listener(token, "127.0.0.1", raw.port,
                       name="demo-bin-raw") as ln_raw:
        # ---------------------------------------------------------- 1
        C.step(1, "环境：二进制 HTTP upstream + raw TCP echo + 网关 + 两个面板监听器")
        print(f"  http upstream  : http://127.0.0.1:{upstream.server_address[1]}/bin/echo")
        print(f"  raw tcp echo   : 127.0.0.1:{raw.port}（不经网关）")
        print(f"  gateway        : http://{gw.bind}")
        print(f"  panel listener : 127.0.0.1:{ln_http.listen_port} -> 网关（HTTP 变体）")
        print(f"  panel listener : 127.0.0.1:{ln_raw.listen_port} -> raw echo（TCP 变体）")

        base_http = C.session_baseline(token, ln_http.listen_port)
        base_raw = C.session_baseline(token, ln_raw.listen_port)

        C.add_route(gw, "demo-bin", "/bin", f"http://127.0.0.1:{upstream.server_address[1]}")

        frames = [
            encode_frame(1, b"PING"),
            encode_frame(2, b"\x00\xff\x10\xfe-\x00 binary"),
            encode_frame(9, b""),  # 空载荷帧
        ]
        body = b"".join(frames)
        print(f"  待发帧序列：{len(frames)} 帧，{len(body)} B —— {body!r}")

        # ---------------------------------------------------------- 2
        C.step(2, "HTTP 变体：帧序列作为 POST 体经网关往返")
        status, _, data = C.http_request("127.0.0.1", ln_http.listen_port, "POST",
                                         "/bin/echo", body)
        resp_frames, consumed = decode_frames(data)
        print(f"  -> {status}，响应体 {len(data)} B，解出 {len(resp_frames)} 帧（消耗 {consumed} B）")
        for j, (t, p) in enumerate(resp_frames):
            print(f"    帧 {j}: type={t} len={len(p)} payload={p!r}")
        C.note("客户端发的 Content-Length 被网关按 hop-by-hop 剥除，上游侧改用 chunked"
               " 定界（见上方 [upstream] 输出）；帧字节本身原样穿过。")

        # ---------------------------------------------------------- 3
        C.step(3, "raw TCP 变体：面板监听器直连 echo（无 HTTP 包裹）")
        sock = socket.create_connection(("127.0.0.1", ln_raw.listen_port), timeout=5)
        try:
            sock.sendall(body)
            echo = recv_exact(sock, len(body))
        finally:
            sock.close()
        echo_frames, _ = decode_frames(echo)
        print(f"  收到回显 {len(echo)} B，解出 {len(echo_frames)} 帧 —— 与发送逐字节一致：{echo == body}")
        C.note("这条路径不经过网关，纯 TCP 字节流；面板会话负载里没有 HTTP 请求行，"
               "tx 首字节直接就是帧头 0x01。")

        # ---------------------------------------------------------- 4
        C.step(4, "面板测量：注册 binframe 解读器，看两条会话的原始负载")
        C.delete_interpreter(token, INTERP_ID)  # 上次运行可能残留
        C.register_interpreter(token, INTERP_ID, "二进制帧解帧", INTERP_CODE)

        sess_http = C.wait_for_new_sessions(token, ln_http.listen_port, base_http, 1)
        sess_raw = C.wait_for_new_sessions(token, ln_raw.listen_port, base_raw, 1)
        C.print_session_rows(sess_http + sess_raw)

        for tag, sess, ln in (("HTTP 变体", sess_http, ln_http), ("TCP 变体", sess_raw, ln_raw)):
            if not sess:
                print(f"  {tag}：未等到会话")
                continue
            det = C.session_detail(token, sess[0]["id"])
            C.interpret_session(token, sess[0]["id"])  # 清解读缓存，纳入新解读器
            det = C.session_detail(token, sess[0]["id"])
            tx = base64.b64decode(det["payload_b64"]["tx"])
            rx = base64.b64decode(det["payload_b64"]["rx"])
            print(f"\n  {tag} 会话 {det['id'][:12]}：tx 前 48 B = {tx[:48]!r}")
            print(f"  {tag} 会话 {det['id'][:12]}：rx 前 48 B = {rx[:48]!r}")
            C.print_interpret(det, "tx", only=(INTERP_ID,))
            C.print_interpret(det, "rx", only=(INTERP_ID,))

        C.delete_interpreter(token, INTERP_ID)

        # ---------------------------------------------------------- 5
        C.step(5, "metrics 对照 + 清理")
        for m in gw.admin("/metrics")["routes"]:
            print(f"  route {m['route']['id']}: {json.dumps(m['stats'], ensure_ascii=False)}")
        C.note("TCP 变体不经网关，不出现在 metrics 里；两条路径的字节都在面板会话中。")

    # -------------------------------------------------------------- 6
    C.step(6, "清理")
    upstream.shutdown()
    raw.shutdown()
    print("  网关、监听器、mock upstream、解读器已停止/删除")
    C.maybe_reset(token)
    print("\n演示四完成。")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except C.ApiError as e:
        print(f"\nAPI 调用失败：{e}", file=sys.stderr)
        sys.exit(1)
