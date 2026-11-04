#!/usr/bin/env python3
"""演示四：多协议供应商 —— 一份配额跨两个协议池。

网关行为（/llm 子网关）：
  - Provider 的 protocols 是数组：声明几个协议就进几个池，
    但 quota / metrics / selector 记账都按**供应商**记一份
  - 同一供应商在不同协议池里按该协议的凭证风格注入：
    openai 端点 -> Bearer，anthropic 端点 -> x-api-key + anthropic-version
  - 选池仍按请求的协议段（或 v1 推断结果），池内按剩余配额加权

观察的本质：**一个供应商侍奉两个协议时，quota 视图里只有一条记录**——
openai 端点的请求与 anthropic 端点的请求共同消耗同一份 RPM/TPM。
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
                                       name="demo-llm-04") as ln:
        # ---------------------------------------------------------- 1
        C.step(1, "环境：三端点 LLM mock upstream + 网关 + 面板监听器")
        print(f"  mock upstream : http://127.0.0.1:{upstream.server_address[1]}")
        print(f"  gateway       : http://{gw.bind}")
        print(f"  panel listener: 127.0.0.1:{ln.listen_port} -> {ln.obj['target']}")
        baseline = C.session_baseline(token, ln.listen_port)

        # ---------------------------------------------------------- 2
        C.step(2, "挂三家供应商：两个单协议 + 一个双协议共享")
        for pid, protos, key in (
            ("demo-openai", ["openai"], "sk-mock-openai-key"),
            ("demo-anthropic", ["anthropic"], "sk-mock-anthropic-key"),
            ("demo-shared", ["openai", "anthropic"], "sk-mock-shared"),
        ):
            gw.admin(f"/llm/providers", "POST", {
                "id": pid, "protocols": protos,
                "base_url": f"http://127.0.0.1:{upstream.server_address[1]}",
                "api_key": key, "rpm_limit": 60, "tpm_limit": 100000,
            })
        print("  provider 列表（id / protocols / enabled）：")
        for p in gw.admin("/llm/providers")["providers"]:
            print(f"    {p['id']:16s} {json.dumps(p['protocols'])} enabled={p['enabled']}")
        C.note("demo-shared 同时声明 openai 与 anthropic：两个协议池里都有它，"
               "但它只有一份配额——protocols 决定池成员资格，quota 按供应商计。")

        # ---------------------------------------------------------- 3
        C.step(3, "隔离：禁用两个单协议供应商，让两池只剩 demo-shared")
        for pid in ("demo-openai", "demo-anthropic"):
            gw.admin(f"/llm/providers/{pid}", "PUT", {"enabled": False})
            print(f"  已禁用 {pid}")
        for p in gw.admin("/llm/providers")["providers"]:
            print(f"    {p['id']:16s} {json.dumps(p['protocols'])} enabled={p['enabled']}")

        # ---------------------------------------------------------- 4
        C.step(4, "两协议端点都落到 demo-shared（凭证按协议注入）")
        for tag, path in (
            ("openai 端点", "/llm/v1/chat/completions"),
            ("anthropic 端点", "/llm/anthropic/v1/messages"),
        ):
            obj = ({"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "hi"}]}
                   if "openai" in tag else
                   {"model": "claude-3-5-sonnet-20241022", "max_tokens": 64,
                    "messages": [{"role": "user", "content": "hi"}]})
            status, provider, data, _ = M.post(ln.listen_port, path, obj)
            seen = M.upstream_seen(path)
            cred = M.seen_credential(seen)
            extra = f" + anthropic-version={seen['anthropic-version']}" if seen.get("anthropic-version") else ""
            print(f"  {tag:14s} POST {path:30s} -> {status} provider={provider}"
                  f" mock 实收凭证={cred}{extra}")
        C.note("两条请求用同一把 sk-mock-shared：openai 端点被注入为 Bearer、"
               "anthropic 端点被注入为 x-api-key 并补 anthropic-version —— 供应商"
               "只存一份 key，按协议选凭证风格。")

        # ---------------------------------------------------------- 5
        C.step(5, "quota 只有一条记录：RPM 已是 2（跨协议累加）")
        for q in gw.admin("/llm/quota")["providers"]:
            if q["id"].startswith("demo-"):
                print(f"    {q['id']:16s} protocols={json.dumps(q['protocols'])}"
                      f" rpm used={q['rpm']['used']}/{q['rpm']['limit']}"
                      f" tpm used={q['tpm']['used']}/{q['tpm']['limit']}")
        q = next(x for x in gw.admin("/llm/quota")["providers"] if x["id"] == "demo-shared")
        print(f"  demo-shared 单条记录：rpm used={q['rpm']['used']}（步骤 4 的"
              " openai 与 anthropic 请求各计 1 次）")
        C.note("若按协议各记一份，rpm used 应为 1；实际是 2 —— 配额跨协议共享。"
               "反过来说，把大流量供应商同时挂两池要小心：配额会被两池分食。")

        # ---------------------------------------------------------- 6
        C.step(6, "面板测量：两条会话")
        sessions = C.wait_for_new_sessions(token, ln.listen_port, baseline,
                                           2, timeout=15)
        C.print_session_rows(sessions)
        for s in sessions:
            det = C.session_detail(token, s["id"])
            C.print_interpret(det, "tx", only=("http", "text"))
        C.note("两条会话分别经 /llm/v1/... 与 /llm/anthropic/... 入站，"
               "provider 响应头都是 demo-shared；tx 负载里客户端的假凭证"
               "依旧原样（网关在转发前才替换）。")

        # ---------------------------------------------------------- 7
        C.step(7, "清理：恢复启用、删供应商、停网关与监听器、停 mock")
        for pid in ("demo-openai", "demo-anthropic"):
            gw.admin(f"/llm/providers/{pid}", "PUT", {"enabled": True})
        for pid in ("demo-openai", "demo-anthropic", "demo-shared"):
            gw.admin(f"/llm/providers/{pid}", "DELETE")
        print("  供应商 demo-openai / demo-anthropic / demo-shared 已删除")

    upstream.shutdown()
    print("  网关、监听器、mock upstream 已停止")
    C.maybe_reset(token)
    print("\n演示四完成。")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except C.ApiError as e:
        print(f"\nAPI 调用失败：{e}", file=sys.stderr)
        sys.exit(1)
