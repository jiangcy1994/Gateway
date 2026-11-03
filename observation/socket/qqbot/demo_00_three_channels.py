#!/usr/bin/env python3
"""演示零：三路接入总览 —— OpenAPI 透传 / Webhook 回调 / WebSocket 客户端。

同一个机器人配置（同一 app_id）下，top-gateway 的 QQ Bot 子网关同时提供
三条通道，本演示把三条各走一遍（对着共享的 QQ Bot Mock）：

  1. OpenAPI 透传（出站）：业务端经 /qqbot/{app_id}/... 调平台，
     网关自动取 token 并替换鉴权头；
  2. Webhook 回调（入站，平台推送）：平台 POST 网关 /qqbot/callback/{app_id}，
     网关先 ACK（{"op":12}）再把事件异步投递到业务端 event_url；
  3. WebSocket 客户端（入站，主动长连接）：网关替 bot 维持到平台的 WS 会话，
     收到的事件与 webhook 走同一个 deliver_event 投递。

前置：QQ Bot Mock 已运行（默认 http://127.0.0.1:8080）。
"""

import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _common as C


def main() -> int:
    C.check_mock_reachable()
    prev_webhook = C.webhook_state()

    with C.Gateway() as gw, C.EventSink(name="biz-backend") as sink:
        try:
            # ---------------------------------------------------------- 1
            C.step(1, "注册机器人：event_source=webhook，事件投递到本机业务后端")
            bot = C.add_bot(gw, event_url=sink.url, event_secret="demo-event-secret")
            C.show("bot", {k: bot.get(k) for k in
                           ("id", "app_id", "event_source", "event_url", "enabled")})
            C.note("webhook 形态的机器人不拉起 WS supervisor：/__admin/qqbot/ws 为空")
            C.show("ws 快照", C.ws_status())

            # 平台 webhook 注册指向本演示网关的回调地址。
            # 注意：mock 用 webhook 的 secret 校验 op 13 回签，网关用
            # bot.app_secret 签名——两者需一致（这里同为 mock_client_secret）。
            C.set_webhook(gw.callback_url(), C.MOCK_APP_SECRET)
            C.show("平台 webhook 注册", C.webhook_state())

            # ---------------------------------------------------------- 2
            C.step(2, "通道一（出站）：OpenAPI 透传，网关自动取 token")
            status, headers, body = gw.openapi(
                "GET", f"/qqbot/{C.MOCK_APP_ID}/users/@me",
                headers={"Authorization": "Bearer client-fake-token"})
            C.show("GET /users/@me", {"status": status,
                                      "x-qqbot-bot": headers.get("x-qqbot-bot"),
                                      "mock 视角": json.loads(body)})
            C.note("客户端带的是假凭证，网关已替换为平台的 QQBot <token>；"
                   "x-qqbot-bot 响应头标明本次命中的机器人配置。")

            # ---------------------------------------------------------- 3
            C.step(3, "通道二（入站）：用户在群里 @ 机器人 → 平台推 webhook → 网关投业务端")
            t0 = time.time()
            rec = C.dispatch_group_message()
            C.show("dispatch", rec["records"])
            sink.wait_for(1)
            ev = sink.events[0]
            C.show("业务端收到", {"path": ev["path"], "auth": ev["auth"],
                                 "ctype": ev["ctype"],
                                 "到达耗时": f"{time.time() - t0:.2f}s"})
            payload = json.loads(ev["body"])
            C.show("事件体", {"id": payload.get("id"), "t": payload.get("t"),
                             "s": payload.get("s"),
                             "content": payload.get("d", {}).get("content")})
            C.note("Authorization: Bearer demo-event-secret 由网关按 event_secret 注入；"
                   "t=GROUP_AT_MESSAGE_CREATE，外层 id 即被动回复用的 msg_id。")
            C.note("注意外层 s 为 null：mock 的 webhook 推送不带 s（WS 帧才带，见步骤 5），"
                   "所以 seq/last_seq 天然是 WS 信道一侧的概念。")

            # ---------------------------------------------------------- 4
            C.step(4, "回调地址验证（op 13）：网关即时回签 ed25519")
            v = C.validate_webhook()
            resp_sig = (v.get("response") or {}).get("signature")
            C.show("validate", {"ok": v.get("ok"), "status": v.get("status"),
                                "回签长度": len(resp_sig or "")})
            C.note(f"网关回签 == mock 期望签名（expected_signature）："
                   f"{resp_sig == v.get('expected_signature')}")
            C.note("网关只签名不验签（ADR-Q7/R4）：平台 inbound 签名公钥不在参考文档内，"
                   "故回调路径本身不校验 X-Signature-Ed25519。")

            # ---------------------------------------------------------- 5
            C.step(5, "通道三（入站）：切换 event_source=websocket，网关替机器人长连接")
            gw.admin(f"/qqbot/bots/{bot['id']}", "PUT", {"event_source": "websocket"})
            row = C.wait_for_ws(bot["id"], connected=True)
            C.show("ws 状态", row)

            C.note("平台 webhook 仍指向本网关时两条通道并行：")
            base = len(sink.events)
            rec = C.dispatch_group_message()
            C.show("dispatch", rec["records"])
            sink.wait_for(base + 2)
            for i, e in enumerate(sink.events[base:], base):
                p = json.loads(e["body"])
                print(f"  业务端[{i}]：id={p.get('id', '')[:20]}… s={p.get('s')!r}")
            C.note("同一事件被投递了两次：平台既推了 webhook、又推了 WS 会话，"
                   "网关两侧都投给了同一个 event_url——去重由业务端按事件 id 做。")
            C.note("两份副本可以靠外层 s 区分：webhook 推送不带 s，WS 帧带 s——"
                   "只有经 WS 到达的那份携带序号。")

            C.set_webhook(C.DEAD_WEBHOOK)
            C.note(f"把平台 webhook 指向必失败地址 {C.DEAD_WEBHOOK}，"
                   "让本次事件只经 WS 到达。")
            base = len(sink.events)
            t0 = time.time()
            rec = C.dispatch_group_message()
            C.show("dispatch", rec["records"])
            sink.wait_for(base + 1)
            ev = sink.events[base]
            payload = json.loads(ev["body"])
            C.show("业务端收到", {"id": payload.get("id"), "t": payload.get("t"),
                                 "s": payload.get("s"),
                                 "到达耗时": f"{time.time() - t0:.2f}s"})
            row = C.ws_status(bot["id"])
            C.show("ws 状态（投递后）",
                   {k: row.get(k) for k in ("connected", "session_id", "last_seq",
                                            "reconnects", "last_error")})
            C.note("records 里 ws_sessions=1、webhook_ok=false：平台推送落空（不重试），"
                   "事件由 WS 会话送达；本份副本外层 s 非 null，且 last_seq 已随之前进。")
            C.note("到达耗时的零头里有 ~2s：平台向不可达 webhook 的推送是同步等待的，"
                   "本机 loopback 连接拒绝约 2s（机器特性），它推迟的是平台推 WS 的时机，"
                   "不经过网关——网关收到 WS 帧后的投递仍是毫秒级。")

            # ---------------------------------------------------------- 6
            C.step(6, "收尾：删除机器人（WS supervisor 联动停止）")
            gw.admin(f"/qqbot/bots/{bot['id']}", "DELETE")
            C.show("bot 列表", [b["id"] for b in gw.admin("/qqbot/bots")["bots"]])
            C.show("ws 快照（删除后）", C.ws_status())
            C.note("bots.toml 同步落盘：重启网关后机器人仍在（本演示用临时文件，"
                   "进程退出即清）。")
        finally:
            C.restore_webhook(prev_webhook)
            C.show("平台 webhook 已还原为", C.webhook_state())

    print("\n演示零完成。")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except C.ApiError as e:
        print(f"\nAPI 调用失败：{e}", file=sys.stderr)
        sys.exit(1)
