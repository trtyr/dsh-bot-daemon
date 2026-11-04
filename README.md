# dsh-bot-daemon

BOT 团队运行时 daemon——为 DeepSeek Harness (dsh) 管理一支 BOT 团队。

## 能力

- **Team 生命周期**：create / list / archive / pause / resume（快照原子写：tmp + rename）
- **BOT 拉起**：headless 方式 spawn `dsh --profile <bot> --json`，解析事件流取最终答案
- **身份反查**：BOT 内部再调 `bot_send` 时按调用方 PID 自动落到同一个 Team——链式调用不串队
- **会话隔离**：同一角色在不同 Team 各自独立的 `.session-id`
- **长期记忆共享**：`~/.dsh/agents/<bot>/memory.md` 跨 Team 共享
- **工程化**：9 个结构化错误码、分级日志（stdout + `~/.dsh/bot-daemon/daemon.log`）、`GET /` 自描述接口

## 安装

```sh
cargo install dsh-bot-daemon
```

## 用法

```sh
dsh-bot-daemon                     # 启动（默认 127.0.0.1:18923）
curl -X POST localhost:18923/teams -H 'content-type: application/json' \
     -d '{"label":"我的任务","origin":"jiangwantang"}'
curl localhost:18923/teams         # 列出
curl localhost:18923/health        # 健康检查
```

配套：主会话装 `dsh-bot-bridge` 插件后，对话中即可 `bot_send` / `bot_list`。

## 前置

- 已安装并登录 `dsh`（DeepSeek Harness）
- 每个BOT 一个 profile（`~/.dsh/profiles/<bot>/`）+ 人设（`~/.dsh/agents/<bot>/AGENTS.md`）

## License

MIT
