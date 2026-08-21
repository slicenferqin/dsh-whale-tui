# dsh-whale-tui

DeepSeek Harness（DSH）的原生终端用户界面。基于 Rust / ratatui 实现，以 DSH 插件形式分发，交互契约参照 [grok-build](https://github.com/xai-org/grok-build) 的终端设计。

[![CI](https://github.com/slicenferqin/dsh-whale-tui/actions/workflows/ci.yml/badge.svg)](https://github.com/slicenferqin/dsh-whale-tui/actions)

## 概述

dsh-whale-tui 将 DSH 的完整 agent 能力带入终端：真实会话、流式事件、工具审批、模型与服务商管理、会话恢复与回溯，全部经由单一 JSON-RPC 通道与宿主通信。TUI 进程持有 TTY，宿主进程持有 runtime，两者职责清晰分离。

内置 demo 模式（`--demo`）无需 runtime 与 API key 即可体验全部交互；`--dump-frame` 提供无 TTY 的确定性布局检查。

## 功能

- **会话生命周期**：initialize / prompt / cancel / shutdown，流式解析 assistant chunk、usage、turn/end 事件
- **审批与提问**：工具审批弹窗、ask_user 问答卡（单选 / 多选 / 自由文本），双向请求-响应通道
- **Esc 状态机**：turn 中取消；空闲时双击清空草稿 / 双击 rewind 回溯（800ms 窗口）
- **模型与服务商**：`/model` 模型切换；`/provider` 服务商面板（列表、key 状态、新增、编辑 key、删除）；新增向导内置 pi-ai 目录（37 个服务商预设）与自定义 OpenAI/Anthropic 兼容端点，key 写入 credentials 后宿主热加载
- **会话恢复**：`/resume` 直读 `~/.dsh/sessions` 的 JSONL（zstd 多帧），恢复后回放完整历史并接续活会话
- **权限模式**：Normal / Plan / Always-approve 循环切换（Shift+Tab），计划审查（a/s/c/y/q）
- **任务视图**：Ctrl+T 任务清单快照，Ctrl+G 后台任务与活跃子代理
- **鼠标交互**：弹窗选项可直接点击选择（与键盘同一代码路径），滚轮在弹窗内翻动选项；`/mouse` 开关鼠标上报，默认关闭以保证终端原生选区复制
- **剪贴板**：native → tmux → OSC52 三级路由，备份落盘 `~/.dsh/last-copy.txt`
- **终端适配**：启动探测 TERM_PROGRAM / TMUX（VS Code 家族自动改用 Ctrl+D 退出提示），色彩按终端能力量化（truecolor / 256 / 16），主题实时预览并持久化

## 架构

    +------ plugin 模式（推荐） ---------------------------------------+
    | dsh --profile tui（宿主 Node 进程）                                |
    |   |- dsh-base + 本插件 tui-runner 行（cordis 插件, inject agents） |
    |   |    spawn(dsh-tui --attach-fds, stdio inherit + fd3/4 pipe)    |
    |   |    JsonRpcLineTransport（@deepseek-ai/dsh-sdk-protocol）       |
    |   |    initialize / session/prompt / session/cancel / shutdown    |
    |   |    session.event / session.status / subagent.* 通知转发        |
    |   |    审批 / ask_user 双向请求；模型、权限、压缩、回溯扩展         |
    |   +- dsh-tui（Rust/ratatui，TTY 归 TUI，fd3/4 走协议）             |
    +------------------------------------------------------------------+

    +------ standalone（未来）------------------------------------------+
    | dsh-tui --runtime-bin <bin>：自己 spawn SDK runtime 子进程         |
    +------------------------------------------------------------------+

## 安装与运行

前置依赖：Rust 工具链、Node.js、全局 dsh 0.1.0-rc.6。发布包为原生二进制，当前支持 macOS（Apple Silicon / Intel）与 Linux x64；Windows 请使用 WSL。

    npm install -g @deepseek-ai/dsh@0.1.0-rc.6

    scripts/build-npm.sh                       # cargo release + stage vendor 二进制 + npm pack
    dsh plugin --profile tui add ./dist/*.tgz  # 安装到 tui profile
    dsh --profile tui                          # 启动

本地开发：

    cargo build                                # 编译
    cargo test                                 # 单元测试（142 项）
    cargo run -- --demo                        # 脚本化 demo（无需 runtime/API key）
    cargo run -- --demo --theme light
    cargo run -- --dump-frame 100x30           # 无 TTY 的确定性布局检查

## 配置

TUI 的默认 provider / model / theme 从 `~/.dsh/settings.yaml` 的 `dsh-whale-tui:` 块读取（provider/model 缺省回退到全局 `agent-default-model:`，再回退 stock）：

    dsh-whale-tui:
      provider: opencode-go
      model: deepseek-v4-flash
      theme: dark

命令行 `--provider` / `--model` 优先级最高。`/theme` 的选择会持久化到该块。

## 键位

| 键 | 行为 |
|---|---|
| Enter | 单行模式：空闲发送、turn 中排队；多行模式：换行 |
| Alt+Enter | 发送多行输入；turn 中先取消当前 turn 再发送 |
| Shift+Enter | 不切模式直接插入换行 |
| Ctrl+M | prompt 聚焦时切换多行模式；scrollback 聚焦时打开模型选择器 |
| Esc | turn 中取消；空闲时双击清空 / 双击 rewind（800ms 窗口） |
| Ctrl+C | 先清草稿，再按取消 |
| Tab | scrollback 与 prompt 焦点切换 |
| ← / →，Alt+← / Alt+→ | 移动光标 / 按词移动 |
| Ctrl+A / Ctrl+E，Home / End | 行首 / 行尾 |
| Ctrl+W，Alt+Backspace / Alt+D | 删除前 / 后一个词 |
| Ctrl+U / Ctrl+K | 删到行首 / 行尾（scrollback 聚焦时为半页滚动） |
| Ctrl+Z | 撤销上一次编辑 |
| ↑ / ↓ | 草稿内移动 / 浏览输入历史；scrollback 中选择条目 |
| h / l，e | 折叠 / 展开 / 切换选中条目 |
| g / G | 跳到首个 / 最后一个条目 |
| Shift+H / Shift+L | 上一 / 下一个 turn（用户提问） |
| Shift+K / Shift+J | 上一 / 下一条 assistant 回复 |
| Ctrl+J / Ctrl+K | 上 / 下滚一行（不动选中） |
| Shift+E | 全部折叠 / 全部展开 |
| Enter / Ctrl+F | 全屏查看选中块 |
| Shift+Tab | 循环切换 Normal / Plan / Always-approve 权限模式 |
| Ctrl+O | 切换 always-approve |
| Shift+↑ / Shift+↓，PageUp / PageDown | 滚动会话视口（任何焦点） |
| Ctrl+T | todos 面板：agent 任务清单快照（y 复制 · q/Esc 关闭） |
| Ctrl+G | tasks 面板：后台任务 + 活跃子代理（r 刷新） |
| Ctrl+P / ? | 命令面板（slash 命令 + 常用操作，可过滤） |
| Ctrl+X / Ctrl+. | 快捷键速查 |
| Ctrl+N ×2 | 新会话（双击确认） |
| Ctrl+Q ×2 / Ctrl+D | 退出（双击确认） |
| z（问题卡内） | 自由文本回答（Enter 提交 · Esc 返回选项） |
| y / Y | 复制选中块内容 / 元数据 |
| 鼠标点击 | 弹窗内直接选择选项（等价于方向键 + Enter） |
| 鼠标滚轮 | 弹窗内翻动选项；会话区滚动视口 |
| `/mouse` | 开关鼠标上报；开启后选区复制需按住 Shift |

## Slash 命令

| 命令 | 行为 |
|---|---|
| /provider | 服务商面板：列表（key 状态）、a 新增、e 编辑 key、d 删除；`/provider add` 打开新增向导（内置 pi-ai 目录 / 自定义端点），写入 `llm-pi-ai` 块 + credentials 后宿主热加载 |
| /model | 模型选择器 |
| /resume | 会话选择器：恢复历史并接续活会话 |
| /new (/clear) | 新会话 |
| /exit (/quit) | 退出 |
| /help | 命令列表 |
| /session-info (/context /status /info) | 会话明细：模型 / 目录 / turn 数 / token 用量 |
| /theme | 主题实时预览（方向键切换 · Enter 保持 · Esc 还原），选择持久化 |
| /copy | 复制最近回复 |
| /compact | 压缩当前会话历史 |
| /mouse | 开关鼠标上报 |

## 项目结构

    src/
      main.rs       CLI、终端守卫、事件循环
      bus.rs        AppEvent / Cmd
      proto.rs      NDJSON JSON-RPC（attach/spawn、request/notify、超时、kill/shutdown）
      app.rs        应用状态：RunState、Esc 状态机、follow-up 队列、焦点、弹窗命中区
      transcript.rs 会话事件 → 渲染 Cell（消息/思考/工具卡）
      ui.rs         ratatui 渲染：状态栏/scrollback/输入框/快捷键栏/弹窗
      theme.rs      grok 式颜色槽位（深/浅两主题）
      settings.rs   settings.yaml 读写（dsh-whale-tui 块）
      resume.rs     会话恢复（JSONL zstd 解析与回放）
      demo.rs       脚本化 demo 流
    npm/            桥插件（cordis.patch.yml + lib/index.js + bin 入口）
    scripts/        构建与打包（build-npm.sh / package-native.mjs）
    docs/           设计文档与交互原型

## 设计文档

- docs/01-grok-tui-spec.md — grok-build pager 交互细节复刻 spec（键盘绑定、Esc 语义、审批弹窗、工具卡、主题槽位）+ DSH 落点对照 + 优先级
- docs/02-openma-teardown.md — openma/deepseek-harness-tui（唯一 Rust/ratatui 同类）架构拆解与差距清单
- docs/04-dsh-capability-map.md — deepseek-harness 0.1.0-rc.6 能力地图：29 个 `ctx.*` seam、13 个 session projection、19 个模型侧工具，及 DSH 独有能力的 TUI 落点
- docs/prototypes/provider-setup.html — provider 管理弹窗的交互原型

## 社区发现

仓库带 [dsh-plugin](https://github.com/topics/dsh-plugin) topic；发布 npm 后可被 [awesome-dsh-plugin](https://github.com/awesome-dsh-plugin/awesome-dsh-plugin) 精选列表与 dsh-find-plugin 检索收录。
