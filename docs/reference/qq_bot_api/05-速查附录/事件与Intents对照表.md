# 事件与 Intents 对照表

## Intents 位定义

事件订阅的 intents 标记位，每位对应一类事件（摘自《事件订阅通用数据结构》）。

| Intent | 位移 | 包含事件 | 说明 |
| --- | --- | --- | --- |
| GUILDS | 1 << 0 | GUILD_CREATE, GUILD_UPDATE, GUILD_DELETE, CHANNEL_CREATE, CHANNEL_UPDATE, CHANNEL_DELETE | 当机器人加入新guild时；当channel被创建时；当channel被更新时；当guild资料发生变更时；当channel被删除时；当机器人退出guild时 |
| GUILD_MEMBERS | 1 << 1 | GUILD_MEMBER_ADD, GUILD_MEMBER_UPDATE, GUILD_MEMBER_REMOVE | 当成员加入时；当成员被移除时；当成员资料变更时 |
| GUILD_MESSAGES | 1 << 9 | MESSAGE_CREATE, MESSAGE_DELETE | 删除（撤回）消息事件；发送消息事件，代表频道内的全部消息，而不只是 at 机器人的消息。内容与 AT_MESSAGE_CREATE 相同 |
| GUILD_MESSAGE_REACTIONS | 1 << 10 | MESSAGE_REACTION_ADD, MESSAGE_REACTION_REMOVE | 为消息删除表情表态；为消息添加表情表态 |
| DIRECT_MESSAGE | 1 << 12 | DIRECT_MESSAGE_CREATE, DIRECT_MESSAGE_DELETE | 删除（撤回）消息事件；当收到用户发给机器人的私信消息时 |
| GROUP_AND_C2C_EVENT | 1 << 25 | C2C_MESSAGE_CREATE, FRIEND_ADD, FRIEND_DEL, C2C_MSG_REJECT, C2C_MSG_RECEIVE, GROUP_AT_MESSAGE_CREATE, GROUP_ADD_ROBOT, GROUP_DEL_ROBOT, GROUP_MSG_REJECT, GROUP_MSG_RECEIVE | 用户在群里@机器人时收到的消息；群管理员主动在机器人资料页操作开启通知；用户在机器人资料卡手动关闭"主动消息"推送；群管理员主动在机器人资料页操作关闭通知；用户添加使用机器人；用户在机器人资料卡手动开启"主动消息"推送开关；用户删除机器人；机器人被移出群聊；机器人被添加到群聊；用户单聊发消息给机器人时候 |
| INTERACTION | 1 << 26 | INTERACTION_CREATE | 互动事件创建时 |
| MESSAGE_AUDIT | 1 << 27 | MESSAGE_AUDIT_PASS, MESSAGE_AUDIT_REJECT | 消息审核不通过；消息审核通过 |
| FORUMS_EVENT | 1 << 28 | FORUM_THREAD_CREATE, FORUM_THREAD_UPDATE, FORUM_THREAD_DELETE, FORUM_POST_CREATE, FORUM_POST_DELETE, FORUM_REPLY_CREATE, FORUM_REPLY_DELETE, FORUM_PUBLISH_AUDIT_RESULT | 当用户回复评论时；当用户删除帖子时；当用户创建主题时；当用户创建帖子时；当用户发表审核通过时；当用户删除主题时；当用户更新主题时 |
| AUDIO_ACTION | 1 << 29 | AUDIO_START, AUDIO_FINISH, AUDIO_ON_MIC, AUDIO_OFF_MIC | 音频播放结束时；音频开始播放时；下麦时；上麦时 |
| PUBLIC_GUILD_MESSAGES | 1 << 30 | AT_MESSAGE_CREATE, PUBLIC_MESSAGE_DELETE | 当频道的消息被删除时；当收到@机器人的消息时 |

## 事件一览（文档中显式定义的事件）

| 事件名 | Intent | 事件体文档 |
| --- | --- | --- |
| INTERACTION_CREATE | INTERACTION (1<<26) | [互动事件](../03-事件/互动事件/互动事件.md) |
| C2C_MESSAGE_CREATE | GROUP_AND_C2C_EVENT (1<<25) | [单聊消息事件](../03-事件/单聊事件/单聊消息事件.md) |
| C2C_MSG_REJECT | GROUP_AND_C2C_EVENT (1<<25) | [单聊消息接收关闭](../03-事件/单聊事件/单聊消息接收关闭.md) |
| C2C_MSG_RECEIVE | GROUP_AND_C2C_EVENT (1<<25) | [单聊消息接收开启](../03-事件/单聊事件/单聊消息接收开启.md) |
| FRIEND_DEL | GROUP_AND_C2C_EVENT (1<<25) | [用户删除好友](../03-事件/单聊事件/用户删除好友.md) |
| FRIEND_ADD | GROUP_AND_C2C_EVENT (1<<25) | [用户添加好友](../03-事件/单聊事件/用户添加好友.md) |
| GROUP_AT_MESSAGE_CREATE | GROUP_AND_C2C_EVENT (1<<25) | [群@机器人消息](../03-事件/群聊事件/群@机器人消息.md) |
| GROUP_MESSAGE_CREATE | GROUP_AND_C2C_EVENT (1<<25) | [群消息（全量模式）](../03-事件/群聊事件/群消息-全量模式.md) |
| CHANNEL_CREATE | GUILDS (1<<0) | [子频道创建](../03-事件/频道事件/子频道创建.md) |
| CHANNEL_DELETE | GUILDS (1<<0) | [子频道删除](../03-事件/频道事件/子频道删除.md) |
| CHANNEL_UPDATE | GUILDS (1<<0) | [子频道更新](../03-事件/频道事件/子频道更新.md) |
| GUILD_CREATE | GUILDS (1<<0) | [频道创建](../03-事件/频道事件/频道创建.md) |
| GUILD_UPDATE | GUILDS (1<<0) | [频道更新](../03-事件/频道事件/频道更新.md) |
| GUILD_DELETE | GUILDS (1<<0) | [频道解散](../03-事件/频道事件/频道解散.md) |

## OpCode 含义

网关上下行消息的 opcode（摘自《事件订阅通用数据结构》），mock 的 WebSocket 服务需要按此协议实现。

| CODE | 名称 | 接入方式 | 客户端行为 | 描述 |
| --- | --- | --- | --- | --- |
| 0 | Dispatch | webhook/websocket | Receive | 服务端进行消息推送 |
| 2 | Identify | websocket | Send | 客户端发送鉴权 |
| 6 | Resume | websocket | Send | 客户端恢复连接 |
| 7 | Reconnect | websocket | Receive | 服务端通知客户端重新连接 |
| 9 | Invalid Session | websocket | Receive | 当 identify 或 resume 的时候，如果参数有错，服务端会返回该消息 |
| 10 | Hello | websocket | Receive | 当客户端与网关建立 ws 连接之后，网关下发的第一条消息 |
| 12 | HTTP Callback ACK | webhook | Reply | 仅用于 http 回调模式的回包，代表机器人收到了平台推送的数据 |
