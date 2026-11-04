#!/usr/bin/env python3
"""演示零：LLM 协议透传总览 —— 显式协议段选池 + 凭证替换的两侧对照。

网关行为（/llm 子网关，与路由表无关的保留前缀）：
  - 显式协议段 /llm/{protocol}/... 直接定供应商池：未知协议段 -> 404，
    该协议全部供应商禁用 -> 503
  - 三种 LLM 协议端点原样透传：OpenAI Chat Completions / Responses、
    Anthropic Messages（均为非流式 JSON；流式见 demo_01）
  - 凭证替换：客户端的 Authorization / x-api-key 一律被剥除，换成供应商的
    key（openai 注入 Bearer，anthropic 注入 x-api-key + anthropic-version）

观察的本质：**面板会话的 tx 负载里留着客户端的假凭证，mock 实收的已是供应商
凭证**——网关剥凭证、面板看甲乙方，两者互不干扰，恰好构成凭证替换的两侧对照。
"""

import json
import os
import sys

# _common.py 在 socket 根目录（observation/socket/_common.py），取上级目录。
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import _common as C
import _llm_mock as M


def main() -> int:
    token = C.load_token()

    upstream = M.spawn()
    with C.Gateway() as gw, C.Listener(token, "127.0.0.1", int(gw.bind.rsplit(":", 1)[1]),
                                       name="demo-llm-00") as ln:
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
        C.note("protocols 是数组：一个供应商可同时声明多个协议（共享一份配额，"
               "见 demo_04），此处各挂单协议；路由表里不需要任何条目——/llm 是"
               "保留前缀，协议段直接选供应商池。")

        # ---------------------------------------------------------- 3
        C.step(3, "三个端点经显式协议段请求（客户端一律带假凭证）")
        for tag, path, obj in (
            ("Chat Completions", "/llm/openai/v1/chat/completions",
             {"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "hi"}]}),
            ("Responses", "/llm/openai/v1/responses",
             {"model": "gpt-4o-mini", "input": "hi"}),
            ("Messages", "/llm/anthropic/v1/messages",
             {"model": "claude-3-5-sonnet-20241022", "max_tokens": 64,
              "messages": [{"role": "user", "content": "hi"}]}),
        ):
            status, provider, data, secs = M.post(ln.listen_port, path, obj)
            usage = json.loads(data).get("usage")
            seen = M.upstream_seen(path)
            print(f"  {tag:16s} POST {path:34s} -> {status} provider={provider}"
                  f" usage={usage} 耗时 {secs:.2f}s")
            print(f"  {'':16s}   mock 实收凭证：{ {k: v for k, v in seen.items() if v} }")
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
        C.step(5, "面板测量：五条会话的 tx/rx 与一条请求的解读")
        sessions = C.wait_for_new_sessions(token, ln.listen_port, baseline,
                                           5, timeout=15)
        C.print_session_rows(sessions)
        busiest = max(sessions, key=lambda s: s["bytes_rx"])
        det = C.session_detail(token, busiest["id"])
        C.print_interpret(det, "tx", only=("http", "text"))
        C.print_interpret(det, "rx", only=("http",))
        C.note("面板按 TCP 连接计量会话：步骤 3 的三次请求 + 步骤 4 的两次失败"
               "请求各成一条会话；tx 负载里原样留着客户端的假凭证，与步骤 3 的"
               "mock 侧实收互为对照。")

        # ---------------------------------------------------------- 6
        C.step(6, "清理：删供应商、停网关与监听器、停 mock")
        for pid in ("demo-openai", "demo-anthropic"):
            gw.admin(f"/llm/providers/{pid}", "DELETE")
        print("  供应商 demo-openai / demo-anthropic 已删除")

    upstream.shutdown()
    print("  网关、监听器、mock upstream 已停止")
    C.maybe_reset(token)
    print("\n演示零完成。")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except C.ApiError as e:
        print(f"\nAPI 调用失败：{e}", file=sys.stderr)
        sys.exit(1)
