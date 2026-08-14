# 人工验证清单（本人待验证项）

> 维护规则：每完成一项验证，勾掉并注明日期/结果；新增无法自动验证的项随时追加。
> 生成日期：2026-08-14。当前代码基线：cabec65（v0.1.16）。

## 1. 审批弹窗（真实触发）—— 最优先

**为什么自动验证不了**：本机 macOS 的 DSH 沙箱不强制（landlock 是 Linux-only），bash/fs
工具从不进入「沙箱拒绝 → 升级审批」路径，approval/asked 事件从不产生。
UI 与桥接已就绪：demo 模式按 F2 可看弹窗；桥接的 approval/request 监听器 +
ui/approve 应答链路与 ask_user（已活测通过）共用同一套管线。

**验证步骤（建议 Linux 机器）**：
1. 启动 dsh --profile tui，Shift+Tab 切到 workspace-write（或 read-only）
2. 发「Run the command: echo hi > /tmp/x.txt」或触发沙箱外写
3. 期望：permission 弹窗出现（工具名/原因/参数/选项）
4. Enter = 允许一次 → 命令执行；Esc = 取消；检查会话日志有 approval/asked + approval/decided
5. 若 Linux 也不触发：检查权限规则（settings.yaml permission rules / cordis patch）

## 2. Plan 审批 a/s/c/y/q（真实全流程）

**为什么**：需要模型真的进入 plan 模式（enter_plan_mode 需用户批准）+ 生成 plan + exit_plan_mode，
多轮交互且依赖模型配合，没做端到端自动验证。

**验证步骤**：
1. /plan 或 Shift+Tab 进 plan 模式 → 发一个需要计划的模糊任务
2. 模型调 exit_plan_mode → 期望 TUI 出现 plan 详情滚动窗 + 动作条
3. a 批准 → 模型开工；s 输入意见 → 意见以 custom 回到模型（模型应修订计划）；
   q 放弃；方向键滚动详情
4. 检查会话日志的 ask_user 工具结果内容

## 3. Esc 中断真实 turn —— [x] 已验证 2026-08-14

结果：数数任务流式中按 Esc，turn/end = aborted/user（session/cancel → agent.cancel 全链路）。
**为什么曾列为待验证**：需要一个运行中的长 turn + 时序；已实现 session/cancel 但当时尚未实测。

**验证步骤**：
1. 发一个长任务（如「从 1 数到 100，每行一个数字，慢慢输出」）
2. 输出进行中按 Esc → 期望：立即停止、状态回 idle、草稿保留（有草稿时）
3. 检查日志 turn/end reason = cancelled（不是 completed/disposed）

## 4. 活会话保护（跨进程场景）

**为什么**：tui/live-sessions 只能看到同一宿主进程内的活 agent；
Web 与 TUI 是不同进程，互不可见。

**验证步骤**：
1. Web UI 打开会话 A；TUI 里 /resume → 确认 A 仍出现在列表（已知局限，见下）
2. 同一次 TUI 内打开两个窗口驱动同一会话 → 期望第二个被列表过滤/恢复被拒
3. 已知风险：Web 与 TUI 同时驱动同一会话会交错日志（harness 在 resume 时可自愈，
   chen-001/dsh-grok-tui 有 repairInterleavedLog 先例）——决定：接受风险 + 文档警告，
   还是接 Web API 网关查活会话（TODO）

## 5. /resume 恢复后的模型上下文正确性

**为什么**：自动测试只验证了「回放显示 + 追问 turn 跑通」；恢复后模型是否真的
继承了历史上下文（不是只有显示层回放）需要语义验证。

**验证步骤**：
1. 会话里问「记住这个数字：42」
2. /exit 后重新 dsh --profile tui，/resume 该会话
3. 问「我刚才让你记住的数字是多少？」→ 期望回答 42

## 6. 未实现功能（实现后按此验证）

| 功能 | 验证点 |
|---|---|
| /compact | 长会话后执行 → 上下文 token 明显下降、历史要点保留、模型仍能接上下文 |
| Esc rewind（2×Esc） | 回滚选择器出现 → 选早期 turn → 之后内容消失、会话 fork 正确、可重发 |
| @file 补全 | 输入 @ + 部分文件名 → 候选列表、.gitignore 尊重、回车插入路径并附内容 |
| c（plan 行评论）/ y（复制） | plan 审批里按 c 选行评论回模型；y 复制全文到剪贴板 |
| /session-info、/context | 显示会话详情/上下文分类明细 |
| 命令面板 Ctrl+P | 快捷键+slash+skills 可搜索 |
| 剪贴板 | 在 tmux/SSH 下复制走三路由（native→tmux→OSC52）+ 备份文件 |
| 主题 | /theme 实时预览、深浅切换、窄终端自适应 |
| 图片粘贴 | macOS Cmd+V 截图 → chip；Windows Alt+V |
| 多窗口 | 两个 TUI 窗口各自会话、互不干扰 |
| Windows | standalone 模式（fd 3/4 仅 unix；plugin 模式 Windows 走 standalone 或需 named pipe） |

## 7. 已知瑕疵（小）

- 状态栏文案在 70% 分界处偶尔裁半格（draw_status 的 split 无截断处理）
- cargo 有 dead-code 警告若干
- Esc 语义的「取消后 1 秒宽限」未实现（spec 2.5）
- 权限弹窗 Esc 目前=取消请求，spec 是「停驻 scrollback 不回答」（后续对齐）
- ask_user 的自由文本回答（z 键）未实现（Esc 跳过）

## 8. 性能/稳定性

- 大会话（>1MB JSONL）replay 速度与内存
- 长时间运行的内存占用（transcript 无限增长，缺虚拟滚动）
- zstd 日志损坏/交错时的表现（read_session_events 目前静默跳过坏行）
