#!/usr/bin/env python3
"""演示二：Webhook 回调 —— 先 ACK 再投递、投递失败不重试、op 13 签名。

网关行为（/qqbot/callback/{app_id}，src/qqbot/webhook.rs）：
  - op 13（回调地址验证）：即时回签 {"plain_token","signature"}（ed25519，只签不验）；
  - 其余 op（含事件）：立即 ACK {"op":12}，再 tokio::spawn 异步投递事件字节
    到 bot.event_url（先 ACK，平台才不会判定超时——ADR-Q2）；
  - 投递 5s 超时；非 2xx / 传输错误 / 超时只记 errors，**不重试、不向平台暴露**
    （at-most-once——ADR-Q1）；
  - event_url 未配置 → 计错误丢弃；event_secret 非空 → 附 Bearer；
  - 禁用 bot 的回调路径返回 404（与转发的 503 语义区分——ADR-Q8）。

mock 观测点：
  - /_mock/events 每条事件的 ws_sessions 与 webhook_ok——平台视角的推送结果；
  - /__admin/qqbot/metrics 网关视角的投递计数（errors 只增不重试）。
"""

import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _common as C

BOT_ID = "demo-webhook"


def main() -> int:
    C.check_mock_reachable()
    prev_webhook = C.webhook_state()

    with C.Gateway() as gw:
        # ACK 时序需要一个「慢业务端」：响应延迟 3s（低于网关 5s 投递超时）。
        slow = C.EventSink(mode="slow", delay=3.0, name="slow-biz")
        fail = C.EventSink(mode="fail", name="fail-biz")
        try:
            # ---------------------------------------------------------- 1
            C.step(1, "注册机器人：event_url 指向慢业务端，平台 webhook 指向本网关")
            bot = C.add_bot(gw, id=BOT_ID, event_url=slow.url,
                            event_secret="demo-event-secret")
            C.show("bot", {k: bot.get(k) for k in
                           ("id", "event_source", "event_url", "event_secret")})
            C.set_webhook(gw.callback_url(), C.MOCK_APP_SECRET)
            C.show("平台 webhook 注册", {"url": C.webhook_state().get("url")})

            # ---------------------------------------------------------- 2
            C.step(2, "先 ACK 再投递：平台秒收 200，业务端 3s 后才回响应")
            t0 = time.time()
            C.dispatch_group_message()
            # 平台视角：mock 立即收到网关的 ACK（op 12），推送结果为 ok；
            # 业务端视角：请求已到达，但 3s 的模拟处理还没走完。
            time.sleep(0.6)
            events = C.mock_api("/events")["events"]
            last = events[-1] if events else {}
            C.show("平台视角（0.6s 时）", {k: last.get(k) for k in
                                               ("t", "webhook_ok", "ws_sessions")})
            C.show("业务端视角（0.6s 时）", {"已到达": len(slow.events),
                                             "已回响应": slow.done_count()})
            slow.wait_for_done(1)
            rec0 = slow.events[0]
            C.show("业务端视角（处理完）",
                   {"业务端响应耗时": f"{rec0['done_ts'] - rec0['ts']:.2f}s（模拟 3s 处理）",
                    "距 dispatch": f"{time.time() - t0:.2f}s"})
            C.note("若网关先同步投递再 ACK，平台 0.6s 时不会是 webhook_ok=true——"
                   "业务端还没回响应，平台却已确认收到：ACK 与投递是分开的两件事"
                   "（ACK 不计 stats，投递任务自带 guard）。")

            # ---------------------------------------------------------- 3
            C.step(3, "op 13 回调地址验证：网关即时回签")
            v = C.validate_webhook()
            resp_sig = (v.get("response") or {}).get("signature")
            C.show("validate", {"ok": v.get("ok"), "status": v.get("status"),
                                "event_ts": v.get("event_ts")})
            C.note(f"网关回签 == expected_signature：{resp_sig == v.get('expected_signature')}"
                   f"（签名的种子由 secret 循环补齐到 32 字节，签名输入是 "
                   f"event_ts + plain_token 拼接）")

            # ---------------------------------------------------------- 4
            C.step(4, "投递失败不重试：业务端恒 500 → 平台无感知、errors 只 +1")
            gw.admin(f"/qqbot/bots/{BOT_ID}", "PUT", {"event_url": fail.url})
            errors_before = C.bot_metrics(BOT_ID).get("errors", 0)
            C.dispatch_group_message()
            time.sleep(1.0)
            events = C.mock_api("/events")["events"]
            C.show("平台视角", {"webhook_ok": (events[-1] or {}).get("webhook_ok"),
                              "业务端收到的尝试次数": len(fail.events)})
            C.show("网关 metrics", {"errors（前）": errors_before,
                                    "errors（后）": C.bot_metrics(BOT_ID).get("errors", 0)})
            time.sleep(2.5)
            C.show("再等 2.5s 后", {"业务端收到的尝试次数": len(fail.events),
                                   "errors": C.bot_metrics(BOT_ID).get("errors", 0)})
            C.note("网关把事件投给了业务端一次，业务端 500 → 记错误、到此为止："
                   "没有二次投递，平台侧始终 webhook_ok=true。")

            # ---------------------------------------------------------- 5
            C.step(5, "无 event_url → 事件计错误丢弃（显式 null 清除覆盖）")
            gw.admin(f"/qqbot/bots/{BOT_ID}", "PUT",
                     {"event_url": None, "event_secret": None})
            bot = next(b for b in gw.admin("/qqbot/bots")["bots"] if b["id"] == BOT_ID)
            C.show("PUT 合并后", {k: bot.get(k) for k in ("event_url", "event_secret")})
            errors_before = C.bot_metrics(BOT_ID).get("errors", 0)
            C.dispatch_group_message()
            time.sleep(0.8)
            C.show("网关 metrics", {"errors（前）": errors_before,
                                    "errors（后）": C.bot_metrics(BOT_ID).get("errors", 0)})
            C.note("PUT 是合并语义：event_url 给 null 是「清除覆盖」，缺省是「保留」；"
                   "事件没有投递目标，记错误后丢弃，平台依然 ACK 正常。")

            # ---------------------------------------------------------- 6
            C.step(6, "禁用 bot 的回调路径：404（与转发的 503 区分）")
            gw.admin(f"/qqbot/bots/{BOT_ID}", "PUT", {"event_url": slow.url, "enabled": False})
            status, _, body = C.http_request(
                "127.0.0.1", int(gw.bind.rsplit(":", 1)[1]), "POST",
                f"/qqbot/callback/{C.MOCK_APP_ID}",
                json.dumps({"op": 0, "t": "GROUP_AT_MESSAGE_CREATE", "d": {}}).encode(),
                {"Content-Type": "application/json"})
            print(f"  平台 POST 回调（bot 已禁用）-> {status} {body[:80]!r}")
            gw.admin(f"/qqbot/bots/{BOT_ID}", "PUT", {"enabled": True})
            C.note("回调地址不应再可达：禁用走 404 而非 503（ADR-Q8）；"
                   "平台拿到的应是「地址不存在」而不是「服务不可用」。")

            # ---------------------------------------------------------- 7
            C.step(7, "收尾：删除机器人、还原平台 webhook")
            gw.admin(f"/qqbot/bots/{BOT_ID}", "DELETE")
            C.show("bot 列表", [b["id"] for b in gw.admin("/qqbot/bots")["bots"]])
        finally:
            slow.close()
            fail.close()
            C.restore_webhook(prev_webhook)
            C.show("平台 webhook 已还原为", C.webhook_state())

    print("\n演示二完成。")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except C.ApiError as e:
        print(f"\nAPI 调用失败：{e}", file=sys.stderr)
        sys.exit(1)
