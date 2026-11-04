#!/usr/bin/env python3
"""LLM 演示共享的 mock upstream：一个 handler 同时说三种 LLM 协议端点。

`observation/socket/llm/` 下各演示导入本模块（`import _llm_mock as M`）：

  - `spawn()` 在随机端口拉起 mock（daemon 线程），返回 server；
  - `SEEN_AUTH` 按上游路径记 mock 实收的凭证与 anthropic-version（用来证明
    客户端凭证被网关替换为供应商凭证）；
  - `post()` 是各演示共用的请求辅助（客户端一律带假凭证），
    `upstream_seen()` 按网关转发规则推算 mock 侧的记账键。

upstream mock 协议：
  - OpenAI Chat Completions（非流式 JSON / 流式 SSE，usage.total_tokens）
  - OpenAI Responses（非流式 JSON，usage.input_tokens + output_tokens）
  - Anthropic Messages（非流式 JSON / 流式 SSE，message_start 给 input_tokens、
    message_delta 给累计 output_tokens）
  - GET /v1/models（给 demo_03 演示「显式协议段下 models 可达」的对照）
"""

import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import _common as C

# mock 收到的凭证（按上游路径记账，证明客户端凭证被网关替换为供应商凭证）
SEEN_AUTH: dict[str, dict] = {}

# 每次响应让 usage 数值可辨认识别（同时作为 TPM 对照的期望值）
USAGE_CC = 58        # chat completions，total_tokens
USAGE_RS = 43        # responses，input 25 + output 18
USAGE_MS_IN, USAGE_MS_OUT = 9, 7   # anthropic messages（input / output）


class LlmMockUpstream(BaseHTTPRequestHandler):
    """一个 mock 同时说三种 LLM 协议端点，按路径分发。"""

    protocol_version = "HTTP/1.1"

    def handle(self) -> None:
        try:
            super().handle()
        except (ConnectionResetError, BrokenPipeError):
            pass

    def _read_body(self) -> bytes:
        if "chunked" in (self.headers.get("Transfer-Encoding") or "").lower():
            chunks = []
            while True:
                size = int(self.rfile.readline(65536).strip(), 16)
                if size == 0:
                    self.rfile.readline()
                    break
                chunks.append(self.rfile.read(size))
                self.rfile.readline()
            return b"".join(chunks)
        return self.rfile.read(int(self.headers.get("Content-Length") or 0))

    def _json(self, code: int, obj: dict) -> None:
        body = json.dumps(obj, ensure_ascii=False).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _sse(self, events: list[bytes], delay: float = 0.3) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        for ev in events:
            self.wfile.write(ev)
            self.wfile.flush()
            time.sleep(delay)

    def do_POST(self) -> None:
        body = self._read_body()
        SEEN_AUTH[self.path] = {
            "authorization": self.headers.get("Authorization"),
            "x-api-key": self.headers.get("x-api-key"),
            "anthropic-version": self.headers.get("anthropic-version"),
        }
        want_stream = b'"stream":true' in body.replace(b" ", b"")

        if self.path == "/v1/chat/completions":
            if want_stream:
                self._sse([
                    b'data: {"id":"chatcmpl-mock","object":"chat.completion.chunk",'
                    b'"choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"}}]}\n\n',
                    b'data: {"choices":[{"index":0,"delta":{"content":" from mock"}}]}\n\n',
                    b'data: {"choices":[],"usage":'
                    b'{"prompt_tokens":12,"completion_tokens":8,"total_tokens":%d}}\n\n' % USAGE_CC,
                    b"data: [DONE]\n\n",
                ])
            else:
                self._json(200, {
                    "id": "chatcmpl-mock", "object": "chat.completion",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello from mock"}}],
                    "usage": {"prompt_tokens": 50, "completion_tokens": 8, "total_tokens": USAGE_CC},
                })
        elif self.path == "/v1/responses":
            self._json(200, {
                "id": "resp_mock", "object": "response", "status": "completed",
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "hi"}]}],
                "usage": {"input_tokens": 25, "output_tokens": 18, "total_tokens": USAGE_RS},
            })
        elif self.path == "/v1/messages":
            if want_stream:
                self._sse([
                    b'event: message_start\n'
                    b'data: {"type":"message_start","message":{"id":"msg_mock",'
                    b'"usage":{"input_tokens":%d,"output_tokens":1}}}\n\n' % USAGE_MS_IN,
                    b'event: content_block_delta\n'
                    b'data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}\n\n',
                    b'event: message_delta\n'
                    b'data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},'
                    b'"usage":{"output_tokens":%d}}\n\n' % USAGE_MS_OUT,
                    b'event: message_stop\ndata: {"type":"message_stop"}\n\n',
                ])
            else:
                self._json(200, {
                    "id": "msg_mock", "type": "message", "role": "assistant",
                    "content": [{"type": "text", "text": "Hello from mock"}],
                    "usage": {"input_tokens": USAGE_MS_IN, "output_tokens": USAGE_MS_OUT},
                })
        else:
            self._json(404, {"error": {"code": 404, "message": "unknown endpoint: " + self.path}})

    def do_GET(self) -> None:
        if self.path == "/health":
            self._json(200, {"ok": True})
            return
        if self.path == "/v1/models":
            self._json(200, {"object": "list", "data": [
                {"id": "mock-model", "object": "model", "owned_by": "mock"}]})
            return
        self._json(404, {"error": {"code": 404, "message":
                                  "mock only serves LLM endpoints: POST /v1/*, GET /v1/models"}})

    def log_message(self, fmt, *args) -> None:
        pass


def spawn() -> ThreadingHTTPServer:
    """在随机端口起 mock upstream（daemon 线程），返回 server。"""
    server = ThreadingHTTPServer(("127.0.0.1", C.free_port()), LlmMockUpstream)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def client_headers(path: str) -> dict:
    """客户端假凭证：按「这个端点属于哪家 SDK」选头风格——网关反正会替换。"""
    if "anthropic" in path or "/messages" in path or "/complete" in path:
        return {"Content-Type": "application/json", "x-api-key": "sk-ant-client-fake"}
    return {"Content-Type": "application/json", "Authorization": "Bearer sk-client-fake"}


def post(port: int, path: str, obj: dict | None = None, *,
         body: bytes | None = None, sse: bool = False):
    """经面板监听器 POST 一次（每次新建连接，便于面板按 TCP 连接计量会话）。

    客户端一律带假凭证。sse=True 时逐行读响应（SSE 每事件以 \\n\\n 结尾：
    read(512) 会阻塞到收满或 EOF，看不出流式；readline 来一行给一行，
    间隔即上游的发出节奏），返回 (status, provider, arrivals, elapsed)，
    arrivals = [(相对秒, 行字节)]；否则返回 (status, provider, body, elapsed)，
    elapsed 为端到端耗时（秒），供各演示打印。
    """
    import http.client

    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
    payload = body if body is not None else json.dumps(obj or {}).encode()
    t0 = time.time()
    conn.request("POST", path, body=payload, headers=client_headers(path))
    resp = conn.getresponse()
    provider = resp.getheader("x-llm-provider")
    if sse:
        arrivals = []
        while line := resp.readline():
            arrivals.append((time.time() - t0, line))
        elapsed = arrivals[-1][0] if arrivals else time.time() - t0
        conn.close()
        return resp.status, provider, arrivals, elapsed
    data = resp.read()
    elapsed = time.time() - t0
    conn.close()
    return resp.status, provider, data, elapsed


def upstream_seen(client_path: str) -> dict:
    """按网关的转发规则推算 SEEN_AUTH 的记账键（mock 侧 self.path，含 query）：

    /llm/{protocol}/v1/... 剥协议段得 /v1/...；/llm/v1/... 原样（大小写保留）。
    """
    rest = client_path.split("/llm/", 1)[1]
    if rest.split("/", 1)[0].lower() in ("openai", "anthropic", "systemone"):
        rest = rest.split("/", 1)[1]
    return SEEN_AUTH.get("/" + rest, {})


def seen_credential(seen: dict) -> str | None:
    """mock 实收的供应商凭证（按协议 injected：anthropic 是 x-api-key，openai 是 Bearer）。"""
    return seen.get("x-api-key") or seen.get("authorization")
