# DSH 能力地图与 TUI 落点（deepseek-harness 独有能力）

> 分析基线：本机全局安装的 `@deepseek-ai/dsh@0.1.0-rc.6`
> （`/opt/homebrew/lib/node_modules/@deepseek-ai/dsh`，含 194 个 `@deepseek-ai/*` 依赖包）
> 分析日期：2026-08-17。材料来源：各包 `package.json` 描述 + `lib/**/*.d.ts` 类型声明。
> 方法说明：**没有**读文档站。类型声明是本版本的事实来源；若文档站与此处冲突，以本地安装为准。
> 用途：确定 TUI 下一阶段该做什么。docs/01 回答「grok 怎么做 TUI」，本文回答「DSH 有什么值得我们暴露」。
> 术语：本文所说 **seam**（能力缝）= DSH 用 `ctx.*` 暴露的抽象服务，可由不同实现包替换。

---

## 1. DSH 不是「另一个 grok」

grok-build 是一个自带 pager 的单体 agent 平台。DSH 是 **cordis 插件宿主**：所有能力都是可替换的 seam，profile 通过 patch 层组合插件。

结论对 TUI 的两条直接影响：

1. **不能硬编码平台逻辑。** 某个 seam 没挂实现，对应入口就该消失。上一轮把命令面板改成 Harness 能力驱动，方向是对的，要继续沿用到所有面板。
2. **读模型是 session projection，不是原始事件。** 见第 3 节 —— 这是本次调研最重要的发现。

### 1.1 已命名的 29 个 ctx.* seam

从包描述中提取（可能仍有未在描述里点名的 seam）：

| seam | 实现/消费包 |
|---|---|
| `ctx.agents` | dsh-subagent-in-process-driver, dsh-subagent-spawn-in-process |
| `ctx.approval` | dsh-user-approval |
| `ctx.codeRuntime` | dsh-code-runtime(-worker-thread) |
| `ctx.compaction` | dsh-compaction, dsh-compaction-basic, dsh-compaction-tool-result-pruner |
| `ctx.credentials` | dsh-credentials(-local) |
| `ctx.fs` | dsh-fs, dsh-fs-local, dsh-fs-sandbox, dsh-fs-observation-policy |
| `ctx.jobs` | dsh-jobs(-local), dsh-tool-jobs |
| `ctx.permissionPresets` | dsh-permission-presets |
| `ctx.sandbox` | dsh-sandbox(-local), dsh-bash-sandbox, dsh-pwsh-sandbox, dsh-sandbox-windows-acl |
| `ctx.sessionPersistence` | dsh-session-persistence(-jsonl) |
| `ctx.sessionProjections` | **dsh-session-projection**（见第 3 节） |
| `ctx.sessionProjectionCache` | dsh-session-projection-cache |
| `ctx.sessionQuery` | dsh-session-query-sqlite（**FTS5 全文检索**） |
| `ctx.sessionReferenceResolver` | dsh-session-reference（跨会话快照引用） |
| `ctx.settings` | dsh-settings(-file) |
| `ctx.shell` | dsh-shell, dsh-bash-local, dsh-pwsh-local |
| `ctx.spillStore` | dsh-spill(-local), dsh-spill-policy |
| `ctx.storage` | dsh-storage(-domain/-json) |
| `ctx.subagents` | dsh-subagent + fork/spawn in-process 后端 |
| `ctx.subprocess` | dsh-subprocess(-local) |
| `ctx.tokenMeter` | dsh-token-meter |
| `ctx.tools` | dsh-tools, dsh-mcp-client |
| `ctx.userQuestions` | dsh-user-questions, dsh-tool-ask-user |
| `ctx.web` | dsh-web, dsh-web-search-deepseek |
| `ctx.workflowEngine` | dsh-workflow(-worker-thread) |
| `ctx.workspaceRegistry` | dsh-workspace |
| `ctx.apiProxy` / `ctx.directoryPicker` / `ctx.layout` | web host 专用，TUI 不涉及 |

另有未走 `ctx.*` 命名的：`ctx.terminal`（dsh-terminal，持久 PTY）、`ctx.commands`、`ctx.agentDefaultModel`（后两个我们已在用）。

---

## 2. 我们是进程内插件 —— 这是权限优势

官方 web UI 走 remote（`dsh-api-gateway` / `typert.remote-client`），只能访问被显式远程暴露的服务。我们的 Rust TUI 由 cordis 插件 spawn，桥接层持有真实 `ctx`，**可直读任何 seam**。

抽查结果（是否带 `lib/typert.remote-client.js`，即是否为远程客户端暴露）：

| 服务 | 远程可调 |
|---|---|
| dsh-goal | ✅ 唯一一个 |
| dsh-schedule / dsh-tool-todo / dsh-token-meter / dsh-plan-mode | ❌ |
| dsh-session-query / dsh-tool-cordis / dsh-terminal / dsh-workflow | ❌ |

两点结论：
- `dsh-goal` 被 DSH 自己当成**一等客户端界面**（专门做了远程暴露），而我们零支持 —— 见 6.1。
- 其余服务 web 拿不到、我们拿得到。这是我们相对官方 UI 的结构性优势，不该浪费。

---

## 3. Session Projection：TUI 该读的东西

### 3.1 机制

插件把会话状态发布成**投影单元**（`ProjectionDefinition`）：纯同步 `init` / `apply` / `view`，state 必须是纯 JSON。框架在每个已提交会话事件上驱动 `apply`，并支持持久化 checkpoint 与重放恢复。

投影键表 `SessionProjectionMap` 是 **merge-extensible** 的 —— 任何插件都能新增键。**所以泛化读投影的客户端会自动跟随平台演进；逐个解析事件的客户端不会。**

### 3.2 已有 13 个投影键

| projection key | owner | TUI 现状 |
|---|---|---|
| `goal` | dsh-goal | ❌ 完全没有 |
| `contextPressure` | dsh-token-meter | ❌ |
| `contextBreakdown` | dsh-token-meter | ❌ |
| `subagentTiming` | dsh-subagent | ❌ |
| `todos` | dsh-tool-todo | ⚠️ 走错缝，见第 5 节 |
| `plan` | dsh-plan-mode | 部分（计划审查有，投影未读） |
| `title` | dsh-session-title | 部分 |
| `subagent` | dsh-subagent | 部分（子代理卡） |
| `sessionStats` | dsh-session-stats | ✅ 状态栏 |
| `tokenUsage` | dsh-token-meter | ✅ 状态栏 |
| `permissions` | dsh-permission-presets | ✅ |
| `imageLimits` | dsh-host-apiproxy | web 专用 |
| `sessionListMetadata` | dsh-host-apiproxy | web 专用 |

### 3.3 集成路径（已核实到类型层）

`SessionProjectionRegistry` 挂在 cordis `Context` 上，任何插件可见：

```ts
ctx.sessionProjections.snapshot(session): ProjectionSnapshot   // 当前全量值 + seq
ctx.sessionProjections.onChanged(
  (session, key, value, seq) => void
): () => void                                                   // 取消订阅函数
```

`onChanged` 是**键无关**的：一个监听器收全部投影、全部会话的变更，按 session id 过滤即可。

`Agent` 接口带 `readonly session: Session` 与 `readonly id: SessionId`，而桥接层已经在调 `ctx.agents.get(...)`，所以初始快照拿得到：

```js
const agent = ctx.agents.get(sessionId)
const snap  = ctx.sessionProjections.snapshot(agent.session)
const off   = ctx.sessionProjections.onChanged((session, key, value, seq) => {
  if (session.id !== sessionId) return
  notify('tui/projection', { key, value, seq })
})
```

协议侧只需要**一个**新通知 `tui/projection { key, value, seq }`，Rust 侧按 key 分发。新增投影不需要改协议。

已核实到实现（`dsh-session-projection/lib/types/index.js`）：

```ts
interface ProjectionSnapshot {
  asOfSeq: number                        // = session.seq - 1，空日志为 -1
  values: Partial<SessionProjectionMap>
}
```

三条实现层语义，都影响客户端正确性：

1. **`snapshot()` 填充每一个已注册 key**，不是只填「变过的」。某个域还没产出内容时给的是 `view(init())` —— 对 `goal` / `todos` 就是 `null`。所以新会话的快照会合法地推一批 null，客户端必须把 null 读成「已清空」。key 缺席只代表**没有插件注册过它**。
2. **`onChanged` 只在投影值真的变了时才触发**，不是每个已提交事件都触发：
   ```js
   const changed = !Object.is(next, cell.state)
   if (changed && this.listeners.size > 0) { … }
   ```
   所以客户端不需要自己去抖动/去重。载荷还先过了 `schema.parse(view(next))`，是 wire-safe 的。
3. **`drive()` 只处理已提交事件；checkpoint restore 不触发监听器。** 因此**恢复会话时，不主动拉快照就什么都不会显示**，要等下一次事件恰好改动某个投影。桥接层必须在 attach/resume 时调一次 `snapshot()` —— 我们挂在 `registerSession()` 这个所有路径的收口处。

另外 `drive()` 有「late build mid-stream」分支：cell 不存在时先 `buildCell(def, session.events.slice(0, event.seq))` 折完历史再走正常判定，所以中途才注册的消费者不会丢历史。

---

## 4. 模型侧工具清单（影响工具卡分类器）

19 个 `dsh-tool-*` 包。`src/toolcard.rs` 目前按名字词干分类，对下表基本命中，但有几个需要专门渲染：

| 工具包 | 暴露的工具 | 工具卡现状 |
|---|---|---|
| fs | read / write / edit | ✅ Read / Edit |
| fs-search | glob / grep（打包 ripgrep） | ✅ Search |
| bash | bash（可选后台 job + **沙箱升级**） | ✅ Run；升级请求未渲染 |
| bash-persistent | 持久 Bash（PTY 支撑） | ⚠️ 与一次性 bash 无区别 |
| pwsh | pwsh | ⚠️ 落到 Other |
| todo | todo_write | ✅（但见第 5 节） |
| web | web_search / web_fetch | ✅ Web |
| jobs | job_output / job_list / job_kill | ⚠️ 落到 Other |
| subagent | 子代理委派 | 部分 |
| subagent-control | send_message / interrupt_agent / list_agents | ❌ |
| subagent-report | 子代理汇报 | ❌ |
| ask-user | ask_user_question | ✅ 问答卡 |
| skill | skill 加载 | ❌ |
| goal | 目标工具（带执行期权限校验） | ❌ |
| workflow | 跑 JS 编排脚本 | ❌ |
| ralph | **fresh-agent Ralph 循环** | ❌ |
| cordis | **自指：检视运行时、挂载/卸载模型写的插件** | ❌ |
| str-replace-editor | view / create / replace / insert | ⚠️ 落到 Edit（可接受） |
| call-timeout-policy | 非工具，是 tools/execute 包装器 | — |

---

## 5. 需要修正的既有实现：todos 走错了缝

当前 `src/transcript.rs` 从 `tool/call` 的 `arguments` **启发式解析** todo 快照（容忍 `todos`/`items`/`tasks`/裸数组，字段容忍 `content`/`text`/`title`…）。

DSH 的权威定义是投影：

```ts
todos: TodoItem[] | null                    // whole-value，last-wins
interface TodoItem {
  content: string
  status: 'pending' | 'in_progress' | 'completed'
}
```

字段我们猜对了（`content` 优先、三个 status 全部命中），但**接口选错了**：

- 投影是 replay-safe 且带持久化 checkpoint 的；解析事件流不是
- 参数形状是工具实现细节，投影是契约
- 我们多实现的 `cancelled` 状态在 DSH 下是死分支（保留无害，跨 harness 时有用）

**处置**：改读 `todos` 投影，把现有解析器降级为 fallback（投影缺失时才用）。这样在 DSH 下正确，在别的 harness 下仍能工作。

---

## 6. DSH 独有能力：TUI 落点与优先级

### Tier 1 —— 建议立刻做

#### 6.1 Goal 与 GoalBar

DSH 的 goal 不是 grok 的 `/goal`，是**事件溯源的目标状态机 + race-fenced 轮次驱动器**（dsh-goal + dsh-goal-round-driver + dsh-tool-goal + dsh-command-goal）。

```ts
GoalView {
  objective: string
  phase: 'active' | 'paused' | 'blocked' | 'complete'
  blockedReason?: { code: string; message: string }   // phase==='blocked' 时必在
  maxGoalRounds: number
  roundsStarted: number
  createdAt / updatedAt: number
  activation: 'armed' | 'disarmed'    // 进程本地，从不持久化
}
// 操作：create / edit / pause / resume / complete / block / clear
```

关键点：**消息带目标轮次归属**

```ts
GoalMessageSource { kind: 'goal', goalId, revision, round }
```

所以 transcript 能标「目标 X 第 3/10 轮」—— 这是 DSH 特有的、grok 没有对应物的渲染。

官方 web 的落点：`dsh-client-ui-goal` = **GoalBar 常驻输入框上方**，读 goal 投影。

写路径：**服务可用，但不要走 `/goal` 命令**（已核实实现）。

服务是 **`ctx.goals`**（复数）—— 注意四个近似名字别混：服务 `goals`、投影 key `goal`、持久会话事件 `goal/change`、cordis 事件 `goal/changed`。全部**同步**（没有 Promise，别 await），mutation 走 CAS 传当前 `GoalRef`：

```ts
get(agent): GoalView | undefined
create(agent, { objective, maxGoalRounds? }): GoalView      // 不收 ref
edit(agent, ref, { objective?, maxGoalRounds? }): GoalView
pause(agent, ref) / resume(agent, ref) / complete(agent, ref): GoalView
block(agent, ref, { code, message }): GoalView
clear(agent, ref): GoalRef                                   // 返回墓碑 ref
disarm(agent): GoalView | undefined                          // ← 不要用，见下
```

CAS 失配抛 `GoalError`，按 **`error.code`** 路由，不要解析 message：

```
GOAL_AGENT_NOT_LIVE · GOAL_NOT_FOUND · GOAL_ALREADY_EXISTS · GOAL_STALE_REVISION
GOAL_INVALID_OBJECTIVE · GOAL_INVALID_MAX_ROUNDS · GOAL_INVALID_BLOCK_REASON
GOAL_INVALID_EDIT · GOAL_INVALID_TRANSITION
```

每次成功 mutation 把 revision +1，所以**每次调用后必须刷新 ref**。桥接层要在 `inject` 里加 `'goals'`。

### ⚠️ 为什么不能用 `/goal` 命令做按钮

`dsh-command-goal` 的 `parseGoalCommand` 最后一行是**兜底 create**：

```js
if (control === "clear")  return { kind: "clear" }
if (control === "pause")  return { kind: "pause" }
if (control === "resume") return { kind: "resume" }
if (/^edit(?=\s)/iu.test(input)) return { kind: "edit", objective: input.slice(4).trim() }
return { kind: "create", objective: input }      // ← 任何无法识别的文本
```

四个后果：

1. **静默建目标陷阱**：命令层没有 `complete` / `block`，所以 **`/goal complete` 会新建一个 objective 为字符串 `"complete"` 的目标**。`/goal block`、`/goal done` 同理。TUI 按钮一旦走命令通道就是数据污染。
2. **错误码全被抹平**：handler 把所有 `GoalError` 映射成同一句 "The goal command is not valid for the current state."。CAS 失配和非法状态迁移分不出来 —— 对一个需要决定「重读重试」还是「告诉用户原因」的 GoalBar 是致命的。
3. `/goal edit <objective>` 打在一个 **complete** 的目标上会**静默走 create 而不是 edit**。
4. 只返回文本，没有结构化状态 —— 要拿状态得去 screen-scrape `Status:` / `Rounds:` 那几行。

所以：**读走投影 + 事件，写走 `ctx.goals`，`/goal` 只留给用户手输。**

### ⚠️ `activation` 与 `disarm()`

`resume` / `create` 置 `armed`；`pause` / `complete` / `block` / `clear` 以及 `agent/session-start` 置 `disarmed`。**没有独立的 arm API** —— arm 只是 create/resume 的副作用。

**不要从 TUI 调 `disarm()`**：它不写 revision、不发 `goal/changed`，所以没有任何观察者（包括我们自己的 bar）会知道状态变了。想停继续跑用 `pause()`（持久且发事件），想重启用 `resume()`。`disarm()` 归 round driver 所有。

### ⚠️ 轮次计数器不能读投影

**`goal` 投影的 `roundsStarted` 不会随轮次推进** —— `applyGoalProjection` 第一行就是 `if (event.type !== "goal/change") return state`。推进发生在 service 自己的 fold 里，条件是一条被采纳的 goal-sourced `user/message`。所以投影里的值**冻结在最后一次 mutation 的快照上**，只在下次 mutation 提交时跳变。

这正是官方 web GoalBar **不显示轮次计数器**的原因。投影同样刻意不含 `activation`。

我们的做法：从我们本来就在转发的 `user/message` 事件里取 —— `source.kind === 'goal'` 时带 `{goalId, revision, round}`，按 goalId 记录最高已采纳轮次，与投影值取 max（两者都是真值的下界）。想要权威的 live view 也可以直接 `ctx.goals.get(agent)`（含 live `roundsStarted` 和 `activation`）。

另外 `blockedReason.code` 可以区分谁 block 的：`model-reported` = 模型（经 `dsh-tool-goal`），`round-limit` / `queue-failed` / `prompt-rejected` = round driver。

### 订阅

```js
ctx.on('goal/changed', ({ agent, change }) => …)
// change: { operation, ref, goal?: GoalView }   goal 缺席 = clear 墓碑
```
从未 scoped 的 root context 会收到**所有** agent 的，要按 `payload.agent` 过滤。`change.goal` 已经是新鲜的 `GoalView`，不用回读。

GoalBar 不需要 capability flag：没挂 goal 插件 → 没有 goal 投影 → 条自动不出现。

TUI 落点建议：
- 输入框上方一条 GoalBar：objective + phase 徽标 + `roundsStarted/maxGoalRounds` + activation
- `blocked` 时高亮 `blockedReason.message`
- transcript 里给带 goal round 归属的消息加轮次标记
- 命令：`/goal`（创建/编辑）、pause/resume/complete/clear
- 注意 `activation` 是进程本地状态，不能当持久字段展示

#### 6.2 投影驱动渲染（架构性）

按 3.3 的路径落地。一次改动同时修好 todos、拿到 goal / context / subagentTiming，并且**未来任何插件新增投影自动可用**。这是本文最高杠杆的一项。

#### 6.3 真正的上下文压力条 —— `contextPressure`

```ts
ContextPressureProjection {
  pressureTokens?: number    // provider 报的最近一次 prompt 真实大小（含 cache 读写，不含输出）
  projectedTokens?: number   // 下一次请求会花多少 = 上者 + 自采样以来增减量的启发式重定价
  contextWindow?: number     // 最新路由容量
}
```

`projectedTokens` 的设计值得照抄语义：锚定 provider 真实值、只对增量做估算，因此**压缩一发生就立刻反映**——`pressureTokens` 做不到，因为压缩自身不产生 usage 事件。

我们现在状态栏显示的是原始 input tokens。换成 `projectedTokens / contextWindow` 会比 grok 的 context bar 更准。

#### 6.3.1 自动压缩阈值（已核实）

`dsh-compaction-basic`：`DEFAULT_THRESHOLD_RATIO = 0.8`、`DEFAULT_RETAIN_RATIO = 0.16`。触发判定是

```js
if (measurement.totalTokens < spec.thresholdTokens) return null
// thresholdTokens = floor(contextWindow * policy.thresholdRatio)
```

即**占用达到上下文窗口 80% 时自动压缩**，压缩后保留约 16%。比 grok 的 85% 更早。

三个使用注意：

1. `thresholdRatio` 可配置且支持按 provider/model 覆盖（`modelPolicies`）。**可以宿主侧读到**：`ctx.get('compaction').config.thresholdRatio`（`BasicCompactionEngine.config` 是公开字段；抽象 `CompactionEngine` 类型上没有，要 cast）。没有 wire 暴露 —— 但我们是进程内插件，所以拿得到。目前 TUI 硬编码 `ui.rs::COMPACT_THRESHOLD = 0.80`，改成读配置是个小改进。
2. 策略比较的是它自己的 `measure().totalTokens`（**请求+响应**压力），不是我们展示的 `projectedTokens`（只有 prompt 侧）。所以 0.8 色带是准确的**指引**而非硬线，文案该说「大约在这里压缩」。
3. `retainRatio = 0.16` —— 压缩成功后表面回落到窗口的约 16% 加摘要，也就是仪表会掉到哪里。

`resolveCompactSpec` / `resolveTargetPolicy` **不可 import**（包只导出 `BasicCompactionEngine`），所以 `floor(contextWindow * ratio)` 和 `modelPolicies` 的精确匹配都得自己重算。

#### 6.3.2 压缩期间客户端看到什么（已核实）

机制：投影状态必须保持 O(1)，没法记住每个节点的价格，所以压缩前一个 metering 事件先声明被替换区间的「影子价格」（`shadowedTokenCount`），替换事件再消费这个 claim。

按顺序会观察到：

1. **`compaction/summary` 时数字不动，但仍然收到一次变更通知** —— fold 返回了新的 state 对象（只为存 claim），而 registry 是按 state **引用**去重的，不是按 view 相等。所以会有一帧数字完全相同的空更新。**不要当成错误，也不要在这上面做动画。**
2. **替换事件时数字下跌**：`contextBreakdown.messageTokens` 减 `shadowedTokenCount - summaryTokens`，`contextPressure.projectedTokens` 减同一个量。`pressureTokens` **不动**（压缩自己不产生 usage）—— 这就是 `projectedTokens` 存在的全部理由；只渲染 `pressureTokens` 的仪表在整个压缩期间看起来是卡住的。`projectedTokens` 在 0 处截断，所以一次大压缩配上过时的锚点可能让仪表触底到 0%，**这是预期而非 bug**。
3. **下一次请求的 usage 到达时**，`pressureTokens` 跳到压缩后的真实 prompt 大小，此刻 `projectedTokens == pressureTokens`。

投影层没有任何东西需要失效（每个值都是整体 last-wins，替换重画即可）。**需要失效的是 transcript**：替换事件的 `shadowedSeqs: number[]` 精确列出了被影子化的表面节点 seq —— 我们目前没有处理，属于待办。

#### 6.4 `/context` 明细 —— `contextBreakdown`

```ts
ContextBreakdownProjection {
  systemTokens: number    // 最新请求信封的系统提示词
  toolsTokens: number     // 工具 schema
  messageTokens: number   // 当前模型可见会话面
}
```

即 grok `/context` 的分类视图，白送。

**硬约束（类型注释里写明的）**：三者用固定密度估算，**系统性低估 CJK 文本与 JSON schema**，所以「present these as approximations of composition, never as a total」。实现时必须按成分展示、**不能求和当总量**，否则会和 `projectedTokens` 打架。

### Tier 2 —— 独特且中等成本

| # | 能力 | DSH 侧 | TUI 落点 |
|---|---|---|---|
| 6.5 | **沙箱状态与提权** | ctx.sandbox + dsh-sandbox-policy（per-call 解析器）+ bwrap / landlock / Windows ACL 三后端；`dsh-tool-bash` 带 sandbox-escalation | 状态栏显示当前 profile；审批卡显示「请求提权」及其范围。DSH 的沙箱故事比 grok 深得多 |
| 6.6 | **持久终端面板** | ctx.terminal（owner-scoped **交互式** PTY seam）+ dsh-terminal-bash + dsh-tool-bash-persistent | 挂一个 pane 到活 PTY。我们本身就是终端，这是天然契合；grok 无对应物 |
| 6.7 | **spill 解析** | ctx.spillStore + dsh-spill-policy 把超大工具输出替换成引用 | 全屏查看器按引用取全文，而不是显示截断提示 |
| 6.8 | **`cordis_define` 卡片** | dsh-tool-cordis：模型可往运行中的宿主写并挂载插件 | 带 run/stop 的插件定义卡（web 已有 `dsh-client-ui-cordis`）。grok 完全没有这个概念 |
| 6.9 | **产出文件尾巴** | — | 每轮末列出本轮产出/引用的文件（web 有 `dsh-client-ui-deliverables`：produced-files turn tail + 可点击文件引用） |
| 6.10 | **嵌套工具调用树** | — | web 的 `dsh-client-ui-tool` 是 **call-tree** renderer；我们的 transcript 是平的。结构性差距 |
| 6.11 | **会话全文检索** | ctx.sessionQuery（SQLite **FTS5**） | `/resume` 现在只能按标题过滤；全文搜索几乎免费（docs/01 §14 已标记过） |

### Tier 3 —— 以后再说

- **Ralph 循环**（dsh-tool-ralph，fresh-agent 循环）：需要展示迭代轮次的卡片
- **schedule 面板**（dsh-schedule：`after` / `at` / **fixed-rate**，比 grok 只有间隔的 `/loop` 更丰富）
- **agent presets**（dsh-agent-presets，按 preset `cordis.yml` 做**每会话 agent 组合**；`~/.dsh/.agent-presets/` 已存在）
- **Code Mode 指示器**（dsh-agent-tool-presentation 能把一个 agent 的工具组合成 **Code Mode / native / bot** 三种呈现 —— DSH 概念，Code Mode 下调用形态完全不同，值得单独渲染）
- 消息级反馈（dsh-message-feedback）· 重复调用提醒（dsh-repeat-tool-reminder）· 跨会话 `@` 引用（dsh-session-reference）· 文件观察策略/读后写校验（dsh-fs-observation-policy）· persona · pwsh 专属渲染 · workflow-run 嵌套披露

---

## 7. 官方 web UI surface 对照（32 个 `dsh-client-ui-*`）

这是「一个 DSH 客户端应该暴露什么」的最佳代理。标 ❌ 的是我们完全没有的：

| surface | 我们 |
|---|---|
| conversation / composer / commands / input-trigger（`/` `@`） | ✅ |
| model-selection / permission-presets / plan / user-questions / theme | ✅ |
| jobs（会话头后台任务列表） | ✅ Ctrl+G |
| skill（skill 引用 + 专用工具行） | ❌ |
| **goal**（GoalBar） | ❌ |
| **trajectory**（事件账本 + 交互式时序总览） | ❌ 我们只有状态栏聚合值 |
| **deliverables**（产出文件尾巴） | ❌ |
| **cordis**（动态插件定义卡） | ❌ |
| **tool**（调用**树** + 按工具的呈现槽位） | ⚠️ 平铺 |
| **agent-preset** | ❌ |
| **workflow-run**（持久工作流节点 + 嵌套披露） | ❌ |
| subagent（会话目录 + 续跑路由 + `@` 源） | 部分 |
| message-feedback | ❌ |
| attachment（草稿图片轨 + 图片画廊） | ❌ |
| sidebar（会话多级树 / 搜索 / 分组 / 状态点） | ❌ ≈ grok Dashboard |
| settings-*（5 个）/ workspace / directory-picker-* / layout / slots / primitives | web 专用或不适用 |

---

## 8. 不追的东西

- **grok 的 Dashboard / worktree**：DSH 侧有 `ctx.workspaceRegistry` 和 web 的会话树，但对 TUI 仍是低频入口。继续排最后。
- **web host 专用**：`imageLimits`、`sessionListMetadata`、`ctx.apiProxy`、`ctx.directoryPicker`、`ctx.layout`。
- **grok 的内核能力**（hooks / memory / marketplace）：那是 grok 平台的事，DSH 的对应物是插件与 skill，按 DSH 的形状做，不照搬 grok 的 UI。

---

## 9. 建议实施顺序

1. **spike**：核实 `ProjectionSnapshot` 字段名，桥接层打通 `snapshot` + `onChanged`，加 `tui/projection` 通知
2. **todos 切投影**（第 5 节），现有解析器降级为 fallback
3. **GoalBar**（6.1）—— 最大的 DSH 独有缺口
4. **contextPressure 压力条 + contextBreakdown 的 `/context`**（6.3 / 6.4）
5. 之后按 Tier 2 取：沙箱状态 → spill 解析 → 产出文件尾巴 → 调用树

理由：1–2 是地基且能立刻修掉一处错接口；3 是 DSH 自己都做了远程暴露的一等界面；4 用同一条投影管道，边际成本很低。

---

## 10. 我们 profile 里到底有什么（已核实）

投影和服务存在于包里 ≠ 存在于我们的运行时。`~/.dsh/profiles/tui` 的 bundles 是 `['@deepseek-ai/dsh-base', 'dsh-whale-tui']`，profile 自己的 `cordis.patch.yml` 是空的，所以**能力完全由 dsh-base 决定**。

核验 `dsh-base/cordis.patch.yml` 的实际 row：

| plugin row | 在 base | 影响 |
|---|---|---|
| `session-projection` | ✅ | 投影管道可用 |
| `goal` + `goal-round-driver` + `command-goal` | ✅ **三件套齐全** | GoalBar 可做，且 `/goal` 命令应该已经能通过现有 Harness 命令路径工作 |
| `token-meter` | ✅ | `tokenUsage` / `contextPressure` / `contextBreakdown` 可用 |
| `tool-todo` | ✅ 配置 `allowParallelInProgress: true` | `todos` 投影可用；**多条可同时 in_progress** |
| `compaction-basic` | ✅ | 压缩压力可读 |
| `sandbox` | ✅ | Tier 2 沙箱状态可做 |
| **`session-stats`** | ❌ **不在 base** | **`sessionStats` 投影在我们 profile 里不存在**。状态栏的 turns/steps/LLM 耗时/工具耗时/TTFB/TPS 必须继续自己算 |
| **`terminal`** | ❌ 不在 base | Tier 2 的持久 PTY 面板需要先往 profile 加插件，不能假定可用 |

两条直接结论：

1. **不要按 `sessionStats` 投影重写状态栏** —— 那个插件没挂。现有自算逻辑保留。
2. **`allowParallelInProgress: true` 意味着 todos 可以有多条 `in_progress`**。`App::open_todos` 现在用 `.position(|i| status == InProgress)` 取第一条，在并行场景下这个"落在进行中那行"的语义是任意的。渲染本身没问题（全部行都画），但选中语义要改成明确的策略。

另外：桥接层已经在用 `ctx.commands.list(agent)` 列 Harness 命令、`tui/execute-command` 执行，而 `command-goal` 在 base 里 —— 所以 **goal 的写路径可能已经免费可用**，GoalBar 只需要补读路径（投影）。`capabilities` 里还没有 `goal` 标志，要加。

---

## 11. 待核实清单

- [x] ~~`ProjectionSnapshot` 的确切字段名~~ —— `{ asOfSeq, values }`，见 3.3
- [x] ~~`onChanged` 的触发时机~~ —— **仅值变化时**（`!Object.is`），见 3.3
- [x] ~~投影是否需要先注册才有值~~ —— `snapshot()` 按需 lazy fold；未注册的 key 才缺席。但 **restore 不触发 onChanged**，resume 必须主动拉快照
- [x] ~~`goal` 的可变操作怎么调~~ —— **`ctx.goals`**（复数，同步，CAS 传 `GoalRef`，按 `error.code` 路由）。**不要走 `/goal` 命令**：它把无法识别的文本兜底成 create，且抹平所有错误码 —— 见 6.1
- [x] ~~`GoalActivation` 由谁 arm/disarm~~ —— create/resume 置 armed，pause/complete/block/clear + session-start 置 disarmed；无独立 arm API。TUI **可以**经 pause/resume 驱动，**不可**调 `disarm()`（不发事件），见 6.1
- [ ] 沙箱当前 profile 从哪个 seam 读（`dsh-sandbox-policy` 提到「each session's current model context」）
- [ ] spill 引用在 tool result 里的载荷形状，以及取全文的调用
- [ ] DSH 工具调用是否真有父子关系可构树，还是 web 的树来自子代理层级
- [x] ~~`ctx.terminal` 是否可用~~ —— **不在 dsh-base**，要用得先往 profile 加 `dsh-terminal` + `dsh-terminal-bash`；接口能力仍待查
- [ ] `dsh-tool-cordis` 的事件形状（挂载/卸载/运行状态如何上报）
- [ ] 压缩替换事件的 `shadowedSeqs` 目前没被处理 —— transcript 需要按它丢弃被影子化的节点（见 6.3.2）
- [ ] `tokenUsage` 投影应当取代我们对 `assistant/chunk` 的 usage 抓取：它按 (turn, step) **替换**而非累加同一次请求的采样，所以重试不会双计；我们现在的抓取会。另注意它把 reasoning 折进 `outputTokens`，不单独暴露
- [ ] 是否把 `dsh-session-stats` 加进 profile 的 `cordis.patch.yml`（它只在 web-app bundle 里挂）。加了能白拿六个聚合值，但会改用户的 profile，且 live/per-turn 计时无论如何仍要客户端算
