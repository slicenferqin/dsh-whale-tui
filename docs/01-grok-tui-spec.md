# Grok TUI 复刻对照 Spec（grok-build pager 交互细节）

> 分析基线：xai-org/grok-build @ `eb267feff13129e568df38fb6fdf0ceb65f735d6`（sparse checkout 仅 `crates/codegen/xai-grok-pager` + `xai-grok-shell`）
> 分析日期：2026-08-14。材料来源：pager 的 docs/user-guide/ 全部 24 篇 + 源码目录结构。
> 用途：我们自研 Rust TUI（DSH 插件）的设计蓝本。只借鉴交互与视觉语言，不复制代码。
> 说明：grok 的权限/工具规则引擎、sandbox、hooks、记忆等是「内核能力」而非「TUI 能力」，本 spec 只记录其中影响 TUI 交互的部分。

---

## 1. 总体布局与视觉语言

全屏 TUI（alt-screen），两条渲染路径：**fullscreen**（默认）与 **minimal**（实验性，scrollback-native，终端 16 色无主题）。

- **两大主区**：上部 scrollback（会话历史）+ 底部 prompt（输入区）。Tab/空格在两者间切焦。
- **底部快捷键栏（shortcuts bar）**：随状态变化的上下文快捷键提示（焦点在哪个面板、agent 是否运行、选中条目类型）。
- **状态行**：输入框上方可临时出现「still-running」状态行（后台任务计数）；右上角有 context bar（token 用量）与用量 chip。
- **条目 = block**：user prompt（可作 sticky header）、agent 消息（完整 Markdown + 语法高亮）、thinking 块（可折叠）、工具调用卡（edit diff / execute / search 等）、task 列表、subagent 生命周期块。左侧 accent 竖线 + 可折叠指示符。
- **欢迎屏**：会话列表 + 新建（Ctrl+W worktree）+ 导入 Claude 设置入口。
- **Dashboard**（Ctrl+\）：同进程全部顶层会话的 live roster（peek/reply/dispatch/pin/rename/stop/attach）。
- **侧面板**：tasks 面板（Ctrl+G：subagent+后台任务+monitor/loop）、todos 面板（Ctrl+T）、prompt 队列（Ctrl+;）。
- **光标颜色**随主题（OSC 12）、退出还原（OSC 112）。
- **紧凑模式**（/compact-mode）：去垂直 padding、水平 padding 压到 1 列。

## 2. 键盘绑定总表

> 全部为内置绑定、不可改键（config 只开关 vim_mode / simple_mode）。两种输入模式：
> **Simple**（默认）：方向键导航，字母键自动聚焦输入框；**Vim**（opt-in）：j/k 导航、H/L 按 turn 跳、h/l 折叠。

### 2.1 导航（scrollback 聚焦）

| 键 (vim) | Alt (simple) | 动作 |
|---|---|---|
| j / k | 下 / 上 | 选下/上一条目 |
| Shift+L / Shift+H | Shift+右 / Shift+左 | 下/上一 turn（用户 prompt） |
| Shift+J / Shift+K | — | 下/上一 assistant 回复 |
| g / Shift+G | — | 回顶部 / 回底部 |
| Ctrl+K / Ctrl+J | 同 | 上/下滚一行（不动选择） |
| Ctrl+U / Ctrl+D | 同 | 上/下半页 |
| PageUp / PageDown | 同 | 翻页（选择随视口） |

### 2.2 视图与折叠（scrollback 聚焦）

| 键 | 动作 |
|---|---|
| h / l（左/右） | 折叠/展开选中条目 |
| e | toggle 折叠 |
| Shift+E | 全部展开/全部折叠 |
| Ctrl+E | 展开/折叠所有 thinking 块 |
| r | 选中条目切 raw markdown |
| y / Shift+Y | 复制块内容 / 复制块元数据（如命令文本） |
| Enter / Ctrl+F | 全屏查看器打开块内容 |

手动折叠可「钉住」（respect_manual_folds，默认关）：钉住的块不被流式更新重置，展开钉住的块会停止跟随滚动。

### 2.3 聚焦切换

- Tab：scrollback 与 prompt 双向；在阻断卡内 Tab/Shift+Tab 在卡的选项间循环。
- **Esc 不是聚焦键**（语义见 2.5）。

### 2.4 阻断卡（三张，共享同一契约）

优先级：**权限弹窗 > 取消-turn 面板 > 问题卡**（同时打开时按此序接管键盘）。

- **问题卡（ask_user_question）**：上/下 或 j/k 选答案；左/右 或 [/] 换问题；1–9/a–f 直接选答案；z 跳自由文本框；Space 多选 toggle；Enter 确认并前进/提交；Esc 逐级后退（清 pending 再停驻 scrollback，Tab 回来）；y 复制答案；Shift+X 跳过该问题；Ctrl+F 全屏。
- **权限弹窗**：上/下 选选项；1–9 直选；Enter 确认；左/右 收窄/放宽 always 记忆范围；e 手编 always-allow 模式（bash）；Ctrl+F 展开完整参数；Ctrl+O 打开 always-approve；Esc 停驻 scrollback（不回答不驳回）；Ctrl+C 取消请求；在 No 行打字 = 给 agent 回一条消息。
- **取消-turn 面板**：1–4/Enter 选「取消并保留子代理」等选项；Esc = 继续运行（面板关闭）。

### 2.5 Escape 语义（重点复刻对象）

| 状态 | 手势 | 效果 |
|---|---|---|
| turn 运行中（默认模式） | Esc | **立即取消**（有草稿也取消，草稿保留） |
| turn 运行中（全屏 vim 模式） | Esc | 吞掉（不取消），用 Ctrl+C |
| 正在取消 | Esc | 重发 cancel（重试） |
| 空闲 + 非空输入框 | 2×Esc（800ms 内） | 清空输入（进历史） |
| 空闲 + 空输入 + 有消息 | 2×Esc | 打开 **rewind 选择器**（= /rewind） |
| 空闲 + 无消息/搜索打开/有 pending 浮层 | Esc | 吞掉 |

- Steal-Esc 优先级：浮层/弹窗/下拉/搜索/选区/语音/命令模式退出 > 中途取消 > 清空/rewind。
- **取消后 1 秒宽限**：抑制 rewind 触发（狂按 Esc 不会误开 rewind）。
- Ctrl+C vs Esc：非空草稿时 Ctrl+C 先清草稿（turn 继续），再按才取消；Esc 直接取消保草稿。空闲时 Ctrl+C 一键清空、Esc 要两下。

### 2.6 Agent 级 / 全局 / turn 中

| 键 | 动作 |
|---|---|
| Ctrl+P / ? | 命令面板（快捷键+slash 命令+skills 可搜索） |
| Ctrl+M | agent 屏=模型选择器；prompt 聚焦=多行模式 toggle |
| Ctrl+C | 取消当前 turn（或先清草稿） |
| Ctrl+O | always-approve 模式 toggle |
| Ctrl+S | 会话选择器（/resume） |
| Shift+Tab | 模式循环 Normal → Plan → Always-approve |
| Ctrl+B | 前台命令转后台 |
| Ctrl+T / Ctrl+G | todos 面板 / tasks 面板 |
| Ctrl+L | 扩展弹窗（VS Code 家族则是 mid-turn 插话 send-now） |
| 上（空输入） | 打开历史面板（最近 prompt 预填） |
| ! | shell 模式（空输入时） |
| Ctrl+Enter / Ctrl+I（Apple Terminal: Ctrl+O；VS Code 家族: Ctrl+L） | **send-now**：取消当前 turn 并立刻发送（turn 中） |
| Ctrl+N | 新会话（1s 内双击确认） |
| Ctrl+\ | Dashboard toggle |
| Ctrl+Q / Ctrl+D | 退出（双击确认） |
| Ctrl+. / Ctrl+X | 快捷键速查（KKP 可用时用 Ctrl+.） |

**turn 中的 follow-up**：普通 Enter = 排队（默认 queue，等 turn 结束；steer 则在下个安全间隙注入）；空输入框再按 Enter = 立即发送队首；agent 阻塞等待时 Enter 直接打断接管。

### 2.7 鼠标

点击选条目；滚轮滚动；点击 prompt 聚焦；hover 高亮（可配）；X11 中键粘贴 PRIMARY；拖拽选择。

### 2.8 Dashboard 键位（Ctrl+\）

上/下 导航（选中即 peek）；Enter 打开/发回复；Ctrl+S 回复并 attach；Ctrl+/ 搜索过滤；Ctrl+R 改名；Ctrl+T pin；Ctrl+G 分组切换（状态与目录）；Ctrl+X 取消 turn（2s 内两次=删除会话）；Esc 逐级后退；Ctrl+\ 退出。分组 section 有折叠标记；Idle 组默认只显示最近 8 个 +「N more」行。

## 3. 权限/审批系统（TUI 呈现部分）

### 3.1 模式矩阵

| 模式 | 免问范围 | 适用 |
|---|---|---|
| default（ask） | 只读工具 + 内置只读命令白名单 | 日常交互 |
| acceptEdits | 文件编辑免问 | 本地写码+事后看 diff |
| plan | 兼容占位（真计划用 plan mode） | Claude 兼容 |
| auto | 安全检查放行的免问，其余拦/升级 | 少打扰交互 |
| dontAsk | 仅预批准的工具/命令 | 严格 CI |
| bypassPermissions（always-approve） | 全部（deny 规则/hook 仍生效） | 可信自动化 |

切模式入口：Shift+Tab 循环、Ctrl+O、/always-approve、/auto、/settings、CLI --always-approve、ACP yoloMode。

### 3.2 授权流水线（顺序）

PreToolUse hook → deny>ask>allow 规则（多来源合并，按严重级非顺序）→ 记忆授权（per-project）→ 内置自动放行（只读工具+只读命令白名单）→ prompt 策略（当前模式）。危险命令列表（rm/chmod/git push 等）即使有记忆前缀授权也要问。

### 3.3 弹窗选项（复刻重点）

- **Allow once / Reject once（可附消息给模型）/ Enable always-approve / Allow all edits this session**（文件编辑专属，仅内存）
- remember_tool_approvals=true 时追加：**Always allow: <command>**（按前缀记忆）/ 对应 never-allow 行 / MCP 工具与 web 域等价行。危险命令的记忆默认收窄为完整命令。

### 3.4 Plan 模式审批视图（a/s/c/y/q 是这个交互的灵魂）

agent 调 exit_plan_mode 后弹出可滚动 plan 预览 + 底部动作条：
**a** 批准并开工（有 pending 评论则带评论批准）· **s** 要求修改（焦点回 prompt 写意见）· **c** 对选中行/范围加评论 · **y** 复制全文 · **q** 放弃计划并退出 plan 模式。Tab 在预览/输入间切换；三种焦点态：Preview / Commenting / Prompt。
plan mode 状态机：Inactive→Pending→Active→ExitPending→Inactive；持久化到磁盘。plan 文件编辑自动放行，其它编辑直接拒绝（任何权限模式下都生效）。

## 4. 工具调用卡渲染规则

（pager.toml 块样式 + 文档观察；grok 的 ToolOutput 联合类型：Bash/ReadFile/SearchReplace/WebSearch/Text 等）

- **Edit diff 块**：缩进 diff、hunk 分隔符（默认 …）、可选双列行号（GitHub 式）、折叠头带 +N/-M 统计、背景可配。
- **Execute（shell）块**：折叠时只显示开头 2 行+结尾 3 行输出；运行中 accent 线动画；头部风格 shell（前缀）或 label（Run 前缀）；折叠时命令文本弱化。
- **Thinking 块**：折叠显示「Thinking...」头 + 截断 3 行；运行时 accent 线动画；Ctrl+E 全局折叠/展开。
- **通用**：bullet 样式可配（none/·/•/●/▸/▶/◆，默认 ◆）；折叠时弱化；dim_details 弱化括号细节（行数/匹配数）；失败调用自动展开；卡片头 = 工具名/标题（Bash 显示命令、文件工具显示路径、Web 显示查询词）。
- **Subagent 块**：父 scrollback 里一个生命周期块（Subagent running: "desc" — Thinking + 实时活动后缀如 Running: cargo test），Enter/Ctrl+F 打开 **framed 子视图**（带标题栏的完整子 transcript，q/Esc 返回）；后台 subagent 结束后追加 completed/failed/cancelled 块。
- **Task 列表**：todo_write 渲染为勾选列表；badge 格式可配（2/5 / 列式 / 逗号式）。

## 5. 状态栏与上下文

- **右上角 context bar**：当前上下文/窗口（由服务端 totalTokens 与模型 state 广告驱动）。
- **用量 chip**：缓存命中率/进出 token/API 次数/工具耗时。
- **/context**：上下文分类明细（系统提示词/消息/推理与开销/空闲 + 工具定义、skills、MCP 通告的 token 估算）。
- **/session-info**（别名 /status /info）：认证方式、模型、turn 数、上下文用量；c 复制会话 ID，y 复制整块。
- **still-running 状态行**（输入框上方）：后台任务计数（command/monitor/loop/subagent），阻塞等待时加「send a message to interrupt」提示。
- 完成事件落到 transcript 为单个「Task completed」chip；「Worked for」标记每 turn 一条。

## 6. 输入体验

- **多行**：Ctrl+M toggle（/multiline）；开时 Enter=换行、Shift+Enter/Alt+Enter=发送。
- **@file 引用**：@src/main.rs、@src/main.rs:10-50、@src/（目录浏览）；模糊文件选择器，默认尊重 .gitignore 且隐藏 dotfile，@! 搜索隐藏文件。发送时自动附内容。
- **moded composer**：! shell 模式（空 prompt 打 !）、# remember 模式。
- **历史**：上箭头 面板（最近 prompt 预填、上下步进、边打边改）/ /history 模糊搜索（Enter/Tab 回填）。
- **粘贴/附件**：Ctrl+V 文本/文件/截图（macOS/Linux）、Windows 截图 Alt+V；非图片文件插入绝对路径；图片变 chip；X11 PRIMARY 中键粘贴、Shift+Insert。
- **follow-up 队列**（Ctrl+; 面板）：queue（默认）/ steer 两种行为；send-now 和弦见 2.6。
- **语音**：voice 模块（可选，P2）。
- 外部编辑器编辑草稿：Ctrl+G（minimal）/ 被占时走命令面板；VISUAL→EDITOR→vi。

## 7. Slash 命令目录

分两类来源：**shell builtin**（agent 后端执行）与 **pager builtin**（前端执行），同一菜单展示，模糊匹配，Tab/Enter 接受。

- **会话**：/new(/clear) · /resume · /dashboard(/sessions) · /compact [note]（auto-compact 阈值默认 85%）· /context · /session-info(/status /info) · /fork · /rewind(/undo) · /edit-prompt(minimal) · /copy [n|file] · /export · /quit(/exit) · /home(/welcome) · /delete · /rename(/title)
- **模型与模式**：/model <id> [effort](/m) · /effort <lvl> · /always-approve · /auto · /multiline(/ml) · /history · /compact-mode · /vim-mode · /minimal | /fullscreen(/full) · /plan [desc] · /view-plan(/show-plan)
- **记忆**：/memory(/mem) · /flush · /dream · /remember
- **扩展**：/hooks · /plugins · /marketplace · /skills（同弹窗不同 tab）
- **媒体**：/imagine · /imagine-video
- **调度**：/loop [interval] <prompt>
- **工作流/目标**：/goal · /deep-research · /workflow · /workflows
- **其它**：/theme(/t) · /feedback · /btw（旁白不打断）· /mcps · /doctor(/terminal-setup) · /release-notes · /docs · /tutorial · /import-claude · /config-agents(/agents) · /personas · /login · /logout · /usage(/cost) · /privacy · /settings(/config) · /timestamps
- **Skills 即命令**：user-invocable: true 的 skill 直接以 /name 出现；重名用 /local:x、/user:x、/plugin:x 限定；内建命令总是赢裸名。

## 8. 会话管理

- 存储：~/.grok/sessions/<encoded-cwd>/<session-id>/，文件：summary.json（元数据索引）、updates.jsonl（权威会话流，驱动 /resume）、chat_history.jsonl、plan.json（todo 状态）、rewind_points.jsonl、signals.json、compaction_checkpoints/、subagents/。
- /resume 选择器：按标题过滤 + **全文内容搜索**（Extended search results）；Ctrl+/ 立即搜索。
- /fork：复制历史开新 agent（可选 worktree、可选指令）。
- /rewind：回滚到任意早前 turn，丢弃其后一切；2×Esc 也可打开。
- /delete：确认；/resume 列表里 d→y；dashboard Ctrl+X 两次。
- /export、/copy 细节：复制默认 OSC 52，若不可达落 ~/.grok/last-copy.txt 备份并 toast 提示。
- 标题：自动 + /rename 手动（--auto 取消钉住）。

## 9. 主题系统

- 5 内置主题 + auto：GrokNight（默认，中性深底+品红 accent）、GrokDay、TokyoNight、RosePineMoon、OscuraMidnight；auto 跟随系统（macOS AppleInterfaceStyle / Linux portal / Windows 注册表 / SSH 用 GROK_APPEARANCE→COLORFGBG→OSC 11），5s 轮询。
- **颜色槽位体系**（复刻要点）：bg_base/bg_light/bg_dark/bg_highlight/bg_hover/bg_terminal/bg_visual；accent_user/assistant/thinking/tool/system/error/success/running/skill/plan/verify/remember/model；text_primary/secondary；gray_dim/gray/gray_bright；语义色 command/path/running/warning/fuzzy_accent；border/scrollbar/paste/diff/markdown 全套。
- 量化：RGB 按终端能力（truecolor/256/16）自动量化；NO_COLOR 单色。非 truecolor 隐藏需要特性的主题。
- /theme 选择器实时预览，Enter 应用、Esc 还原；直接传名跳过。
- 光标颜色随主题（OSC 12/112）。紧凑模式。语法高亮内置 3 个 tmTheme（二进制内置）。
- pager.toml 微调：布局 padding、scrollbar、滚动行为（follow_indicator、follow_by_overscroll）、sticky headers、折叠指示符、动画 fps、各块样式。
- minimal 模式：固定 16 色、无主题、直接画在终端背景上。

## 10. 子代理与后台任务视图

- spawn_subagent 工具：background/resume_from/capability_mode(read-only|read-write|execute|all)/isolation(none|worktree)/cwd；**深度限制 = 1**（子代理不能再 spawn）。
- tasks 面板（Ctrl+G）：Subagents 组在最上（spinner、耗时、kill/查看），后台命令、monitor、loop 带行数 badge；任务 ID 可见。
- 后台命令：Ctrl+B 转后台；get_command_or_subagent_output/wait_commands_or_subagents/kill_command_or_subagent。
- /loop（60s 起、7 天过期、最多 50）、monitor（每行输出→通知；persistent 直到 kill；音量过大自动停）。

## 11. 终端集成

- 终端品牌检测（20+ 种：Kitty/WezTerm/Ghostty/iTerm2/VTE/VS Code 家族/Windows Terminal…）决定：Ctrl+Enter vs Ctrl+O vs Ctrl+L、Ctrl+. 可用性（KKP）、Shift+Enter 行为、alt-screen 策略（tmux control mode/Zellij 用 inline）。
- **/doctor**：颜色级别、剪贴板路由、tmux 设置（clipboard/dcs-passthrough/extended-keys/truecolor 可自动修，只改配置不 source）、鼠标报告、语音权限。
- **剪贴板三路由**：native → tmux buffer → OSC 52（tmux 内自动包 passthrough 信封）；SSH 预测 OSC 52 但标「未验证」，toast 给备份路径；GROK_CLIPBOARD_NO_OSC52=1 关。
- 鼠标滚动失效提示（Apple Terminal View→Allow Mouse Reporting 等）。

## 12. Dashboard（多会话 roster）

- 行 = 顶层会话（父+forks），状态排序：Needs input → Working → Idle(最近 8) → Inactive(默认折叠) → Completed/Failed；状态图标：Working 动画 spinner、实心圆 黄/绿/红/琥珀、空心圆 idle/inactive。
- peek 面板：最后回复类型+时间+最近回复 3 行 + 实时 reply 输入。
- 打开 = details view（无边框弹窗的全宽会话视图，顶部 agent 名 + cycle chip + [Dashboard]）。
- dispatch 输入永远开新会话；Ctrl+/ 切搜索模式；64KiB 上限。

## 13. headless / ACP 边界（参考用）

- headless：grok -p（plain/json/streaming-json），--allow/--deny 规则注入。
- ACP：grok agent stdio|serve，yoloMode 设权限模式；TUI 本身通过 leader socket 与 shell 通信。

---

## 14. DSH 落点对照总表

| grok 特性 | DSH 落点 | 缺口 |
|---|---|---|
| 流式消息/思考 | assistant/chunk、reasoning 流（session/event） | 无 |
| 工具调用卡 | tool/call + tool/result 事件；bash 有 exit code、fs 工具有 diff/hunk | 需自建按工具类型的渲染器（可仿 grok ToolOutput 联合） |
| 权限弹窗 | DSH 审批 waterfall（tools/pre-execute）、permissionPresets、ask_user_question | 无；DSH 的权限模式≈preset，UI 层映射即可 |
| Plan 模式审批视图 | dsh-plan-mode + exit_plan_mode | 需自建 a/s/c/y/q 审批 UI；DSH 的 plan 审批走 ask_user |
| 模式循环 Shift+Tab | DSH permission presets 列表 | 需在 TUI 侧做循环状态机 |
| 取消 turn | agent.cancel（进程内插件模式可用！） | openma 走 SDK 协议反而没有；我们进程内可硬取消 |
| rewind | ctx.sessions.fork(source, boundary) + 回放 | ccch1mneyyy/dsh-TUI 已证明可行 |
| /compact | dsh-compaction 服务 + /compact 命令 | 无 |
| todo 面板 | tool/call 里的 todo_write 全量快照 | 需自建面板 |
| subagent 视图 | subagent.started/finished + session tree | 需自建 framed 子视图/树导航 |
| tasks 面板/后台任务 | ctx.jobs + job_* 工具事件 | 需自建面板 |
| /resume 全文搜索 | session persistence JSONL + sessionQuery sqlite | 全文搜索可用 sqlite 查询服务 |
| @file 补全 | 需自建：fs 搜索 + glob | 自建 |
| 剪贴板三路由 | 无；参考 openma clipboard.rs（native/tmux/OSC52） | 自建 |
| 主题槽位 | 无；自建 theme 模块 | 自建（可借 grok 槽位命名） |
| 状态栏用量 | dsh-token-meter / usage 事件 | 需确认事件形状 |
| 命令面板 | ctx.commands 注册表 | 需把命令注册表暴露给 TUI |
| /doctor | 自建（终端探测逻辑） | 自建 |
| Dashboard 多会话 | ctx.agents live registry | 进程内可直读 |

## 15. 复刻优先级建议

**P0（体验灵魂，MVP 必做）**
1. 全屏 scrollback + 底部 prompt 双区布局、Tab 焦点切换、快捷键栏
2. 流式渲染：消息 Markdown、thinking 折叠（Ctrl+E）、工具卡（Edit diff / Bash / 通用）
3. **Esc 语义全套**：中途取消（保草稿）、2×Esc 清空、2×Esc rewind、取消后宽限
4. 权限弹窗（Allow once/Reject once/Always-approve/Always-allow 记忆行）+ Shift+Tab 模式循环
5. Plan 审批视图 a/s/c/y/q
6. follow-up 队列 + send-now（turn 中插话）
7. /resume（列表+过滤）+ /new + /model + /compact + /exit
8. 状态栏：上下文用量 + 运行状态行（still-running）

**P1（第二迭代）**
- @file 模糊补全与引用、! shell 模式、历史面板（上箭头）、多行 toggle
- todo 面板（Ctrl+T）、tasks 面板（Ctrl+G）、subagent framed 子视图
- 主题槽位 + 亮暗两主题 + /theme 实时预览、鼠标（滚轮/选择/复制）
- 命令面板（Ctrl+P）、/session-info、/context、/export、/copy 备份路径
- 剪贴板三路由 + 终端探测（至少 tmux/VS Code/WezTerm 三类差异）

**P2（锦上添花）**
- Dashboard 多会话 roster、worktree fork、/loop、monitor、语音、图片粘贴、/doctor、minimal 模式、OTEL、记忆/目标/工作流等内核特性（随 DSH 自身能力走）

## 16. 待确认清单

- [ ] 状态栏上下文条的具体渲染元素（文档只给了语义；需运行时截图或 views/ 源码确认）
- [ ] 工具卡的确切颜色/折叠头文案细节（pager.toml 只给样式参数）
- [ ] follow-up steer 模式的注入时机细节（工具间 vs 模型调用前）
- [ ] rewind 选择器 UI 形态（列表？时间线？）——docs 未给图
- [ ] 键盘绑定是否真的完全不可 remap（docs 明说「cannot currently be remapped」）
- [ ] minimal 模式与 fullscreen 的 scrollback 差异细节
- [ ] 权限弹窗「1–9」直选在超过 9 个选项时的行为
- [ ] /copy 到文件的确切参数解析规则