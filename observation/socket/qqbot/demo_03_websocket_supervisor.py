#!/usr/bin/env python3
"""演示三：WebSocket 接入 —— 网关替机器人维持长连接。

网关行为（src/qqbot/ws.rs，每个 event_source=websocket 的 bot 一个 supervisor）：
  - 注册/变更 bot 时联动：需要 WS → ensure 起 supervisor，停止/删除 → stop；
  - 连接流程：取接入点（GET /gateway/bot）→ connect → op 10 Hello →
    op 2 Identify（有 session 且 last_seq>0 才改用 op 6 Resume）→ READY；
  - 周期心跳（op 1，带最新 seq；连续两次未收到 op 11 才重连）；
  - 入站事件（op 0）经 deliver_event 投递到 event_url——与 webhook 同一路径；
  - 配置变更（PUT）→ 停旧 supervisor、按新配置重建（新会话，新 session_id）。

mock 观测点：
  - dispatch records 的 ws_sessions（推给了几个会话）；
  - GET /__admin/qqbot/ws 的 connected / session_id / last_seq / reconnects / last_error。
"""

import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _common as C

BOT_ID = "demo-ws"


def main() -> int:
    C.check_mock_reachable()
    prev_webhook = C.webhook_state()

    with C.Gateway() as gw, C.EventSink(name="ws-biz") as sink:
        try:
            # ---------------------------------------------------------- 1
            C.step(1, "摘掉平台 webhook：本演示的事件只经 WS 到达")
            C.set_webhook(C.DEAD_WEBHOOK)
            C.show("平台 webhook 注册", {"url": C.webhook_state().get("url")})
            C.note(f"指向必失败地址 {C.DEAD_WEBHOOK}：平台推送落空但不重试，"
                   "事件的唯一入口是 WS 会话。")

            # ---------------------------------------------------------- 2
            C.step(2, "注册 event_source=websocket 的机器人：supervisor 自动拉起")
            bot = C.add_bot(gw, id=BOT_ID, event_source="websocket",
                            event_url=sink.url, event_secret="demo-event-secret")
            C.show("bot", {k: bot.get(k) for k in
                           ("id", "event_source", "ws_url", "intents")})
            row = C.wait_for_ws(BOT_ID, connected=True, timeout=20)
            C.show("ws 状态", row)
            C.note("注册的一侧联动：bot 变更先 persist 再 ensure/stop（src/qqbot/admin.rs）；"
                   "intents=100663296 = (1<<25)|(1<<26)，含群与单聊事件位。")

            # ---------------------------------------------------------- 3
            C.step(3, "事件经 WS 到达：dispatch → ws_sessions=1 → 业务端")
            rec = C.dispatch_group_message()
            C.show("dispatch", rec["records"])
            sink.wait_for(1)
            payload = json.loads(sink.events[0]["body"])
            C.show("业务端收到", {"id": payload.get("id"), "t": payload.get("t"),
                                 "s": payload.get("s"),
                                 "auth": sink.events[0]["auth"]})
            row = C.ws_status(BOT_ID)
            C.show("ws 状态（投递后）",
                   {k: row.get(k) for k in ("connected", "session_id", "last_seq",
                                            "reconnects", "last_error")})
            C.note("last_seq 随事件 s 前进；投递路径与 webhook 完全相同（deliver_event），"
                   "业务端看不出事件来自哪条通道。")

            # ---------------------------------------------------------- 4
            C.step(4, "配置变更 → supervisor 重建：stop + ensure（新会话）")
            session_before = C.ws_status(BOT_ID).get("session_id")
            seq_before = C.ws_status(BOT_ID).get("last_seq")
            gw.admin(f"/qqbot/bots/{BOT_ID}", "PUT", {"name": "演示机器人（改名）"})
            row = C.wait_for_ws(BOT_ID, connected=True, timeout=20)
            C.show("ws 状态（PUT 后）", row)
            C.note(f"session_id {session_before} -> {row.get('session_id')}："
                   "PUT 停掉旧 supervisor 后按新配置重建，内存里的 session/seq 清零，"
                   "新连接走 Identify 而非 Resume，reconnects 归零。")
            C.note("这也意味着重连的退避计数与旧连接无关："
                   "一次 PUT 相当于一次干净的冷启动。")

            # ---------------------------------------------------------- 5
            C.step(5, "禁用 bot → supervisor 退出（/ws 行消失）")
            gw.admin(f"/qqbot/bots/{BOT_ID}", "PUT", {"enabled": False})
            row = C.wait_for_ws(BOT_ID, connected=False, timeout=10)
            C.show("ws 状态（禁用后）", {"行": row})
            C.note("禁用走 stop：表内没有该 bot 的行（而不是留一行 connected=false）。")

            # ---------------------------------------------------------- 6
            C.step(6, "重新启用 → supervisor 再次拉起，事件恢复投递")
            gw.admin(f"/qqbot/bots/{BOT_ID}", "PUT", {"enabled": True})
            row = C.wait_for_ws(BOT_ID, connected=True, timeout=20)
            C.show("ws 状态", row)
            rec = C.dispatch_group_message()
            C.show("dispatch", rec["records"])
            sink.wait_for(2)
            payload = json.loads(sink.events[1]["body"])
            C.show("业务端收到", {"id": payload.get("id"), "t": payload.get("t"),
                                 "s": payload.get("s")})
            C.note("生命周期全由 bot 表驱动：业务端不需要管连接，只管收 event_url。")

            # ---------------------------------------------------------- 7
            C.step(7, "收尾：删除机器人（supervisor 停止）、还原平台 webhook")
            gw.admin(f"/qqbot/bots/{BOT_ID}", "DELETE")
            row = C.wait_for_ws(BOT_ID, connected=False, timeout=10)
            C.show("ws 快照（删除后）", {"行": row, "全部行": C.ws_status()})
        finally:
            C.restore_webhook(prev_webhook)
            C.show("平台 webhook 已还原为", C.webhook_state())

    print("\n演示三完成。")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except C.ApiError as e:
        print(f"\nAPI 调用失败：{e}", file=sys.stderr)
        sys.exit(1)
