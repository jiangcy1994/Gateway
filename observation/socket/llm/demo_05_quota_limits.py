#!/usr/bin/env python3
"""演示五：配额极限 —— 权重归零的三种归宿（429 / 503）。

网关行为（/llm 子网关）：
  - 选池后按权重 = min(RPM 剩余比, TPM 剩余比) 选供应商，任一维度满额权重即 0；
    池内全部供应商权重 <= 0 -> 429（配额耗尽）
  - 池为空（协议无供应商，或全部禁用）-> 503（另一个错误码）
  - RPM 在请求时（选供应商的临界区里）扣；TPM 由响应尾包 usage 回填
    （见 demo_02），所以 TPM 的第二次请求是在**响应到达之后**才被挡住

观察的本质：**429 与 503 是两个不同的错误**，面板上各有几条会话，
响应体里的信息也不同（配额 vs 无可用供应商）。
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import _common as C
import _llm_mock as M


def main() -> int:
    token = C.load_token()

    upstream = M.spawn()
    with C.Gateway() as gw, C.Listener(token, "127.0.0.1", int(gw.bind.rsplit(":", 1)[1]),
                                       name="demo-llm-05") as ln:
        # ---------------------------------------------------------- 1
        C.step(1, "环境：三端点 LLM mock upstream + 网关 + 面板监听器（不挂任何供应商）")
        print(f"  mock upstream : http://127.0.0.1:{upstream.server_address[1]}")
        print(f"  gateway       : http://{gw.bind}")
        print(f"  panel listener: 127.0.0.1:{ln.listen_port} -> {ln.obj['target']}")
        print("  初始供应商数：0（先看空池行为，再逐个挂小配额供应商）")
        baseline = C.session_baseline(token, ln.listen_port)

        # ---------------------------------------------------------- 2
        C.step(2, "空池：协议一个供应商都没有 -> 503")
        status, _, data = C.http_request("127.0.0.1", ln.listen_port, "POST",
                                         "/llm/anthropic/v1/messages",
                                         b'{"model":"x","max_tokens":8}',
                                         {"Content-Type": "application/json"})
        print(f"  POST /llm/anthropic/v1/messages -> {status} {data[:92]!r}")
        C.note("503 与 429 是两个错误码：503 表示「该协议池为空」（无供应商或"
               "全部禁用），429 表示「池里有供应商但配额全满」。")

        # ---------------------------------------------------------- 3
        C.step(3, "RPM 耗尽 -> 429（用 systemone 隔离 RPM 这一条维度）")
        # systemone 的 usage 提取是 stub（恒 0，见 src/llm/usage.rs），
        # TPM 不会累积，正好 isolates RPM。
        gw.admin("/llm/providers", "POST", {
            "id": "demo-tiny-rpm", "protocols": ["systemone"],
            "base_url": f"http://127.0.0.1:{upstream.server_address[1]}",
            "api_key": "sk-mock-tiny", "rpm_limit": 1, "tpm_limit": 1000,
        })
        for i in (1, 2):
            status, _, data = C.http_request("127.0.0.1", ln.listen_port, "POST",
                                             "/llm/systemone/v1/chat/completions",
                                             b'{"model":"x"}',
                                             {"Content-Type": "application/json"})
            print(f"  RPM 第 {i} 次 -> {status} {data[:76]!r}")
        q = next(x for x in gw.admin("/llm/quota")["providers"] if x["id"] == "demo-tiny-rpm")
        print(f"    demo-tiny-rpm quota: rpm {q['rpm']['used']}/{q['rpm']['limit']}"
              f"（remaining={q['rpm']['remaining']}），"
              f"tpm {q['tpm']['used']}/{q['tpm']['limit']}（stub 恒 0）")
        C.note("rpm_limit=1：第一次请求在临界区里扣光配额，第二次权重=0 -> 429。"
               "systemone 只能经显式协议段访问，且 usage 提取恒为 0，故 TPM 维度"
               "不会先满——这个供应商专门用来观察纯 RPM 耗尽。")

        # ---------------------------------------------------------- 4
        C.step(4, "TPM 超限 -> 429（单次响应就用爆 tpm_limit=10）")
        gw.admin("/llm/providers", "POST", {
            "id": "demo-tiny-tpm", "protocols": ["anthropic"],
            "base_url": f"http://127.0.0.1:{upstream.server_address[1]}",
            "api_key": "sk-mock-tiny", "rpm_limit": 100, "tpm_limit": 10,
        })
        for i in (1, 2):
            status, _, data = C.http_request("127.0.0.1", ln.listen_port, "POST",
                                             "/llm/anthropic/v1/messages",
                                             b'{"model":"x","max_tokens":8}',
                                             {"Content-Type": "application/json"})
            print(f"  TPM 第 {i} 次 -> {status} {data[:76]!r}")
        q = next(x for x in gw.admin("/llm/quota")["providers"] if x["id"] == "demo-tiny-tpm")
        print(f"    demo-tiny-tpm quota: rpm {q['rpm']['used']}/{q['rpm']['limit']}，"
              f"tpm {q['tpm']['used']}/{q['tpm']['limit']}"
              f"（remaining={q['tpm']['remaining']}，单次响应就用了 "
              f"{M.USAGE_MS_IN + M.USAGE_MS_OUT} > 10）")
        C.note("第一次请求成功（TPM 在响应尾包才回填，请求时权重仍正）；回填后"
               " tpm.used=16 > 10，TPM 剩余比归零 -> 权重=0 -> 第二次 429。"
               "权重取 min(RPM 剩余比, TPM 剩余比)：任一维度满额即出局。")

        # ---------------------------------------------------------- 5
        C.step(5, "面板测量：五条会话（一次 503、RPM 与 TPM 各一次 200/429）")
        sessions = C.wait_for_new_sessions(token, ln.listen_port, baseline,
                                           5, timeout=15)
        C.print_session_rows(sessions)
        det = C.session_detail(token, sessions[0]["id"])
        C.print_interpret(det, "rx", only=("http", "text"))
        C.note("503 与 429 的会话都没有上游流量（rx 只有网关自己的错误体）；"
               "成功的那两条会话 rx 里能读到 mock 的 JSON 响应与 usage。")

        # ---------------------------------------------------------- 6
        C.step(6, "清理：删供应商、停网关与监听器、停 mock")
        for pid in ("demo-tiny-rpm", "demo-tiny-tpm"):
            gw.admin(f"/llm/providers/{pid}", "DELETE")
        print("  供应商 demo-tiny-rpm / demo-tiny-tpm 已删除")

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
