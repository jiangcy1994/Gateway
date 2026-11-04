#!/usr/bin/env python3
"""演示三：/llm/v1/{端点} 端点推断 —— SDK 只需把 base_url 指向 /llm/v1。

网关行为（/llm 子网关）：
  - /llm/v1/{端点} 按端点名**闭表**推断协议（chat/completions/responses/...
    -> openai；messages/complete -> anthropic），推断只决定选池与凭证注入，
    转发路径恒为 /v1/...（保留客户端大小写与 query）
  - models / files 两协议共用 -> 404 并提示改用显式协议段；未知端点同样 404；
    systemone 不参与推断（只能经显式协议段访问）
  - 显式协议段可消歧共用端点：/llm/openai/v1/models 走 openai 池可达

观察的本质：**推断不嗅探、不翻译**——同一端点名恒落同一协议池，共用端点
则拒绝并给出可操作提示；而显式协议段永远是最确定的路由方式。
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
                                       name="demo-llm-03") as ln:
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
        C.step(3, "/llm/v1/{端点} 推断选池（凭证注入按推断出的协议来）")
        for tag, path, obj in (
            ("Chat", "/llm/v1/chat/completions",
             {"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "hi"}]}),
            ("Messages", "/llm/v1/messages",
             {"model": "claude-3-5-sonnet-20241022", "max_tokens": 64,
              "messages": [{"role": "user", "content": "hi"}]}),
        ):
            status, provider, data, secs = M.post(ln.listen_port, path, obj)
            seen = M.upstream_seen(path)
            cred = M.seen_credential(seen)
            extra = f" + anthropic-version={seen['anthropic-version']}" if seen.get("anthropic-version") else ""
            print(f"  {tag:10s} POST {path:26s} -> {status} provider={provider}"
                  f" 耗时 {secs:.2f}s mock 实收凭证={cred}{extra}")
        C.note("chat -> openai 池（注入 Bearer），messages -> anthropic 池"
               "（注入 x-api-key + 默认 anthropic-version）：SDK 把 base_url 指"
               "到 /llm/v1 即可，无需显式声明协议。")

        # ---------------------------------------------------------- 4
        C.step(4, "转发路径保留客户端写法：/llm/V1/... 与 query 原样透传")
        status, provider, data, _ = M.post(ln.listen_port, "/llm/V1/chat/completions?top_k=1",
                                           {"model": "gpt-4o-mini",
                                            "messages": [{"role": "user", "content": "hi"}]})
        print(f"  POST /llm/V1/chat/completions?top_k=1 -> {status} provider={provider}")
        print(f"  mock 侧记账键（含 query、保留大小写）："
              f"{sorted(k for k in M.SEEN_AUTH if k.startswith('/V1'))}")
        print(f"  mock 回应体：{data[:76]!r}")
        C.note("推断线索 v1 大小写不敏感（/V1/ 一样进 openai 池），但路径不重写："
               "客户端写 /V1/... 就按 /V1/...?top_k=1 原样转发——mock 只认小写"
               " /v1/*，故回 404；这恰好反过来证明网关没有改写路径。"
               "推理只影响选池，不影响路由。")

        # ---------------------------------------------------------- 5
        C.step(5, "闭表边界：共用端点与未知端点 404")
        for path in ("/llm/v1/models", "/llm/v1/files", "/llm/v1/wat", "/llm/v1"):
            status, _, data = C.http_request("127.0.0.1", ln.listen_port, "POST",
                                             path, b"{}", {"Content-Type": "application/json"})
            print(f"  POST {path:22s} -> {status} {data[:92]!r}")
        C.note("models/files 两协议都用（OpenAI 与 Anthropic 各有 /v1/models），"
               "闭表无法裁决 -> 404 并提示改用显式协议段；未知端点 -> 404。"
               "systemone 不在推断表里（只能显式段访问）。")

        # ---------------------------------------------------------- 6
        C.step(6, "显式协议段消歧：/llm/openai/v1/models 可达")
        status, _, data = C.http_request("127.0.0.1", ln.listen_port, "GET",
                                         "/llm/openai/v1/models", None, {})
        print(f"  GET  /llm/openai/v1/models -> {status} {data[:80]!r}")
        C.note("同一 models 端点：v1 推断拒绝（歧义），显式协议段直接放行——"
               "显式段是最确定的路由方式，歧义端点必须显式化。")

        # ---------------------------------------------------------- 7
        C.step(7, "面板测量：本轮请求的会话")
        sessions = C.wait_for_new_sessions(token, ln.listen_port, baseline,
                                           8, timeout=15)
        C.print_session_rows(sessions)
        det = C.session_detail(token, sessions[0]["id"])
        C.print_interpret(det, "tx", only=("http", "text"))
        C.print_interpret(det, "rx", only=("http",))
        C.note("tx 请求行里可见 /llm/v1/... 与 /llm/V1/...?top_k=1 两种形态；"
               "404 的几条会话没有上游流量，rx 只有网关自己给的 JSON 错误体。")

        # ---------------------------------------------------------- 8
        C.step(8, "清理：删供应商、停网关与监听器、停 mock")
        for pid in ("demo-openai", "demo-anthropic"):
            gw.admin(f"/llm/providers/{pid}", "DELETE")
        print("  供应商 demo-openai / demo-anthropic 已删除")

    upstream.shutdown()
    print("  网关、监听器、mock upstream 已停止")
    C.maybe_reset(token)
    print("\n演示三完成。")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except C.ApiError as e:
        print(f"\nAPI 调用失败：{e}", file=sys.stderr)
        sys.exit(1)
