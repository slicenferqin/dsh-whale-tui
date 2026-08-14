# openma deepseek-harness-tui 架构拆解

> 分析对象：openma-ai/deepseek-harness-tui @ main（clone 于 2026-08-14，v0.1.0，基线 dsh 0.1.0-rc.6）
> 定位：目前唯一 Rust/ratatui 实现的 DSH TUI，双模式（dsh profile 插件 + SDK standalone），预编译 4 平台。
> 用途：我们自研 Rust TUI 的技术参照。只做架构拆解，不抄代码。

---

## 1. 一句话架构

**一个 Rust/ratatui 前端进程 + 一套与官方 SDK 兼容的 NDJSON JSON-RPC 通道**。TUI 永远只跟一个「SDK 协议 peer」说话：standalone 模式下 peer 是自己 spawn 的 runtime 子进程；plugin 模式下 peer 是宿主 dsh 进程（通过继承的 fd 3/4 管道）。TS 侧插件在宿主进程内复刻官方 dsh-sdk-jsonrpc-server 并加了 5 个 TUI 专用方法。

```text
+------------------------- standalone -------------------------+
| dsh-tui (Rust) -- stdio NDJSON JSON-RPC --> dsh-jsonrpc-agent 子进程 |
|        (spawn + initialize + prompt)                         |
+--------------------------------------------------------------+

+------------------------- plugin 模式 -------------------------+
| dsh --profile tui（宿主，Node）                                |
|   |- dsh-base bundle + 本插件 tui-runner 行（cordis 插件）       |
|   |    inject: ['agents']                                     |
|   |    spawn(dsh-tui --attach-fds, stdio inherit + fd3/4 pipe)|
|   |    JsonRpcLineTransport(读 fd4, 写 fd3)                    |
|   |    <-> 复刻官方 SDK server + tui/* 扩展方法                 |
|   |    <-> ctx.agents.create / agent.followup / modelSelection |
|   |    <-> session/event、agent/status、subagent.* 转发         |
|   +- dsh-tui (Rust, --attach-fds)：TTY 归 TUI，fd3/4 走协议     |
+--------------------------------------------------------------+
```

## 2. 代码规模与模块（8891 行）

| 文件 | 行数 | 职责 |
|---|---|---|
| app.rs | 2143 | 应用状态机（RunState: Idle/Starting/Running）、输入框（Input: 历史/stash）、transcript 消费、auto_prompt |
| transcript.rs | 994 | 会话流的渲染模型（Cell/CellKind、NoticeLevel、UsageTotals） |
| ui.rs | 964 | ratatui 渲染：布局 rects（chat/tips/composer）、宠物、鼠标选区 hit-test、dump_frame |
| events.rs | 701 | RPC 通知 → UI 事件解析（session.status/session.event 全类型分发） |
| controller.rs | 665 | UI→运行时命令线程：prompt/interrupt/选模型/取目录/shutdown |
| main.rs | 422 | CLI 解析、终端守卫（raw/alt-screen/mouse/bracketed-paste/panic hook）、主循环、--demo/--dump-frame |
| demo.rs | 412 | 脚本化 demo turn（无需 runtime/API key） |
| proto.rs | 311 | NDJSON JSON-RPC 2.0 传输（spawn/attach、request 超时、kill/shutdown、stderr tail） |
| sessions.rs | 310 | /resume：直读 JSONL 会话日志（zstd 多帧）、workspace slug、摘要 |
| runtime.rs | 280 | standalone runtime 发现（dsh-jsonrpc-agent：flag/venv wheel/PATH）+ cordis 解析 + 凭据注入 |
| theme.rs | 266 | 深/浅两套 DeepSeek Web UI 配色（Theme 槽位） |
| logo.rs / logo_data.rs / pet.rs | 508 | 鲸鱼 logo、kitty graphics 宠物像素（half-block 回退） |
| clipboard.rs | 132 | 剪贴板三路由：native（pbcopy/wl-copy/xclip）→ tmux load-buffer → OSC 52（tmux passthrough 信封） |
| bus.rs | 88 | 事件总线：AppEvent（Term/Rpc/RuntimeStderr/RuntimeExited/Ctl/ShellDone）+ Cmd |
| npm/lib/index.js | 458 | **TS 桥**：宿主内 cordis 插件，复刻官方 SDK server + 5 个 tui/* 方法 |
| npm/cordis.patch.yml | 8 | bundle 补丁：insert 一行 tui-runner |
| 构建 | — | build-npm.sh + package-native.mjs + CI 矩阵（4 平台交叉编译） |

## 3. 进程与线程模型

- **主线程**：ratatui 事件循环。bus_rx.recv_timeout(50ms) 拉事件 → app.handle() → 需要时整帧重绘；每 100ms app.tick()（动画/超时）。needs_redraw 标记批渲染。
- **input 线程**：crossterm::event::read() → AppEvent::Term。
- **frame reader 线程**（proto）：读运行时 stdout 帧 → 响应路由给 pending 等待者、通知转 AppEvent::Rpc；EOF 时 fail 所有 in-flight 并报 RuntimeExited(code)。
- **stderr pump 线程**：保留 200 行诊断尾巴，逐行转 AppEvent::RuntimeStderr。
- **controller 线程**：收 Cmd，执行 RPC 调用（request 阻塞式、带超时），结果/错误以 CtlEvent 回 bus。中断不走 controller——UI 线程直接 interrupt_now()。
- 所有状态单线程持有（App），线程间只走 mpsc——教科书式 ratatui 架构，没有 async。

## 4. 协议层（proto.rs）

- **传输**：NDJSON，一行一帧 JSON-RPC 2.0。请求带 id（dsb-N 自增）；响应按 id 路由；通知按 method 路由；**server→client 请求一律回 -32601**（防 runtime 死等）。
- **客户端请求**：initialize（cwd/provider/model/maxTokens）、session/prompt（contentBlocks → 收 messageId 入队收据）、shutdown。
- **服务端通知**：session.event（全量会话事件信封）、session.status（running/idle）、subagent.started / subagent.finished（仅进程内子代理）。
- **错误面**：RpcFailure(code,message)；request 超时（多数 30s、initialize 180s）；stderr 尾巴附在超时错误里。
- **中断**：kill() = 丢 stdin + SIGKILL 子进程，in-flight 立刻失败，**磁盘 JSONL 存活**（下次 spawn 可续）。shutdown() = 礼貌 shutdown（1.2s）→ kill。
- 请求必须**离开 UI 线程**（controller 线程执行 request）。

## 5. TS 桥（npm/lib/index.js）——本项目的核心借鉴点

- **挂载**：cordis 插件，inject: ['agents']；bundle 补丁只在用户 profile 里 insert 一个 tui-runner 行。
- **进程**：spawn(bin, ['--attach-fds'], { stdio: ['inherit','inherit','inherit','pipe','pipe'] }) —— TTY（0/1/2）继承给 TUI，fd3 宿主→TUI 写方向、fd4 TUI→宿主写方向；用官方 dsh-sdk-protocol 的 JsonRpcLineTransport 组帧。
- **方法**：initialize（provider/model/maxTokens；deepseek-official 缺适配器时自动挂 dsh-llm-deepseek）、session/prompt（getOrCreateSession → ctx.agents.create + createUserMessage + agent.followup）、shutdown。
- **5 个 tui/* 扩展**：
  - tui/catalog：providers + models（vision 位）+ agent presets + 当前选择
  - tui/model-info：reasoning/context/defaultMaxTokens
  - tui/select-model：llm.resolveCallConfig 校验 + installModelSelection（会话级）或改 defaults（未来会话）
  - tui/permission：permissionPresets.set(session, preset)；会话未建时暂存并在首 prompt 后应用（带 permission/preset 事件回显）
  - tui/preset：首 prompt 前暂存 agent preset，创建时经 agents.create({setup}) + presets.mount 组合（api-proxy 的预发布组合模式）
- **事件转发**（复刻官方 server 的订阅）：session/event → session.event；agent/status → session.status；session/created → subagent.started（parentSession 判定）；subagent/end → subagent.finished（info.local 过滤 + carrierKeyOf 取父）。
- **生命周期**：TUI 退出（shutdown 应答后）→ flush → ctx.root.fiber.dispose() → exit；宿主退出 → kill TUI。**stdout 必须干净**（TUI 用宿主 stdout 画屏）。
- 会话创建去重（sessionCreations Map）、agent 被外部 dispose 的防御性校验（对照 ctx.agents.get）。

## 6. 双模式对比

| | plugin（推荐） | standalone |
|---|---|---|
| 启动 | dsh plugin --profile tui add + dsh --profile tui | dsh-tui 直接跑 |
| runtime | 宿主 dsh（dsh-base 全生态） | 自己发现的 dsh-jsonrpc-agent（--runtime-bin/DSH_RUNTIME_BIN/venv wheel/PATH） |
| 会话存储 | ~/.dsh/sessions（与 Web UI 共享） | ~/.dsh-tui/sessions（--session-root 可改） |
| 模型/provider | 宿主真实目录 + tui/select-model 热切换 | 启动参数/运行时配置 |
| **中断** | **无硬中断**（宿主持有 turn；README 明说） | Esc = SIGKILL runtime；JSONL 存活，重开续跑 |
| 凭据 | 宿主凭据系统 | 环境/--api-key/~/.dsh/.credentials.yaml + settings.yaml 默认模型 |
| 审批/ask_user | 协议没有此通知（见差距清单） | 同左 |

## 7. 中断语义细节（差距核心）

- standalone：interrupt_now() → 直接 rt.kill()（SIGKILL）→ 立即 Interrupted 状态；会话日志落盘可续。**代价**：kill 的是整个 agent runtime 进程——turn 没了，后续 spawn 重来（冷启动）。
- plugin：controller 明确「the peer is the host dsh process — never spawn or kill」→ Esc 只能标记/排队，真正的硬取消**做不了**（SDK 协议无 cancel 方法）。
- 这正是我们的机会：**进程内插件（Rust TUI 直接消费 cordis 事件）可以调用 agent.cancel 实现真正的 Esc 取消**，或在协议上扩展一个 session/cancel（TS 桥里一行 agent.cancel，Rust 端发新方法）。

## 8. 会话 /resume（客户端直读方案）

- 不走 RPC：直接扫 <root>/<workspace-slug>/<id>/session.jsonl[.zstd]。slug = 绝对路径 / 变 - 包裹 -...--（/Users/x/proj → --Users-x-proj--）。
- zstd 日志是**多个拼接帧**（每 flush 一帧）——必须循环 StreamingDecoder 直到 EOF（他们专门修过这个坑）。
- 摘要：首个 user/message（source.kind=user）文本前 40 字符、turn/start 计数、mtime 排序、去重、限 50 条。plugin 模式同时扫 ~/.dsh/sessions。
- 启示：**resume 完全可以前端实现**（replay JSONL 重建 transcript），协议零改动；Web UI 会话自动互通。

## 9. UI 层要点

- 布局：chat（上）+ tips（状态行）+ composer（输入区）三段式 Rect 切分；pet 区域右下角；narrow 终端自适应。
- transcript 渲染模型：Cell + CellKind（消息/思考/工具卡/usage 等）、NoticeLevel（错误/警告分级）。
- 鼠标：滚轮滚动、**拖拽选择并在松开时复制**、双击选词、Shift+拖拽走终端原生选区；ui.rs 保留选区 hit-test 布局快照。
- 剪贴板：native（macOS pbcopy / Linux wl-copy→xclip→xsel）→ tmux load-buffer → OSC 52（100KB 上限截断；tmux 内包 ESC Ptmux 信封、payload ESC 翻倍）——与 grok 同构。
- 主题：深/浅两套 DeepSeek Web UI 配色（不是 grok 5 主题），Theme 结构集中（bg/surface/panel/fg 三级 + brand/code/ok/warn/err/bubble）。
- 键位（README 自述「grok-build homage」）：enter 发送/排队、alt+enter send-now、esc 中断/双击清空、ctrl+c 清空/退出、上箭头历史、! 本地 shell、/ 命令、ctrl+m 模型、ctrl+e 展开、ctrl+t 主题。
- demo 模式：内置脚本化事件流 + --dump-frame WxH 渲染单帧文本——**零依赖验证 UI 的手段，值得抄**。
- pet：kitty graphics 协议画像素鲸鱼，不支持时 half-block 降级。

## 10. 发布管线

- npm 包带 vendor/<platform>-<arch>/dsh-tui[.exe] 4 个预编译二进制（darwin-arm64/x64、linux-x64、win32-x64）；package-native.mjs stage/verify。
- CI：test 作业（cargo test + node 脚本测试 + tag 版本一致性校验）→ native 矩阵（4 平台 rustup target + release 构建）→ package（合并产物 + verify + npm pack）→ tag 时 publish + GitHub release。
- 本机构建：build-npm.sh = cargo build --release + stage 当前平台 + npm pack。
- 版本策略：单一版本号横跨 Cargo.toml 与 npm/package.json，tag 必须匹配（check-release-tag.mjs）。

## 11. 与 grok TUI 的功能差距清单

| 能力 | grok | openma | 我们的目标 |
|---|---|---|---|
| 硬取消（Esc） | 有（cancel-turn 面板） | standalone=kill runtime；plugin=无 | **进程内 agent.cancel（超越两者）** |
| 审批弹窗 | 完整（Allow once/Reject+消息/Always-approve/记忆行/手编模式） | 无审批 UI（协议没通知） | 必须做（进程内 waterfall） |
| ask_user 问题卡 | 完整（多题/多选/自由文本/跳过） | 无 | 必须做 |
| plan 审批 a/s/c/y/q | 有 | 无 | 做 |
| 权限模式循环 Shift+Tab | 有 | 有雏形（tui/permission preset） | 有 |
| thinking 折叠 | 有（Ctrl+E 全局） | 有（ctrl+e） | 有 |
| 工具卡 | 分类渲染（diff/bash/search/web） | 有（transcript CellKind） | 强化（grok 式样式参数） |
| 子代理视图 | framed 子视图+tasks 面板 | 有 subagent 生命周期渲染（未见树导航） | 树导航+framed |
| 后台任务面板 | Ctrl+G | 未实现 | 进程内接 ctx.jobs |
| /resume 全文搜索 | 有 | 标题+预览（无全文） | 接 sessionQuery sqlite |
| @file 补全 | 有（fuzzy+gitingore+隐藏文件） | 未见 | 做 |
| rewind/fork | 有（rewind 选择器） | 无 | 做（sessions.fork） |
| 主题系统 | 5 主题+auto+槽位体系+量化 | 深/浅 2 套固定 | 学 grok 槽位 |
| 终端探测/doctor | 20+ 品牌 | 未见 | 精简版 |
| Dashboard 多会话 | 有 | 无 | P2 |
| 鼠标 | 点击/滚轮/hover | 滚轮/拖选/双击 | 对齐 grok |
| 剪贴板 | 三路由+备份文件 | 三路由（无备份文件） | 加备份 |
| 宠物/品牌 | 无强制 | 有（kitty pet） | 可选 |

## 12. 可复用 vs 要重做（技术点清单）

**可直接借鉴（不是抄代码）**
1. TS 桥的「复刻官方 SDK server + tui/* 扩展」模式：我们可照此加 session/cancel、审批/ask_user 通知（把 DSH 的 waterfall 与 user-questions 事件推到 Rust 端）。
2. fd 3/4 + stdio inherit 的插件内原生二进制挂载法（TTY 归 TUI、协议走管道）——如果我们也选「Rust 前端 + TS 桥」架构。
3. 请求/通知/超时/stderr-tail 的 proto 层形状；server→client 请求回 -32601 兜底。
4. zstd 多帧解码 + workspace slug + 客户端 resume 直读。
5. ratatui 单线程事件循环 + mpsc bus + input/frame/stderr 三线程分工。
6. demo 模式 + --dump-frame 的 UI 自验证手段。
7. 剪贴板三路由与 tmux OSC52 信封细节。
8. 发布管线：npm bundle + vendor 平台二进制 + 4 平台 CI 矩阵 + tag 校验。

**要重做/超越**
1. **中断语义**：加 session/cancel（RPC）+ 宿主侧 agent.cancel——这是最大卖点。
2. **审批与 ask_user**：协议上补 server→client 请求或专用通知（官方 SDK 说 server→client 请求是 dead capability，但 TS 桥是我们自己写的，可以双向化）；Rust 端做 grok 式弹窗（含 always-allow 记忆行的 UI）。
3. Esc 语义状态机（取消/2×清空/rewind/宽限期）——openma 完全没有。
4. follow-up 队列与 send-now（openma 只有 alt+enter send-now，无队列面板）。
5. 工具卡分类渲染样式参数化（pager.toml 式）。
6. 主题槽位体系 + /theme 实时预览 + 量化。
7. 快捷键栏（contextual shortcuts bar）。
8. 若走「进程内纯 Rust 插件」而非「TS 桥」：需要 Rust↔Node 边界设计（见结论）。

## 13. 对我们的启示（结论）

1. **架构选型**：openma 证明「Rust TUI + TS 桥」可行且工程代价小（复用 dsh-sdk-protocol 组帧、复用 ctx 服务）。但它把 TUI 钉死在「SDK 协议子集」上，审批/取消/ask_user 全部缺席。**我们的方案：保留 TS 桥模式，把协议扩展为双向**（加 session/cancel 请求、approval/request 与 user-questions 通知），即可拿到进程内全部能力 + Rust 原生性能——这是 grok 体验（P0 清单）得以实现的唯一前提。
2. **MVP 边界**：照 01-grok-tui-spec.md 的 P0：双区布局、流式+thinking+工具卡、Esc 语义、审批弹窗、plan a/s/c/y/q、follow-up 队列、/resume /new /model /compact、状态栏。openma 的 demo/dump-frame 手段保障 UI 先行开发。
3. **差异化**：硬取消 + 审批 + ask_user + Esc 状态机 + grok 式主题槽位，就是我们相对 openma（和所有 TS 竞品）的护城河。
4. **风险**：rc.6 会破（openma 锁 rc.6 基线）；fd 3/4 在 Windows 不可用（openma 明确 bail，Windows 走 standalone）；协议扩展要自己维护 TS 桥。
