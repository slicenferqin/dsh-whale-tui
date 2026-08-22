# dsh-whale-tui

grok-build 风格的 DeepSeek Harness 终端 TUI —— 自研完整实现，作为 DSH 插件发布。

[![CI](https://github.com/slicenferqin/dsh-whale-tui/actions/workflows/ci.yml/badge.svg)](https://github.com/slicenferqin/dsh-whale-tui/actions)

> 鲸鱼在终端里替你干活。

**当前状态：P1 全量完成**。真实会话事件、Esc 状态机、审批与 ask_user 双向通道、模型与权限切换、会话恢复/回溯、压缩、计划审查、任务与子代理视图均已接入。demo 模式（--demo）无需 runtime/API key。

## 与 Web UI、桥接方案的关系

官方 Web UI surface 最全（32 个 `dsh-client-ui-*`，浏览器形态）；社区 [dsh-grok-tui](https://github.com/chen-001/dsh-grok-tui) 把 grok 官方 TUI 二进制桥接进 dsh web——真 grok 手感，但 DSH 独有 surface 与用量指标进不了 grok UI，需 herdr 侧栏 / tmux 面板外挂。本项目是原生 Rust TUI：投影驱动渲染 DSH 语义（GoalBar、contextPressure、压缩影子化、审批/ask_user 双向透传），终端原生（SSH / tmux / 纯键盘）。完整对比见仓库 README「三条路线的对比」。

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
    |   |    审批 / ask_user 双向请求；模型、权限、压缩、回溯扩展         |
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

## 模型默认路由

TUI 的默认 provider/model 从 ~/.dsh/settings.yaml 的 dsh-whale-tui: 块读取（缺省回退到全局 agent-default-model:，再回退 stock）：

    dsh-whale-tui:
      provider: opencode-go
      model: deepseek-v4-flash

命令行 --provider / --model 优先级最高。

## 键位

| 键 | 行为 |
|---|---|
| Enter | 单行模式：空闲发送、turn 中排队；多行模式：换行 |
| Alt+Enter | 发送多行输入；turn 中先取消当前 turn 再发送 |
| Ctrl+M | prompt 聚焦时切换多行模式；scrollback 聚焦时打开模型选择器 |
| Shift+Enter | 不切模式直接插入换行 |
| Esc | turn 中取消；空闲时双击清空 / 双击 rewind（800ms 窗口） |
| Ctrl+C | 先清草稿，再按取消 |
| Tab | scrollback 与 prompt 焦点切换 |
| ↑ / ↓ | prompt 中逐条浏览输入历史；scrollback 中选择条目 |
| h / ←，l / →，e | 折叠、展开、切换选中条目 |
| g / G，Home / End | 跳到首个 / 最后一个条目 |
| Shift+H / Shift+L | 上一 / 下一个 turn（用户提问） |
| Shift+K / Shift+J | 上一 / 下一条 assistant 回复 |
| Ctrl+K / Ctrl+J | 上 / 下滚一行（不动选中） |
| Ctrl+U / Ctrl+D | 上 / 下滚半页（scrollback 聚焦时；composer 里 Ctrl+D 是退出） |
| Shift+E | 全部折叠 / 全部展开 |
| Enter / Ctrl+F | 全屏查看选中块 |
| Ctrl+O | 切换 always-approve |
| Shift+Tab | 循环切换 Normal / Plan / Always-approve 权限模式 |
| PageUp / PageDown，鼠标滚轮 | 翻动会话视口 |
| Ctrl+E | 折叠 / 展开全部 thinking |
| Ctrl+T | todos 面板：agent 任务清单快照（y 复制 · q/Esc 关闭） |
| Ctrl+Q ×2 | 退出（双击确认；composer 内 Ctrl+D 同义） |
| Ctrl+N ×2 | 新会话（双击确认，会丢弃当前上下文） |
| Ctrl+P / ? | 命令面板（slash 命令 + 常用操作，可过滤） |
| Ctrl+X / Ctrl+. | 快捷键速查 |
| Ctrl+G | tasks 面板：后台任务 + 活跃子代理（r 刷新） |
| z（问题卡内） | 自由文本回答（Enter 提交 · Esc 返回选项） |
| y / Y | 复制选中块内容 / 元数据（剪贴板三路由：native→tmux→OSC52，备份 ~/.dsh/last-copy.txt） |
| Shift+↑ / Shift+↓ | **滚动会话视口（任何焦点）** —— 笔记本没有 PageUp 键，用这个 |
| PageUp / PageDown | 翻页滚动（任何焦点） |
| 选中/复制文字 | **默认就能用** —— 鼠标上报默认关闭，选区归终端 |
| `/mouse` | 开启滚轮滚动与点击切焦点。开启后终端把鼠标交给程序，选区要按住 **Shift**（并非所有终端支持）。只开按键上报（`?1000h`+`?1006h`），不开 motion 上报 |
| 滚动（不用鼠标） | PageUp / PageDown、Ctrl+U / Ctrl+D 半页、Ctrl+J / Ctrl+K 单行、j / k、g / G |

## Slash 命令（已实现）

| 命令 | 行为 |
|---|---|
| /resume | 会话选择器：直读 ~/.dsh/sessions 的 JSONL（zstd 多帧），Enter 恢复并回放完整历史，后续消息走 agents.resume 的活会话 |
| /new (/clear) | 新会话 |
| /exit (/quit) | 退出 |
| /help | 命令列表 |
| /copy | 复制最近回复 |
| /model | 模型选择器 |
| /compact | 压缩当前会话历史 |

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

## 实现状态

P1 闭环已落地：插件模式通过 fd3/fd4 JSON-RPC 驱动原生 Rust TUI；`dsh --profile tui` 可直接启动真实会话，`--dump-frame` 可做无 TTY 的确定性布局检查。


