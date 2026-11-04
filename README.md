# dsh-bot-daemon

BOT 群运行时 daemon —— 为 DeepSeek Harness (dsh) 管理一个「群」。

## 它是什么

**群 = 一次会话的投影。** 会话开就建群，会话断就归档（D020）。
群里有一到几个 BOT，群主（也就是那个会话）负责派活。

BOT 是独立的 dsh 进程，一次派活 = 一个 turn，跑完退出。
所以 daemon 干三件事：**起进程、收答案、把答案存在没人也会活的地方**。

## 能力

- **群生命周期**（D026）：`POST /groups/resolve` 按会话找群/建群；`pause`/`resume`/`archive`；三态 `active / paused / archived`
- **同步与异步派活**（D019）：`POST /run` 默认等答完；`async:true` 立刻返回 `jobId`
- **job 表与信箱**：`GET /jobs/:id`、`GET /jobs?groupId=&status=`；结果双写内存表与 `groups/<id>/workspace/mailbox/<jobId>-<bot>.md`
- **派活即入群**（D028）：BOT 被派活就真写进 `meta.json` 的成员表，不靠会话反推
- **群史**（D021）：`groups/<id>/log.jsonl` 只记事件（`member.joined` / `task.returned` / `task.failed`），append-only
- **身份反查**：BOT 再调 `/run` 时按调用方 PID 落到同一个群（D013）
- **中文名解析**：`钟诗雨` 自动解析成 `zhongshiyu`（扫 `AGENTS.md` 首行，进程内缓存）
- **不设并发上限**（D023）：不拦、不排队，让 OS 管内存；`running` 表保留作 PID 反查
- **工程化**：9 个结构化错误码、分级日志、`GET /` 自描述接口

## 安装

```sh
cargo install dsh-bot-daemon
```

## 用法

```sh
dsh-bot-daemon                       # 启动（默认 127.0.0.1:18923）

# 按会话找群（没有就建）
curl -X POST localhost:18923/groups/resolve \
     -H 'content-type: application/json' \
     -d '{"originSession":"<session-id>","label":"我的任务"}'

# 同步派活（等答完）
curl -X POST localhost:18923/run -H 'content-type: application/json' \
     -d '{"name":"钟诗雨","message":"只回收到","groupId":"G-xxxxxxxx"}'

# 异步派活（立刻返回 jobId）
curl -X POST localhost:18923/run -H 'content-type: application/json' \
     -d '{"name":"zhongshiyu","message":"…","groupId":"G-xxxxxxxx","async":true}'
curl localhost:18923/jobs/job-xxxxxxxx    # 取结果
```

配套：`dsh-bot-bridge` 插件把这些包成 `bot_talk` / `bot_dispatch` / `bot_wait` /
`bot_poll` / `bot_inbox` / `group_whoami`，并**按 profile 的 `role` 决定发哪些工具**。

## 前置

- 已安装并登录 `dsh`（DeepSeek Harness）
- 每个 BOT 一个 profile（`~/.dsh/profiles/<bot>/`）+ 人设（`~/.dsh/agents/<bot>/AGENTS.md`）

## 设计文档

在 `~/.devforge/plantree/dsh-bot-bus/plans/001-bot-team-runtime/`：
决策 D018–D031、群模型、异步模型、插件工具面、实施完成报告。

## License

MIT
