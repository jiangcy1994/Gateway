#!/usr/bin/env python3
"""演示二：usage 提取与 TPM 记账 —— 响应尾包回填配额。

网关行为（/llm 子网关）：
  - 请求/响应流式透传的同时，UsageTap 把 chunk 复制进 64 KiB TailBuffer，
    在流结束 / 上游出错 / 消费者断开时**恰好一次**提取 usage（best-effort，
    提取失败记 0，永不影响转发）
  - 非流式 JSON 直接取 usage 对象；SSE 取尾包——openai 汇总 data 块，
    anthropic 取首 message_start 的 input_tokens 加末 message_delta 的
    output_tokens
  - TPM/RPM 都是 60s 滚动窗口；quota 按供应商记一份（跨协议共享，见 demo_04）

观察的本质：**`/__admin/llm/quota` 的 tpm.used 与响应里的 usage 逐项对账**，
流式与非流式混跑后两侧数字必须吻合（本演示 openai 池 159、anthropic 池 32）。
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import _common as C
import _llm_mock as M


def main() -> int:
    token = C.load_token()

    upstream = M.spawn()
    with C.Gateway() as gw, C.Listener(token, "127.0.0.1", int(gw.bind.rsplit(":", 1)[1]),
                                       name="demo-llm-02") as ln:
        # ---------------------------------------------------------- 1
        C.step(1, "环境：三端点 LLM mock upstream + 网关 + 面板监听器")
        print(f"  mock upstream : http://127.0.0.1:{upstream.server_address[1]}")
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
        C.step(3, "五次请求的 usage（3 次非流式 JSON + 2 次 SSE 流式）")
        expect_openai = 0
        expect_anthropic = 0
        for tag, path, obj, sse in (
            ("Chat 非流式", "/llm/openai/v1/chat/completions",
             {"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "hi"}]}, False),
            ("Responses", "/llm/openai/v1/responses",
             {"model": "gpt-4o-mini", "input": "hi"}, False),
            ("Chat 流式", "/llm/openai/v1/chat/completions",
             {"model": "gpt-4o-mini", "stream": True,
              "messages": [{"role": "user", "content": "hi"}]}, True),
            ("Messages 非流式", "/llm/anthropic/v1/messages",
             {"model": "claude-3-5-sonnet-20241022", "max_tokens": 64,
              "messages": [{"role": "user", "content": "hi"}]}, False),
            ("Messages 流式", "/llm/anthropic/v1/messages",
             {"model": "claude-3-5-sonnet-20241022", "max_tokens": 64,
              "stream": True, "messages": [{"role": "user", "content": "hi"}]}, True),
        ):
            status, provider, payload, secs = M.post(ln.listen_port, path, obj, sse=sse)
            if sse:
                tail = [line.rstrip().decode("utf-8", "replace")
                        for _, line in payload if b"usage" in line]
                print(f"  {tag:14s} POST {path:30s} -> {status} provider={provider}"
                      f" 总耗时 {secs:.2f}s")
                print(f"  {'':14s}   SSE 尾包 usage 行：{tail}")
            else:
                usage = json.loads(payload).get("usage")
                print(f"  {tag:14s} POST {path:30s} -> {status} provider={provider}"
                      f" 耗时 {secs:.2f}s")
                print(f"  {'':14s}   JSON usage：{usage}")
        expect_openai = M.USAGE_CC * 2 + M.USAGE_RS
        expect_anthropic = (M.USAGE_MS_IN + M.USAGE_MS_OUT) * 2
        C.note(f"期望 TPM：openai 池 {M.USAGE_CC}(Chat 非流式) + {M.USAGE_RS}"
               f"(Responses) + {M.USAGE_CC}(Chat 流式) = {expect_openai}；"
               f"anthropic 池 ({M.USAGE_MS_IN}+{M.USAGE_MS_OUT}) x 2 = "
               f"{expect_anthropic}。流式取尾包、非流式取 usage 对象，两者"
               "混跑后 quota 只应有一份累加结果。")

        # ---------------------------------------------------------- 4
        C.step(4, "面板测量：五条会话")
        sessions = C.wait_for_new_sessions(token, ln.listen_port, baseline,
                                           5, timeout=20)
        C.print_session_rows(sessions)
        C.note("五次请求各新建一条 TCP 连接，故五条会话；SSE 两条会话的 "
               "bytes_rx 逐块增长，非流式三条一次读走整段 JSON——但它们的 "
               "usage 都会被网关从流尾提取，与会话字节量无关。")

        # ---------------------------------------------------------- 5
        C.step(5, "网关侧对照：metrics（请求数）与 quota（RPM/TPM）")
        for p in gw.admin("/llm/metrics")["providers"]:
            if p["provider"]["id"].startswith("demo-"):
                print(f"  provider {p['provider']['id']}: {json.dumps(p['stats'], ensure_ascii=False)}")
        print("  quota（TPM 由响应尾包 usage 提取，60s 滚动窗口）：")
        quota = {}
        for q in gw.admin("/llm/quota")["providers"]:
            if q["id"].startswith("demo-"):
                quota[q["id"]] = q
                print(f"    {q['id']:16s} rpm used={q['rpm']['used']}/{q['rpm']['limit']}"
                      f"  tpm used={q['tpm']['used']}/{q['tpm']['limit']}")
        ok_o = quota["demo-openai"]["tpm"]["used"] == expect_openai
        ok_a = quota["demo-anthropic"]["tpm"]["used"] == expect_anthropic
        print(f"  对账：openai tpm {quota['demo-openai']['tpm']['used']}"
              f" == {expect_openai} -> {ok_o}；"
              f"anthropic tpm {quota['demo-anthropic']['tpm']['used']}"
              f" == {expect_anthropic} -> {ok_a}")
        C.note("选供应商的权重 = min(RPM 剩余比, TPM 剩余比)：本演示配额极大，"
               "权重恒正，所以五次请求各自落在对应协议池里；配额耗尽时的行为"
               "见 demo_05。")

        # ---------------------------------------------------------- 6
        C.step(6, "清理：删供应商、停网关与监听器、停 mock")
        for pid in ("demo-openai", "demo-anthropic"):
            gw.admin(f"/llm/providers/{pid}", "DELETE")
        print("  供应商 demo-openai / demo-anthropic 已删除")

    upstream.shutdown()
    print("  网关、监听器、mock upstream 已停止")
    C.maybe_reset(token)
    print("\n演示二完成。")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except C.ApiError as e:
        print(f"\nAPI 调用失败：{e}", file=sys.stderr)
        sys.exit(1)
