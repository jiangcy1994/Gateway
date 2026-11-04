#!/usr/bin/env python3
"""演示一：流式 SSE 透传 —— 面板上的逐行到达。

网关行为（/llm 子网关）：
  - 请求/响应体一律流式（Body::from_stream / wrap_stream，不缓冲），
    SSE 响应逐块转发，不做任何聚合
  - 显式协议段 /llm/openai/... 与 /llm/anthropic/... 分别选池，
    两协议的 SSE 事件格式不同（openai: data 块 + [DONE]；
    anthropic: event:/data: 成对事件）

观察的本质：**mock 每写一行 sleep 0.3s，客户端 readline 的相邻到达间隔
约 0.3s**——若网关先收齐再转发，间隔应为 0。面板会话的 rx 负载按块记录，
可逐块核对到达节奏。
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import _common as C
import _llm_mock as M


def _gap_report(arrivals):
    """汇总 SSE 到达：行数、总字节、相邻行到达间隔（秒）。"""
    gaps = [round(b - a, 2) for (a, _), (b, _) in zip(arrivals, arrivals[1:])]
    total = sum(len(line) for _, line in arrivals)
    return len(arrivals), total, gaps


def main() -> int:
    token = C.load_token()

    upstream = M.spawn()
    with C.Gateway() as gw, C.Listener(token, "127.0.0.1", int(gw.bind.rsplit(":", 1)[1]),
                                       name="demo-llm-01") as ln:
        # ---------------------------------------------------------- 1
        C.step(1, "环境：SSE mock upstream + 网关 + 面板监听器")
        print(f"  mock upstream : http://127.0.0.1:{upstream.server_address[1]}"
              "（每写一行 sleep 0.3s，用于暴露「先收齐再转发」）")
        print(f"  gateway       : http://{gw.bind}")
        print(f"  panel listener: 127.0.0.1:{ln.listen_port} -> {ln.obj['target']}")
        baseline = C.session_baseline(token, ln.listen_port)

        # ---------------------------------------------------------- 2
        C.step(2, "挂两个协议的 supplier（同 demo_00）")
        for pid, proto, key in (
            ("demo-openai", "openai", "sk-mock-openai-key"),
            ("demo-anthropic", "anthropic", "sk-mock-anthropic-key"),
        ):
            gw.admin(f"/llm/providers", "POST", {
                "id": pid, "protocols": [proto],
                "base_url": f"http://127.0.0.1:{upstream.server_address[1]}",
                "api_key": key, "rpm_limit": 60, "tpm_limit": 100000,
            })

        # ---------------------------------------------------------- 3
        C.step(3, "两条流式请求（stream: true，逐行读响应）")
        paths = (
            ("openai", "/llm/openai/v1/chat/completions",
             {"model": "gpt-4o-mini", "stream": True,
              "messages": [{"role": "user", "content": "hi"}]}),
            ("anthropic", "/llm/anthropic/v1/messages",
             {"model": "claude-3-5-sonnet-20241022", "max_tokens": 64,
              "stream": True, "messages": [{"role": "user", "content": "hi"}]}),
        )
        for tag, path, obj in paths:
            status, provider, arrivals, secs = M.post(ln.listen_port, path, obj, sse=True)
            rows, total, gaps = _gap_report(arrivals)
            print(f"  {tag:10s} POST {path} -> {status} provider={provider}")
            print(f"  {'':10s}   SSE {rows} 行 / {total} B / 总耗时 {secs:.2f}s"
                  f" 行间隔={gaps}")
            for rel, line in arrivals:
                shown = line.rstrip()
                print(f"  {'':10s}   +{rel:4.1f}s {shown[:84]!r}{'...' if len(shown) > 84 else ''}")
        C.note("间隔约 0.3s 即逐行透传（网关不缓冲）；事件格式原样保留——"
               "openai 是 data 块 + [DONE]，anthropic 是 event:/data: 成对事件。"
               "usage 只在尾包里（openai 最后一个 data 块、anthropic 的 "
               "message_start + message_delta），TPM 记账见 demo_02。")

        # ---------------------------------------------------------- 4
        C.step(4, "面板测量：两条 SSE 会话")
        sessions = C.wait_for_new_sessions(token, ln.listen_port, baseline,
                                           2, timeout=20)
        C.print_session_rows(sessions)
        # SSE 会话的 rx 在「每行一个事件」量级；非流式会话一次读走整段 JSON。
        sse_sessions = [s for s in sessions if 400 < s["bytes_rx"] < 900]
        for s in sse_sessions:
            det = C.session_detail(token, s["id"])
            C.print_interpret(det, "tx", only=("http", "text"))
            C.print_interpret(det, "rx", only=("text",))
        C.note("面板会话行的 pkts 列：两条会话都是 rx=5——SSE 的 12 行被分成 5 个"
               "TCP 段到达（Nagle/刷包节奏），rx 负载里是 text 原文逐块拼接；"
               "客户端侧 readline 的相邻间隔 0.3s 即上游的发出节奏。")

        # ---------------------------------------------------------- 5
        C.step(5, "清理：删供应商、停网关与监听器、停 mock")
        for pid in ("demo-openai", "demo-anthropic"):
            gw.admin(f"/llm/providers/{pid}", "DELETE")
        print("  供应商 demo-openai / demo-anthropic 已删除")

    upstream.shutdown()
    print("  网关、监听器、mock upstream 已停止")
    C.maybe_reset(token)
    print("\n演示一完成。")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except C.ApiError as e:
        print(f"\nAPI 调用失败：{e}", file=sys.stderr)
        sys.exit(1)
