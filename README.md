# dsh-whale-tui

grok-build 风格的 DeepSeek Harness 终端 TUI —— 自研完整实现，作为 DSH 插件发布。

[![CI](https://github.com/slicenferqin/dsh-whale-tui/actions/workflows/ci.yml/badge.svg)](https://github.com/slicenferqin/dsh-whale-tui/actions)

> 鲸鱼在终端里替你干活。

**当前状态：骨架（Skeleton）**。可编译、可跑 demo 模式（--demo 无需 runtime/API key）；
TS 桥与协议已立起 session/cancel 扩展点，审批/ask_user 双向通道待实现。

## 设计依据

- docs/01-grok-tui-spec.md —— grok-build pager 交互细节复刻 spec（键盘绑定、Esc 语义、审批弹窗、工具卡、主题槽位等）+ DSH 落点对照 + P0/P1/P2 优先级
- docs/02-openma-teardown.md —— openma/deepseek-harness-tui（唯一 Rust/ratatui 同类）架构拆解 + 差距清单 + 可复用/要重做清单

## 架构（与 openma 同构，但协议双向化）

    +------ plugin 模式（推荐） ---------------------------------------+
    | dsh --profile tui（宿主 Node 进程）                                |
    |   |- dsh-base + 本插件 tui-runner 行（cordis 插件, inject agents） |
    |   |    spawn(dsh-tui --attach-fds, stdio inherit + fd3/4 pipe)    |
    |   |    JsonRpcLineTransport（@deepseek-ai/dsh-sdk-protocol）       |
    |   |    initialize / session/prompt / session/cancel / shutdown    |
    |   |    session.event / session.status / subagent.* 通知转发        |
    |   |    （审批/ask_user 双向通道：TODO，见 docs/02 第12节）         |
    |   +- dsh-tui（Rust/ratatui，TTY 归 TUI，fd3/4 走协议）             |
    +------------------------------------------------------------------+

    +------ standalone（未来）------------------------------------------+
    | dsh-tui --runtime-bin <bin>：自己 spawn SDK runtime 子进程         |
    +------------------------------------------------------------------+

## 构建与运行

    cargo build                # 编译
    cargo run -- --demo        # 脚本化 demo（无需 runtime/API key）
    cargo run -- --demo --theme light

插件模式（需要已配置的 dsh 0.1.0-rc.6）：

    scripts/build-npm.sh                      # cargo release + stage vendor 二进制 + npm pack
    dsh plugin --profile tui add ./dist/*.tgz # 安装到 tui profile
    dsh --profile tui                         # 启动

## 社区发现

仓库带 [dsh-plugin](https://github.com/topics/dsh-plugin) topic；发布 npm 后可被 [awesome-dsh-plugin](https://github.com/awesome-dsh-plugin/awesome-dsh-plugin) 精选列表与 dsh-find-plugin 检索收录。

## 键位（骨架已实现的部分）

| 键 | 行为 |
|---|---|
| Enter | 空闲=发送；turn 中=排队（queue） |
| Alt+Enter | send-now（取消当前 turn 并发送）——协议侧已备好 session/cancel |
| Esc | turn 中=取消；空闲=双击清空/双击 rewind（800ms 窗口） |
| Ctrl+C | 先清草稿，再按取消 |
| Tab | scrollback 与 prompt 焦点 |
| 上下, h/l | 选条目 / 折叠 |
| PageUp/PageDown | 翻页 |
| Ctrl+E | 折叠/展开 thinking（占位） |
| Ctrl+T | 切主题（dark/light） |
| Ctrl+Q / Ctrl+D | 退出 |

## 目录

    src/
      main.rs       CLI、终端守卫、事件循环
      bus.rs        AppEvent / Cmd
      proto.rs      NDJSON JSON-RPC（attach/spawn、request/notify、超时、kill/shutdown）
      app.rs        应用状态：RunState、Esc 状态机、follow-up 队列、焦点
      transcript.rs 会话事件 → 渲染 Cell（消息/思考/工具卡）
      ui.rs         ratatui 渲染：状态栏/scrollback/输入框/快捷键栏
      theme.rs      grok 式颜色槽位（深/浅两主题）
      demo.rs       脚本化 demo 流
    npm/            TS 桥插件（cordis.patch.yml + lib/index.js + bin 入口）
    scripts/        构建与打包（build-npm.sh / package-native.mjs）

## 下一步（按 spec 的 P0 顺序）

1. TS 桥：审批 waterfall → approval/request 通知、ask_user_question → 问题卡通知（双向化）
2. Rust：权限弹窗与问题卡 UI（1–9 直选、Esc 停驻）、plan 审批视图 a/s/c/y/q
3. Esc 状态机补全（取消宽限期、rewind 选择器 → ctx.sessions.fork 回放）
4. 工具卡分类渲染（diff/bash/web）+ thinking 折叠 + 状态栏用量
5. /resume（JSONL 直读 + zstd 多帧）、/model（tui/catalog）、/compact
6. 终端探测与剪贴板三路由


