#!/usr/bin/env python3
"""演示五：LLM 协议透传 —— OpenAI Chat Completions / Responses、Anthropic Messages。

网关行为（/llm 子网关，与路由表无关的保留前缀）：
  - 两种入站形态：显式协议段 /llm/{protocol}/...，或 /llm/v1/{端点} 由端点名
    闭表推断协议（chat/completions/responses -> openai，messages -> anthropic）；
    两协议共用的端点（models、files）与未知端点 -> 404，提示改用显式协议段
  - 按协议选供应商：供应商声明 protocols（可多选，一份配额跨协议共用）；
    未知协议段 -> 404，协议池空 -> 503，池内全部供应商配额权重 <= 0
    （RPM 或 TPM 耗尽）-> 429
  - 凭证替换：客户端的 Authorization / x-api-key 一律被剥除，换成供应商的 key
    （openai 注入 Bearer，anthropic 注入 x-api-key + anthropic-version）
  - 路径原样透传：/llm/openai/v1/chat/completions 与 /llm/openai/v1/responses
    只是同一协议段下的不同剩余路径；/llm/v1 形态下转发路径恒为 /v1/...
  - 响应流式透传的同时，尾包提取 usage 记 TPM（非流式 JSON 取 usage 对象，
    SSE 取 data 块），可在 /__admin/llm/quota 对照

upstream mock 协议：
  - OpenAI Chat Completions（非流式 JSON / 流式 SSE，usage.total_tokens）
  - OpenAI Responses（非流式 JSON，usage.input_tokens + output_tokens）
  - Anthropic Messages（非流式 JSON / 流式 SSE，message_start 给 input_tokens、
    message_delta 给累计 output_tokens）
"""

import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# _common.py 在 socket 根目录（observation/socket/_common.py），取上级目录。
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import _common as C

# mock 收到的凭证（证明客户端凭证被网关替换为供应商凭证）
SEEN_AUTH: dict[str, dict] = {}

# 每次响应让 usage 数值可辨认识别（同时作为 TPM 对照的期望值）
USAGE_CC = 58        # chat completions，total_tokens
USAGE_RS = 43        # responses，input 25 + output 18
USAGE_MS_IN, USAGE_MS_OUT = 9, 7   # anthropic messages SSE


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
        self._json(404, {"error": {"code": 404, "message": "mock only serves POST LLM endpoints"}})

    def log_message(self, fmt, *args) -> None:
        pass


def main() -> int:
    token = C.load_token()

    upstream = ThreadingHTTPServer(("127.0.0.1", C.free_port()), LlmMockUpstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    with C.Gateway() as gw, C.Listener(token, "127.0.0.1", int(gw.bind.rsplit(":", 1)[1]),
                                       name="demo-llm") as ln:
        # ---------------------------------------------------------- 1
        C.step(1, "环境：三端点 LLM mock upstream + 网关 + 面板监听器")
        print(f"  mock upstream : http://127.0.0.1:{upstream.server_address[1]}"
              "（/v1/chat/completions、/v1/responses、/v1/messages）")
        print(f"  gateway       : http://{gw.bind}（/llm 为保留前缀，不经路由表）")
        print(f"  panel listener: 127.0.0.1:{ln.listen_port} -> {ln.obj['target']}")
        baseline = C.session_baseline(token, ln.listen_port)

        # ---------------------------------------------------------- 2
        C.step(2, "挂两个协议的 supplier（POST /__admin/llm/providers）")
        for pid, proto, key in (
            ("demo-openai", "openai", "sk-mock-openai-key"),
            ("demo-anthropic", "anthropic", "sk-mock-anthropic-key"),
        ):
            gw.admin(f"/llm/providers", "POST", {
                "id": pid, "protocols": [proto],
                "base_url": f"http://127.0.0.1:{upstream.server_address[1]}",
                "api_key": key, "rpm_limit": 60, "tpm_limit": 100000,
            })
        print(json.dumps(gw.admin("/llm/providers")["providers"], ensure_ascii=False, indent=2))
        C.note("protocols 是数组：一个供应商可同时声明多个协议（共享一份配额），"
               "此处各挂单协议；路由表里不需要任何条目——/llm 是保留前缀，"
               "协议段直接选供应商池。")

        # ---------------------------------------------------------- 3
        C.step(3, "三个端点经网关请求（客户端一律带假凭证）")
        import http.client

        def llm(tag: str, path: str, obj: dict, sse: bool = False):
            conn = http.client.HTTPConnection("127.0.0.1", ln.listen_port, timeout=15)
            # 客户端假凭证：openai 走 Authorization，anthropic 走 x-api-key
            hdrs = {"Content-Type": "application/json"}
            if "/anthropic/" in path:
                hdrs["x-api-key"] = "sk-ant-client-fake"
            else:
                hdrs["Authorization"] = "Bearer sk-client-fake"
            t0 = time.time()
            conn.request("POST", path, body=json.dumps(obj).encode(), headers=hdrs)
            resp = conn.getresponse()
            provider = resp.getheader("x-llm-provider")
            if sse:
                # 逐行读（SSE 每事件以 \n\n 结尾）：read(512) 会阻塞到收满或 EOF，
                # 看不出流式；readline 来一行给一行，间隔即上游的发出节奏。
                arrivals = []
                while line := resp.readline():
                    arrivals.append((time.time() - t0, line))
                conn.close()
                gaps = ([round(arrivals[i + 1][0] - arrivals[i][0], 2)
                         for i in range(len(arrivals) - 1)] if arrivals else [])
                print(f"  {tag:16s} POST {path:34s} -> {resp.status}"
                      f" provider={provider} SSE {len(arrivals)} 行 / {sum(len(b) for _, b in arrivals)} B"
                      f" 行间隔={gaps}")
                return arrivals
            data = resp.read()
            conn.close()
            usage = json.loads(data).get("usage")
            seen = SEEN_AUTH.get("/" + path.split("/llm/", 1)[1].split("/", 1)[1], {})
            print(f"  {tag:16s} POST {path:34s} -> {resp.status} provider={provider}"
                  f" usage={usage} 耗时 {time.time() - t0:.2f}s")
            print(f"  {'':16s}   mock 实收凭证：{ {k: v for k, v in seen.items() if v} }")
            return None

        llm("Chat Completions", "/llm/openai/v1/chat/completions",
            {"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "hi"}]})
        llm("Responses", "/llm/openai/v1/responses",
            {"model": "gpt-4o-mini", "input": "hi"})
        llm("Messages", "/llm/anthropic/v1/messages",
            {"model": "claude-3-5-sonnet-20241022", "max_tokens": 64,
             "messages": [{"role": "user", "content": "hi"}]})

        cc_sse = llm("Chat 流式", "/llm/openai/v1/chat/completions",
                     {"model": "gpt-4o-mini", "stream": True,
                      "messages": [{"role": "user", "content": "hi"}]}, sse=True)
        llm("Messages 流式", "/llm/anthropic/v1/messages",
            {"model": "claude-3-5-sonnet-20241022", "max_tokens": 64, "stream": True,
             "messages": [{"role": "user", "content": "hi"}]}, sse=True)

        C.note("客户端发的是假凭证（sk-client-fake / sk-ant-client-fake），"
               "mock 实收的已被网关替换为供应商 key；anthropic 侧还被补了默认 "
               "anthropic-version。两者皆可在面板会话的 tx 负载里看到原样假凭证。")

        # ---------------------------------------------------------- 4
        C.step(4, "协议池语义：未知协议段 404、禁用该协议全部供应商 -> 503")
        status, _, data = C.http_request("127.0.0.1", ln.listen_port, "POST",
                                         "/llm/unknown/v1/chat/completions",
                                         b"{}", {"Content-Type": "application/json"})
        print(f"  未知协议段  -> {status} {data[:80]!r}")
        gw.admin("/llm/providers/demo-anthropic", "PUT", {"enabled": False})
        status, _, data = C.http_request("127.0.0.1", ln.listen_port, "POST",
                                         "/llm/anthropic/v1/messages",
                                         b"{}", {"Content-Type": "application/json"})
        print(f"  anthropic 池全禁 -> {status} {data[:80]!r}")
        gw.admin("/llm/providers/demo-anthropic", "PUT", {"enabled": True})

        # ---------------------------------------------------------- 5
        C.step(5, "面板测量：SSE 会话的逐块到达")
        sessions = C.wait_for_new_sessions(token, ln.listen_port, baseline,
                                           7, timeout=15)
        C.print_session_rows(sessions)
        sse_session = next((s for s in sessions if s["bytes_rx"] > 400
                            and s["bytes_rx"] < 900), None)
        if sse_session and cc_sse:
            det = C.session_detail(token, sse_session["id"])
            C.print_interpret(det, "tx", only=("http",))
            C.print_interpret(det, "rx", only=("text",))

        # ---------------------------------------------------------- 6
        C.step(6, "网关侧对照：metrics（请求数）与 quota（RPM/TPM）")
        for p in gw.admin("/llm/metrics")["providers"]:
            if p["provider"]["id"].startswith("demo-"):
                print(f"  provider {p['provider']['id']}: {json.dumps(p['stats'], ensure_ascii=False)}")
        print("  quota（TPM 由响应尾包 usage 提取，60s 滚动窗口）：")
        for q in gw.admin("/llm/quota")["providers"]:
            if q["id"].startswith("demo-"):
                print(f"    {q['id']:16s} rpm used={q['rpm']['used']}/{q['rpm']['limit']}"
                      f"  tpm used={q['tpm']['used']}/{q['tpm']['limit']}")
        C.note(f"对照：openai 池 TPM 应为 {USAGE_CC}（Chat 非流式）+ {USAGE_RS}"
               f"（Responses）+ {USAGE_CC}（流式 SSE 尾包）"
               f"= {USAGE_CC * 2 + USAGE_RS}；anthropic 池应为 {USAGE_MS_IN + USAGE_MS_OUT}"
               f"（非流式 input+output）+ {USAGE_MS_IN + USAGE_MS_OUT}"
               f"（SSE：首 input + 末 output）= {(USAGE_MS_IN + USAGE_MS_OUT) * 2}。")

        # ---------------------------------------------------------- 7
        C.step(7, "v1 端点推断与共享供应商（/llm/v1/{端点}）")
        # /llm/v1/{端点}：端点名经闭表推断协议，SDK 可直接指 base_url=/llm/v1。
        for tag, path, obj in (
            ("Chat(v1 推断)", "/llm/v1/chat/completions",
             {"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "hi"}]}),
            ("Messages(v1 推断)", "/llm/v1/messages",
             {"model": "claude-3-5-sonnet-20241022", "max_tokens": 64,
              "messages": [{"role": "user", "content": "hi"}]}),
        ):
            status, hdrs, data = C.http_request(
                "127.0.0.1", ln.listen_port, "POST", path,
                json.dumps(obj).encode(), {"Content-Type": "application/json"})
            print(f"  {tag:18s} POST {path:24s} -> {status}"
                  f" provider={hdrs.get('x-llm-provider')}")
        for path in ("/llm/v1/models", "/llm/v1/wat"):
            status, _, data = C.http_request("127.0.0.1", ln.listen_port, "POST",
                                             path, b"{}", {"Content-Type": "application/json"})
            print(f"  {'':18s} POST {path:24s} -> {status} {data[:76]!r}")
        C.note("v1 推断是闭表：models/files 两协议都有 -> 404 提示显式协议段，"
               "未知端点 -> 404；systemone 不参与推断（只能显式段访问）。"
               "推断只决定选池与凭证注入，转发路径恒为 /v1/...。")

        # 一个供应商同时侍奉两个协议：protocols 多选，配额只记一份。
        gw.admin("/llm/providers", "POST", {
            "id": "demo-shared", "protocols": ["openai", "anthropic"],
            "base_url": f"http://127.0.0.1:{upstream.server_address[1]}",
            "api_key": "sk-mock-shared", "rpm_limit": 60, "tpm_limit": 100000,
        })
        for pid in ("demo-openai", "demo-anthropic"):
            gw.admin(f"/llm/providers/{pid}", "PUT", {"enabled": False})
        for tag, path in (
            ("openai 端点", "/llm/v1/chat/completions"),
            ("anthropic 端点", "/llm/v1/messages"),
        ):
            status, hdrs, data = C.http_request(
                "127.0.0.1", ln.listen_port, "POST", path,
                b'{"model":"x","max_tokens":8,"messages":[]}',
                {"Content-Type": "application/json"})
            # mock 按上游路径记账：/llm/v1/chat/completions -> /v1/chat/completions
            seen = SEEN_AUTH.get("/v1/" + path.split("/v1/")[-1], {})
            cred = seen.get("x-api-key") or seen.get("authorization")
            print(f"  共享供应商 {tag:12s} POST {path:22s} -> {status}"
                  f" provider={hdrs.get('x-llm-provider')} mock 实收凭证={cred}")
        q = next(x for x in gw.admin("/llm/quota")["providers"] if x["id"] == "demo-shared")
        print(f"    demo-shared quota: protocols={q['protocols']}"
              f" rpm used={q['rpm']['used']}/{q['rpm']['limit']}"
              f" tpm used={q['tpm']['used']}/{q['tpm']['limit']}")
        for pid in ("demo-openai", "demo-anthropic"):
            gw.admin(f"/llm/providers/{pid}", "PUT", {"enabled": True})
        C.note("demo-shared 声明两个协议：openai 端点请求注入 Bearer、anthropic "
               "端点请求注入 x-api-key，两者共用同一份 RPM/TPM 计数——"
               "quota 视图里只有一条记录，protocols 是数组。")

        # ---------------------------------------------------------- 8
        C.step(8, "配额极限：RPM 耗尽 -> 429、TPM 超限 -> 429")
        # RPM：systemone 的 usage 提取是 stub（恒 0，见 src/llm/usage.rs），
        # TPM 不会累积，正好 isolates RPM 这一条维度。
        gw.admin("/llm/providers", "POST", {
            "id": "demo-tiny-rpm", "protocols": ["systemone"],
            "base_url": f"http://127.0.0.1:{upstream.server_address[1]}",
            "api_key": "sk-mock-tiny", "rpm_limit": 1, "tpm_limit": 1000,
        })
        for i in (1, 2):
            status, _, data = C.http_request("127.0.0.1", ln.listen_port, "POST",
                                             "/llm/systemone/v1/chat/completions",
                                             b'{"model":"x"}', {"Content-Type": "application/json"})
            print(f"  RPM 第 {i} 次 -> {status} {data[:72]!r}")
        q = next(x for x in gw.admin("/llm/quota")["providers"] if x["id"] == "demo-tiny-rpm")
        print(f"    demo-tiny-rpm quota: rpm {q['rpm']['used']}/{q['rpm']['limit']}"
              f"（remaining={q['rpm']['remaining']}），tpm {q['tpm']['used']}/{q['tpm']['limit']}")

        # TPM：anthropic 协议挂一个 tpm_limit=10 的小供应商，并临时禁用
        # demo-anthropic 与 demo-shared，让该协议池只剩它一家
        # （否则加权选择会绕开它）。
        gw.admin("/llm/providers", "POST", {
            "id": "demo-tiny-tpm", "protocols": ["anthropic"],
            "base_url": f"http://127.0.0.1:{upstream.server_address[1]}",
            "api_key": "sk-mock-tiny", "rpm_limit": 100, "tpm_limit": 10,
        })
        for pid in ("demo-anthropic", "demo-shared"):
            gw.admin(f"/llm/providers/{pid}", "PUT", {"enabled": False})
        for i in (1, 2):
            status, _, data = C.http_request("127.0.0.1", ln.listen_port, "POST",
                                             "/llm/anthropic/v1/messages",
                                             b'{"model":"x","max_tokens":8}',
                                             {"Content-Type": "application/json"})
            print(f"  TPM 第 {i} 次 -> {status} {data[:72]!r}")
        q = next(x for x in gw.admin("/llm/quota")["providers"] if x["id"] == "demo-tiny-tpm")
        print(f"    demo-tiny-tpm quota: rpm {q['rpm']['used']}/{q['rpm']['limit']}"
              f"，tpm {q['tpm']['used']}/{q['tpm']['limit']}"
              f"（remaining={q['tpm']['remaining']}，单次响应就用了 "
              f"{USAGE_MS_IN + USAGE_MS_OUT} > 10）")
        gw.admin("/llm/providers/demo-anthropic", "PUT", {"enabled": True})
        gw.admin("/llm/providers/demo-shared", "PUT", {"enabled": True})
        C.note("权重 = min(rpm 剩余比, tpm 剩余比)：任一维度满额该供应商权重即 0，"
               "协议池内全部供应商权重 <= 0 时返回 429（与池为空的 503 区分）。")

        # ---------------------------------------------------------- 9
        C.step(9, "清理：删供应商、停网关与监听器、停 mock")
        for pid in ("demo-openai", "demo-anthropic", "demo-shared",
                    "demo-tiny-rpm", "demo-tiny-tpm"):
            gw.admin(f"/llm/providers/{pid}", "DELETE")
        print("  供应商 demo-openai / demo-anthropic / demo-shared /"
              " demo-tiny-rpm / demo-tiny-tpm 已删除")

    upstream.shutdown()
    print("  网关、监听器、mock upstream 已停止")
    C.maybe_reset(token)
    print("\n演示五完成。")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except C.ApiError as e:
        print(f"\nAPI 调用失败：{e}", file=sys.stderr)
        sys.exit(1)
