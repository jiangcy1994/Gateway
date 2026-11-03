#!/usr/bin/env python3
"""演示一：OpenAPI 透传 —— token 缓存、凭证替换、状态码映射、错误码透传。

网关行为（/qqbot/{app_id}/... 的转发路径，src/qqbot/forward.rs）：
  - 首段 app_id 是路由键：查不到 bot -> 404，bot 禁用 -> 503，token 拉取失败 -> 502；
  - 自动取 token（POST /app/getAppAccessToken），成功后注入
    Authorization: QQBot <token>，**客户端的 Authorization 一律被替换**；
  - token 被缓存：同一 bot 的后续请求不再向平台换 token；
  - 平台业务错误透传不改码（HTTP 200 + err_code 原样回送）。

mock 观测点：/_mock/history（明文调用历史，含 path/status/req_body）——
换 token 接口出现几次，就是网关向平台拉了几次。
"""

import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _common as C

BOT_ID = "demo-openapi"


def main() -> int:
    C.check_mock_reachable()
    # 本演示只走出站通道，不订阅平台推送：webhook 注册保持原样（不重定向）。
    prev_webhook = C.webhook_state()

    with C.Gateway() as gw:
        # 记录技术交流群的入群状态，演示末尾还原（seed 世界里该群为「未加入」，
        # 但共享 mock 的实际状态可能已被此前的试用改写——按「找到什么样、
        # 走时什么样」的约定保存）。
        def tech_joined() -> bool:
            status, _, _ = gw.openapi(
                "GET", f"/qqbot/{C.MOCK_APP_ID}/v2/groups/{C.GROUP_TECH}/bot_state")
            return status == 200

        joined_at_start = None
        try:
            # ---------------------------------------------------------- 1
            C.step(1, "注册机器人（api_base 指向 mock）")
            bot = C.add_bot(gw, id=BOT_ID)
            C.show("bot", {k: bot.get(k) for k in
                           ("id", "app_id", "api_base", "event_url", "enabled")})

            # ---------------------------------------------------------- 2
            C.step(2, "token 缓存：两次 OpenAPI 调用只换一次 token")
            def me(tag: str):
                status, headers, body = gw.openapi(
                    "GET", f"/qqbot/{C.MOCK_APP_ID}/users/@me",
                    headers={"Authorization": "Bearer client-fake-token"})
                who = json.loads(body)
                print(f"  {tag}: -> {status} x-qqbot-bot={headers.get('x-qqbot-bot')}"
                      f" username={who.get('username')}")
                return status

            n_before = C.token_fetch_count()
            me("第 1 次调用")
            n_after_first = C.token_fetch_count()
            me("第 2 次调用")
            n_after_second = C.token_fetch_count()
            C.show(" getAppAccessToken 调用次数（history 累计值）",
                   {"调用前": n_before, "第 1 次后": n_after_first,
                    "第 2 次后": n_after_second})
            C.note(f"第一次调用新增 {n_after_first - n_before} 次换 token、"
                   f"第二次新增 {n_after_second - n_after_first} 次："
                   "第二次命中网关内的 token 缓存，没有再向平台换。")
            C.note("history 是 mock 的全局明文流水（跨演示累积、包含管理端调用），"
                   "所以这里看增量而不是绝对值；网关是本演示新起的进程，没有历史缓存。")

            # ---------------------------------------------------------- 3
            C.step(3, "凭证替换对照：同一假凭证，直连 mock 401 vs 经网关 200")
            status_direct, _, body_direct = C.http_request(
                "127.0.0.1", 8080, "GET", "/users/@me",
                headers={"Authorization": "Bearer client-fake-token"})
            status_gw = me("经网关    ")
            print(f"  直连 mock（Bearer client-fake-token）-> {status_direct}"
                  f" {body_direct[:60]!r}")
            print(f"  经网关  （Bearer client-fake-token）-> {status_gw}")
            C.note("网关把客户端鉴权头整个换成 QQBot <平台 token>，"
                   "所以同样的假凭证经网关是 200。")

            # ---------------------------------------------------------- 4
            C.step(4, "状态码映射：未知 app_id 404、禁用 bot 503")
            status, _, body = gw.openapi("GET", f"/qqbot/9999999999/users/@me")
            print(f"  未注册 app_id 9999999999 -> {status} {body[:72]!r}")
            gw.admin(f"/qqbot/bots/{BOT_ID}", "PUT", {"enabled": False})
            status, _, body = gw.openapi("GET", f"/qqbot/{C.MOCK_APP_ID}/users/@me")
            print(f"  禁用 bot（enabled=false）-> {status} {body[:72]!r}")
            gw.admin(f"/qqbot/bots/{BOT_ID}", "PUT", {"enabled": True})
            C.note("转发路径上「未注册」与「禁用」语义不同（404 vs 503）；"
                   "回调路径上禁用同样是 404（见演示二）。")

            # ---------------------------------------------------------- 5
            C.step(5, "业务错误码透传：群未加入 40034101 / 一次性注入 304061")
            # 40034101 依赖「机器人不在群内」这个世界状态：先用管理端把它钉死，
            # 免得共享 mock 的实际入群状态与 seed 世界不一致（本套演示不重置世界）。
            joined_at_start = tech_joined()
            C.show("技术交流群", {"机器人已入群": joined_at_start})
            if joined_at_start:
                C.set_bot_joined(C.GROUP_TECH, False)
                C.note("经管理端把机器人移出技术交流群（seed 世界即未加入）；"
                       "退群会向平台 webhook 推 GROUP_DEL_ROBOT——当前 webhook 不可达，无副作用。")
            status, _, body = gw.openapi(
                "GET", f"/qqbot/{C.MOCK_APP_ID}/v2/groups/{C.GROUP_TECH}/bot_state")
            print(f"  GET bot_state（未入群）-> HTTP {status} {body[:80]!r}")
            status, _, body = gw.openapi(
                "POST", f"/qqbot/{C.MOCK_APP_ID}/v2/groups/{C.GROUP_TECH}/messages",
                {"msg_type": 0, "content": "你好"})
            print(f"  向未入群的技术交流群发主动消息 -> HTTP {status} {body[:100]!r}")
            C.note("平台业务错误是 HTTP 200 + err_code，网关原样透传不改码。")

            C.mock_api("/inject", "POST", {"error_code": 304061})
            status, _, body = gw.openapi(
                "POST", f"/qqbot/{C.MOCK_APP_ID}/v2/groups/{C.GROUP_READER}/messages",
                {"msg_type": 0, "content": "这条会被注入错误码"})
            print(f"  注入 304061 后的下一条 -> HTTP {status} {body[:100]!r}")
            status, _, body = gw.openapi(
                "POST", f"/qqbot/{C.MOCK_APP_ID}/v2/groups/{C.GROUP_READER}/messages",
                {"msg_type": 0, "content": "注入是一次性的，这条恢复"})
            print(f"  再下一条            -> HTTP {status} {body[:100]!r}")
            C.note("网关不解释 err_code：它只管转发与鉴权，业务语义属于平台。")

            # ---------------------------------------------------------- 6
            C.step(6, "被动回复与去重：msg_id 回复、重复 msg_id 40054005、撤回")
            rec = C.dispatch_group_message()
            msg_id = rec["msg_id"]
            C.show("dispatch", {"msg_id": msg_id, "records": rec["records"]})
            # 平台 webhook 仍指向演示前的地址（不可达），事件推送必然失败——
            # 但 msg_id 与推送结果无关，被动回复照样能用。
            status, _, body = gw.openapi(
                "POST", f"/qqbot/{C.MOCK_APP_ID}/v2/groups/{C.GROUP_READER}/messages",
                {"msg_type": 0, "content": "推荐《项塔兰》", "msg_id": msg_id})
            print(f"  被动回复（msg_id）-> {status} {body[:90]!r}")
            reply = json.loads(body) if status == 200 else {}
            status, _, body = gw.openapi(
                "POST", f"/qqbot/{C.MOCK_APP_ID}/v2/groups/{C.GROUP_READER}/messages",
                {"msg_type": 0, "content": "重复回复", "msg_id": msg_id})
            print(f"  同 msg_id 再发    -> {status} {body[:90]!r}")

            message_id = reply.get("id") or rec["msg_id"]
            status, _, body = gw.openapi(
                "DELETE",
                f"/qqbot/{C.MOCK_APP_ID}/v2/groups/{C.GROUP_READER}/messages/{message_id}")
            print(f"  撤回该消息       -> {status} {body[:70]!r}")
            status, _, body = gw.openapi(
                "DELETE",
                f"/qqbot/{C.MOCK_APP_ID}/v2/groups/{C.GROUP_READER}/messages/{message_id}")
            print(f"  二次撤回         -> {status} {body[:70]!r}")
            C.note("去重键是 msg_id+msg_seq（默认 seq=1）；撤回是幂等的，"
                   "已撤回再撤返回 304024。")

            # ---------------------------------------------------------- 7
            C.step(7, "面板对照：bot 透传统计")
            C.show("metrics", C.bot_metrics(BOT_ID))
            C.note("ACK 与投递在演示二展开；这里 total_requests 只计转发。")

            # ---------------------------------------------------------- 8
            C.step(8, "清理：删除机器人")
            gw.admin(f"/qqbot/bots/{BOT_ID}", "DELETE")
            C.show("bot 列表", [b["id"] for b in gw.admin("/qqbot/bots")["bots"]])
        finally:
            # 演示全程未重定向 webhook，但仍校验一次值未被动过。
            now = C.webhook_state()
            if now != prev_webhook:
                C.restore_webhook(prev_webhook)
                C.show("平台 webhook 已还原为", C.webhook_state())
            # 步骤 5 曾为确保 40034101 把机器人移出技术交流群——按演示前的原样还回去。
            # 此处 bot 已删除，不再经网关校验，直接用管理端回执（joined）确认。
            if joined_at_start:
                back = C.set_bot_joined(C.GROUP_TECH, True)
                C.show("技术交流群已还原为", {"机器人已入群": back.get("joined")})

    print("\n演示一完成。")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except C.ApiError as e:
        print(f"\nAPI 调用失败：{e}", file=sys.stderr)
        sys.exit(1)
