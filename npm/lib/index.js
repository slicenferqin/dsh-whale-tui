/**
 * dsh-whale-tui runner — cordis plugin that mounts the native TUI.
 *
 * ESM on purpose: the harness loader imports plugins concurrently, and a CJS
 * require() of @deepseek-ai/dsh-llm from a non-loader thread hits Node's
 * ERR_REQUIRE_ESM_RACE_CONDITION. Static ESM imports join the same module
 * graph and load cleanly.
 *
 * Install into a profile and launch:
 *
 *     dsh plugin --profile tui add <this package>
 *     dsh --profile tui
 *
 * The runner spawns the platform-native dsh-whale-tui binary with the host TTY
 * (fds 0/1/2 inherited) and serves a JSON-RPC surface over two extra pipes
 * (child fd 3: server -> tui frames, child fd 4: tui -> server frames).
 * It replicates the stock @deepseek-ai/dsh-sdk-jsonrpc-server methods
 * (initialize / session/prompt / shutdown plus session.event, session.status,
 * subagent.started notifications) and adds our protocol extension:
 *
 *   session/cancel — hard-cancel the running turn via agent.cancel.
 *
 * Protocol extensions include bidirectional approval / ask-user requests,
 * model catalog and selection, permission presets, compaction, rewind,
 * session information, resume, and background-job inspection.
 *
 * The agent, tools, persistence, and providers come from the surrounding dsh
 * profile. Stdout of the host process is the TUI screen — keep stdout
 * loggers out of the profile.
 */

import { spawn } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import { JsonRpcLineTransport } from '@deepseek-ai/dsh-sdk-protocol'
import { createUserMessage, freezeMessage } from '@deepseek-ai/dsh-llm'
import { installModelSelection } from '@deepseek-ai/dsh-agent'
import { SessionId } from '@deepseek-ai/dsh-session'

const name = 'dsh-whale-runner'
const inject = ['agents', 'agentDefaultModel', 'commands', 'tools']

function nativeBinary() {
  const key = process.platform + '-' + process.arch
  const exe = process.platform === 'win32' ? 'dsh-whale-tui.exe' : 'dsh-whale-tui'
  const bin = path.join(path.dirname(new URL(import.meta.url).pathname), '..', 'vendor', key, exe)
  if (!fs.existsSync(bin)) {
    let have = []
    try { have = fs.readdirSync(path.join(path.dirname(new URL(import.meta.url).pathname), '..', 'vendor')) } catch {}
    throw new Error(
      'dsh-whale-tui: no native binary for ' + key +
      ' (packaged: ' + (have.join(', ') || 'none') + '); rebuild with scripts/build-npm.sh'
    )
  }
  return bin
}

function apply(ctx) {
  const bin = nativeBinary()
  const child = spawn(bin, ['--attach-fds'], {
    stdio: ['inherit', 'inherit', 'inherit', 'pipe', 'pipe'],
  })

  // child fd 3 reads what we write; child fd 4 writes what we read.
  // EPIPE after the TUI exits is expected teardown noise: without these
  // handlers the write error event crashes the host (unhandled 'error').
  child.stdio[3]?.on('error', () => {})
  child.stdio[4]?.on('error', () => {})
  const transport = new JsonRpcLineTransport(child.stdio[4], child.stdio[3])

  const defaults = {
    cwd: process.cwd(),
    provider: 'deepseek-official',
    model: 'deepseek-v4-flash',
    reasoningEffort: undefined,
    maxTokens: undefined,
  }
  /** sessionId -> handle, for sessions this server created. */
  const sessions = new Map()
  /** sessionId -> in-flight creation promise (dedupes concurrent prompts). */
  const sessionCreations = new Map()
  /** sessionId -> 会话创建前暂存的完整模式。 */
  const pendingModes = new Map()
  /** child sessionId -> delegating sessionId, for subagent completion notifications. */
  const childParents = new Map()
  /** Agent -> mutable selection captured at the next prompt-assembly boundary. */
  const modelSelections = new WeakMap()
  const disposers = []
  let shuttingDown = false
  let shutdownTask

  // --------------------------------------- notification forwarding ------
  // Replicates the stock HarnessSdkJsonRpcServer constructor subscriptions.
  disposers.push(ctx.on('session/event', (session, event) => {
    transport.notify('session.event', { sessionId: String(session.id), event })
  }))
  disposers.push(ctx.on('agent/status', ({ agent, status }) => {
    transport.notify('session.status', { sessionId: String(agent.session.id), status })
  }))
  const queueItems = (agent) => [
    ...agent.inbox.nextTurn.map((message) => ({
      id: String(message.id),
      placement: 'queued',
      message,
    })),
    ...agent.inbox.nextStep.map((message) => ({
      id: String(message.id),
      placement: message.source.kind === 'user' ? 'steering' : 'context',
      message,
    })),
  ]
  const publishQueue = (agent) => {
    transport.notify('session.queue', {
      sessionId: String(agent.session.id),
      items: queueItems(agent),
    })
  }
  disposers.push(ctx.on('agent/inbox/inserted', ({ agent }) => publishQueue(agent)))
  disposers.push(ctx.on('agent/inbox/claimed', ({ agent }) => publishQueue(agent)))
  disposers.push(ctx.on('agent/inbox/discarded', ({ agent }) => publishQueue(agent)))
  const publishCapabilitiesChanged = () => transport.notify('tui.capabilities-changed', {})
  disposers.push(ctx.on('commands/change', publishCapabilitiesChanged))
  disposers.push(ctx.on('tools/change', publishCapabilitiesChanged))

  // --------------------------------------- session projections ----------
  // DSH's canonical read model (docs/04 section 3). One key-agnostic channel
  // carries every projection, so a projection a future plugin adds needs no
  // protocol change here or in the TUI. Reading these beats re-deriving state
  // from raw events: they are replay-safe and carry persisted checkpoints.
  //
  // `sessionProjections` is optional on purpose — a profile that does not mount
  // dsh-session-projection simply gets no projection traffic, and the TUI keeps
  // its own fallbacks.
  const projections = ctx.get('sessionProjections')
  const publishProjection = (sessionId, key, value, seq) => {
    transport.notify('session.projection', { sessionId, key, value, seq })
  }
  if (projections !== undefined) {
    disposers.push(projections.onChanged((session, key, value, seq) => {
      publishProjection(String(session.id), key, value, seq)
    }))
  }
  disposers.push(ctx.on('session/created', (session) => {
    const parentSession = session.header.parentSession
    if (parentSession === undefined) return
    const parentSessionId = String(parentSession)
    const childSessionId = String(session.id)
    childParents.set(childSessionId, parentSessionId)
    transport.notify('subagent.started', { parentSessionId, childSessionId })
  }))
  disposers.push(ctx.on('subagent/end', (info) => {
    if (!info.local) return
    const childSessionId = String(info.id)
    const parentSessionId = childParents.get(childSessionId)
    if (parentSessionId === undefined) return
    childParents.delete(childSessionId)
    transport.notify('subagent.finished', {
      provider: info.provider,
      agentId: childSessionId,
      parentSessionId,
      childSessionId,
      status: info.stopReason === 'completed' ? 'ok' : 'error',
      stopReason: info.stopReason,
      ...(info.lastAssistantMessage === undefined
        ? {}
        : { lastAssistantMessage: info.lastAssistantMessage }),
    })
  }))

  // ------------------------------------------ interactive dialogs --------
  const dialogSignal = (signal) => {
    const timeout = AbortSignal.timeout(120000)
    return signal === undefined ? timeout : AbortSignal.any([signal, timeout])
  }

  // Approval: answer the approval/request waterfall from the TUI. On any
  // transport failure we delegate (fail-closed via the default answerer).
  disposers.push(ctx.on('approval/request', async (req, next) => {
    try {
      const result = await transport.request('ui/approve', {
        toolName: req.toolName,
        callId: req.callId ?? null,
        reason: req.reason ?? null,
        options: ['allowed-once', 'always-allow', 'rejected'],
      }, dialogSignal(req.signal))
      const outcome = result && result.outcome
      if (outcome === 'allowed-once' || outcome === 'rejected' || outcome === 'cancelled') {
        // "Always allow" row: the TUI answers allowed-once and asks the
        // bridge to switch the session to the approval-free preset, so the
        // remembered grant is just the preset switch (grok persists a
        // per-command prefix; dsh has no such seam, preset is the closest).
        if (result && result.remember === 'always') {
          const svc = ctx.get('permissionPresets')
          const session = req.agent && req.agent.session
          if (svc !== undefined && session !== undefined) {
            try { svc.set(session, 'danger-full-access') } catch {}
          }
        }
        return outcome
      }
      return next()
    } catch {
      return next()
    }
  }))

  // ask_user_question: register THE UI provider for this context (the tui
  // profile composes no other provider). Returns the human answer to the
  // agent loop.
  const userQuestions = ctx.get('userQuestions')
  if (userQuestions !== undefined) {
    disposers.push(userQuestions.registerProvider({
      ask: async (req) => {
        const result = await transport.request('ui/ask-user', {
          questions: req.questions.map((q) => ({
            id: q.id,
            question: q.question,
            header: q.header ?? null,
            detail: q.detail ?? null,
            intent: q.intent ?? null,
            options: (q.options ?? []).map((o) => ({
              label: o.label,
              description: o.description ?? null,
            })),
            multiSelect: !!q.multiSelect,
          })),
        }, dialogSignal(req.signal))
        return { answers: result.answers }
      },
    }))
  }

  // ---------------------------------------------------------- sessions --
  function selectionFromLog(agent) {
    const config = agent.session.requestHeader?.()?.config
    if (config === undefined) {
      return {
        provider: defaults.provider,
        model: defaults.model,
        ...(defaults.reasoningEffort === undefined
          ? {}
          : { reasoningEffort: defaults.reasoningEffort }),
      }
    }
    return {
      provider: config.provider,
      model: config.model,
      ...(config.reasoningEffort === undefined
        ? {}
        : { reasoningEffort: config.reasoningEffort }),
    }
  }

  function selectionFor(agent) {
    const installed = modelSelections.get(agent)
    if (installed !== undefined) return installed
    const selection = {
      current: selectionFromLog(agent),
      assembled: undefined,
    }
    installModelSelection(agent.ctx, selection)
    modelSelections.set(agent, selection)
    return selection
  }

  function registerSession(sessionId, handle) {
    selectionFor(handle.agent)
    sessions.set(sessionId, handle)
    return handle
  }

  async function createSession(sessionId) {
    const handle = await ctx.agents.create({
      sessionId: SessionId(sessionId),
      meta: { cwd: defaults.cwd },
      agentOptions: {
        provider: defaults.provider,
        model: defaults.model,
        ...(defaults.maxTokens === undefined ? {} : { maxTokens: defaults.maxTokens }),
      },
    })
    registerSession(sessionId, handle)
    const staged = pendingModes.get(sessionId)
    if (staged !== undefined) {
      pendingModes.delete(sessionId)
      const permissionPresets = ctx.get('permissionPresets')
      const planMode = ctx.get('planMode')
      if (permissionPresets === undefined) {
        throw new Error('no permission-presets service in this profile')
      }
      if (planMode === undefined) throw new Error('no plan-mode service in this profile')
      planMode.set(handle.agent, staged.plan)
      permissionPresets.set(handle.agent.session, staged.preset)
    }
    return handle
  }

  async function getOrCreateSession(sessionId) {
    if (shuttingDown) throw new Error('SDK server is shutting down')
    const existing = sessions.get(sessionId)
    if (existing) return existing
    const pending = sessionCreations.get(sessionId)
    if (pending) return pending
    const creation = createSession(sessionId)
    sessionCreations.set(sessionId, creation)
    creation.then(
      () => { sessionCreations.delete(sessionId) },
      () => { sessionCreations.delete(sessionId) },
    )
    return creation
  }

  // ------------------------------------------------------------ methods --
  async function initialize(params) {
    if (params.maxTokens !== undefined
      && (!Number.isSafeInteger(params.maxTokens) || params.maxTokens <= 0)) {
      throw new TypeError('initialize maxTokens must be a positive safe integer')
    }
    defaults.cwd = path.resolve(String(params.cwd))
    const configured = ctx.agentDefaultModel.currentSelection()
    defaults.provider = configured.provider
    defaults.model = configured.model
    defaults.reasoningEffort = configured.reasoningEffort
    defaults.maxTokens = params.maxTokens
    return {
      serverInfo: { name: 'dsh-whale-tui-shim', version: '0.1.5' },
      current: {
        provider: defaults.provider,
        model: defaults.model,
        ...(defaults.reasoningEffort === undefined
          ? {}
          : { reasoningEffort: defaults.reasoningEffort }),
      },
    }
  }

  async function prompt(params) {
    const sessionId = String(params.sessionId)
    const handle = await getOrCreateSession(sessionId)
    // An agent-loop-only reload disposes the loop's agents while this record
    // survives; validate against the live registry before delivery.
    if (ctx.agents.get(handle.agent.id) !== handle.agent) {
      throw new Error('session agent was disposed outside the server: ' + sessionId)
    }
    const message = createUserMessage({ content: params.contentBlocks, source: { kind: 'user' } })
    handle.agent.followup(message)
    return { messageId: message.id }
  }
  function prioritizeNextTurn(agent, message) {
    agent.send(message, 'next-turn', true)
    const index = agent.inbox.nextTurn.findIndex((candidate) => candidate.id === message.id)
    if (index <= 0) return
    const before = agent.inbox.nextTurn.slice(0, index)
    agent.inbox.splice('next-turn', 0, index + 1, [message, ...before])
  }

  async function sendNow(params) {
    const sessionId = String(params.sessionId)
    const handle = sessions.get(sessionId)
    if (handle === undefined) throw new Error('unknown session: ' + sessionId)
    const agent = handle.agent
    if (agent.status !== 'running') return { accepted: false }
    const message = createUserMessage({ content: params.contentBlocks, source: { kind: 'user' } })
    agent.cancel({ kind: 'user' }, { keepInbox: true })
    prioritizeNextTurn(agent, message)
    return { accepted: true, messageId: message.id }
  }
  async function updateQueue(params) {
    const sessionId = String(params.sessionId)
    const itemId = String(params.itemId)
    const handle = sessions.get(sessionId)
    if (handle === undefined) throw new Error('unknown session: ' + sessionId)
    const agent = handle.agent
    const message = agent.inbox.nextTurn.find((candidate) => String(candidate.id) === itemId)
    if (message === undefined) throw new Error('queued item is no longer pending: ' + itemId)
    const action = params.action
    if (action?.kind === 'remove') {
      agent.inbox.remove(message.id)
      return { accepted: true }
    }
    if (action?.kind === 'edit') {
      const text = action.text
      if (typeof text !== 'string' || text.trim() === '') {
        throw new TypeError('queue edit text must be non-empty')
      }
      agent.inbox.replace(message.id, freezeMessage({
        ...message,
        content: [{ type: 'text', text }],
      }))
      return { accepted: true }
    }
    if (action?.kind === 'steer') {
      if (agent.status !== 'running') throw new Error('current turn no longer accepts steering')
      if (!agent.inbox.remove(message.id)) {
        throw new Error('queued item is no longer pending: ' + itemId)
      }
      agent.steer(message)
      return { accepted: true }
    }
    if (action?.kind === 'send-now') {
      if (agent.status !== 'running') return { accepted: false }
      agent.cancel({ kind: 'user' }, { keepInbox: true })
      if (!agent.inbox.remove(message.id)) {
        throw new Error('queued item is no longer pending: ' + itemId)
      }
      prioritizeNextTurn(agent, message)
      return { accepted: true }
    }
    throw new TypeError('unknown queue action: ' + String(action?.kind))
  }

  // Resume a persisted session (protocol extension). The harness replays
  // the durable log and hands back a live agent for follow-ups.
  async function load(params) {
    const sessionId = String(params.sessionId)
    const existing = sessions.get(sessionId)
    if (existing !== undefined) {
      return { sessionId, alreadyLive: true }
    }
    const handle = await ctx.agents.resume({
      resumeSessionId: SessionId(sessionId),
      agentOptions: {
        provider: defaults.provider,
        model: defaults.model,
        ...(defaults.maxTokens === undefined ? {} : { maxTokens: defaults.maxTokens }),
      },
    })
    registerSession(sessionId, handle)
    return { sessionId }
  }

  // -------------------------------------------------------- model routes --
  // Resolve exact-route metadata so the picker uses the adapter-owned effort
  // vocabulary rather than inventing generic low/medium/high values.
  async function tuiCatalog(params) {
    const llm = ctx.get('llm')
    if (llm === undefined) throw new Error('no llm service is composed in this profile')
    const providers = llm.listProviders()
    const models = []
    const failures = []
    await Promise.all(providers.map(async (provider) => {
      try {
        const listed = await llm.listModels(provider.id)
        for (const model of listed) {
          const resolved = await llm.resolveModelInfo(provider.id, model.id)
          models.push({
            provider: provider.id,
            id: model.id,
            name: model.name ?? model.id,
            description: model.description ?? null,
            vision: !!(resolved.inputModalities || []).includes('image'),
            contextWindow: resolved.context?.contextWindow ?? null,
            reasoning: resolved.reasoning === undefined ? null : {
              efforts: resolved.reasoning.efforts.map((effort) => ({
                id: String(effort.id),
                name: effort.name,
                description: effort.description ?? null,
              })),
              defaultEffort: resolved.reasoning.defaultEffort === undefined
                ? null
                : String(resolved.reasoning.defaultEffort),
            },
          })
        }
      } catch (error) {
        failures.push(provider.id + ': ' + String(error))
      }
    }))
    let permissionPresets
    const perm = ctx.get('permissionPresets')
    if (perm !== undefined && Array.isArray(perm.names)) permissionPresets = perm.names
    const sessionId = params?.sessionId === undefined ? undefined : String(params.sessionId)
    const handle = sessionId === undefined ? undefined : sessions.get(sessionId)
    const current = handle === undefined
      ? {
          provider: defaults.provider,
          model: defaults.model,
          ...(defaults.reasoningEffort === undefined
            ? {}
            : { reasoningEffort: defaults.reasoningEffort }),
        }
      : selectionFor(handle.agent).current
    const commands = handle === undefined
      ? []
      : ctx.commands.list(handle.agent).map((command) => ({
          name: command.name,
          description: command.description,
          inputHint: command.input?.hint ?? null,
        }))
    const tools = ctx.tools.schemas(handle?.agent).map((tool) => ({
      name: tool.name,
      description: tool.description,
    }))
    const capabilities = {
      models: true,
      permissions: perm !== undefined,
      planMode: ctx.get('planMode') !== undefined,
      compaction: ctx.get('compaction') !== undefined,
      jobs: ctx.get('jobs') !== undefined,
      userQuestions: userQuestions !== undefined,
      sessionSearch: ctx.get('sessionQuery') !== undefined,
      commands: commands.length > 0,
      tools: tools.length > 0,
    }
    return {
      permissionPresets: permissionPresets ?? null,
      capabilities,
      commands,
      tools,
      providers: providers.map((provider) => ({ id: provider.id, name: provider.name ?? provider.id })),
      models,
      failures,
      current,
    }
  }

  async function tuiSelectModel(params) {
    const llm = ctx.get('llm')
    if (llm === undefined) throw new Error('no llm service is composed in this profile')
    const provider = params.provider === undefined ? defaults.provider : String(params.provider)
    const model = params.model === undefined ? defaults.model : String(params.model)
    const requested = {
      provider,
      model,
      ...(params.reasoningEffort === undefined || params.reasoningEffort === null
        ? {}
        : { reasoningEffort: String(params.reasoningEffort) }),
    }
    const resolved = await llm.resolveCallConfig(requested)
    const current = {
      provider: resolved.provider,
      model: resolved.model,
      ...(resolved.reasoningEffort === undefined
        ? {}
        : { reasoningEffort: String(resolved.reasoningEffort) }),
    }
    const sessionId = params.sessionId === undefined ? undefined : String(params.sessionId)
    const handle = sessionId === undefined ? undefined : sessions.get(sessionId)
    if (handle !== undefined) selectionFor(handle.agent).current = current

    defaults.provider = current.provider
    defaults.model = current.model
    defaults.reasoningEffort = current.reasoningEffort
    try {
      await ctx.agentDefaultModel.saveSelection(current)
    } catch (error) {
      ctx.logger.warn('dsh-whale-tui: model switch applies to this session but default save failed: ' + String(error))
    }
    return { ok: true, current }
  }

  async function tuiExecuteCommand(params) {
    const sessionId = String(params.sessionId)
    const handle = sessions.get(sessionId)
    if (handle === undefined) throw new Error('unknown session: ' + sessionId)
    const line = String(params.line)
    const execution = await ctx.commands.execute(handle.agent, line, new AbortController().signal)
    if (execution === undefined) throw new Error('unknown Harness command: ' + line)
    return {
      commandId: String(execution.commandId),
      ...execution.result,
    }
  }

  // Live agents owned by THIS host (the TUI's own sessions). Used by the
  // /resume picker to skip sessions already open here.
  async function tuiLiveSessions() {
    const ids = []
    try {
      for (const agent of ctx.agents.list()) {
        if (agent.session !== undefined) ids.push(String(agent.session.id))
      }
    } catch {}
    return { ids }
  }

  // Grok 风格模式由 DSH 的计划协作与权限预设两个独立服务组成，
  // 因此 TUI 通过一次 RPC 同时驱动二者；首次提示前的选择先暂存。
  async function tuiMode(params) {
    const permissionPresets = ctx.get('permissionPresets')
    if (permissionPresets === undefined) {
      throw new Error('no permission-presets service in this profile')
    }
    const planMode = ctx.get('planMode')
    if (planMode === undefined) throw new Error('no plan-mode service in this profile')
    const sessionId = String(params.sessionId)
    const plan = params.plan
    if (typeof plan !== 'boolean') throw new TypeError('mode plan must be a boolean')
    const preset = String(params.preset)
    const names = Array.isArray(permissionPresets.names) ? permissionPresets.names : undefined
    if (names !== undefined && !names.includes(preset)) {
      throw new Error('unknown permission preset ' + preset + ' (known: ' + names.join(', ') + ')')
    }
    const handle = sessions.get(sessionId)
    if (handle === undefined) {
      pendingModes.set(sessionId, { plan, preset })
      return { ok: true, plan, applied: preset, staged: true }
    }
    const planOutcome = planMode.set(handle.agent, plan)
    permissionPresets.set(handle.agent.session, preset)
    return { ok: true, plan, planOutcome, applied: preset }
  }

  // Manual compaction over the harness's compaction seam. The agent must be
  // idle; busy / no-history outcomes surface as typed errors to the TUI.
  async function tuiCompact(params) {
    const compaction = ctx.get('compaction')
    if (compaction === undefined) throw new Error('no compaction service in this profile')
    const sessionId = String(params.sessionId)
    const handle = sessions.get(sessionId)
    if (handle === undefined) {
      // No session = no history yet; nothing to compact.
      return { ok: true, result: null }
    }
    const signal = AbortSignal.timeout(300000)
    const result = await compaction.compactNow(handle.agent, signal, 'dsh-whale-tui')
    return { ok: true, result: result ?? null }
  }

  // Rewind: fork the session at a turn boundary. The child agent is
  // created with the prefix events as its seed (the same mechanism the
  // official fork subagent uses), so model context restarts exactly at
  // the chosen user message.
  async function tuiRewind(params) {
    const sessionId = String(params.sessionId)
    const boundary = Number(params.boundary)
    const handle = sessions.get(sessionId)
    if (handle === undefined) throw new Error('unknown session: ' + sessionId)
    const source = handle.agent.session
    const seed = source.events.slice(0, boundary + 1)
    const childId = SessionId(crypto.randomUUID())
    const childHandle = await ctx.agents.create({
      sessionId: childId,
      meta: {
        cwd: defaults.cwd,
        parentSession: source.id,
        seedLength: seed.length,
      },
      seed,
      agentOptions: {
        provider: defaults.provider,
        model: defaults.model,
        ...(defaults.maxTokens === undefined ? {} : { maxTokens: defaults.maxTokens }),
      },
    })
    registerSession(String(childId), childHandle)
    return { ok: true, newSessionId: String(childId), boundary }
  }

  // Session facts for /session-info: model route, cwd, completed turns,
  // and the latest usage chunk (scanned backwards from the durable log).
  async function tuiSessionInfo(params) {
    const sessionId = String(params.sessionId)
    const handle = sessions.get(sessionId)
    if (handle === undefined) {
      return {
        sessionId,
        provider: defaults.provider,
        model: defaults.model,
        cwd: defaults.cwd,
        turns: 0,
        usage: null,
      }
    }
    const events = handle.agent.session.events
    let turns = 0
    let usage = null
    for (let i = events.length - 1; i >= 0; i--) {
      const e = events[i]
      if (e.type === 'turn/end') turns++
      if (usage === null && e.type === 'assistant/chunk') {
        const c = e.data && e.data.chunk
        if (c && c.type === 'usage' && c.usage) usage = c.usage
      }
    }
    return {
      sessionId,
      provider: defaults.provider,
      model: defaults.model,
      cwd: defaults.cwd,
      turns,
      usage,
    }
  }

  // Background jobs (bash/subagent) for the tasks pane (Ctrl+G).
  async function tuiJobs() {
    const jobs = ctx.get('jobs')
    if (jobs === undefined) return { jobs: [] }
    return {
      jobs: jobs.list().map((j) => ({
        id: String(j.id),
        kind: j.kind,
        label: j.label,
        status: j.status,
        detail: j.detail ?? null,
      })),
    }
  }

  // Cancel only the current turn. Harness keeps queued work and background
  // agents alive; a prompt accepted after abort becomes the next turn.
  async function cancel(params) {
    const sessionId = String(params.sessionId)
    const handle = sessions.get(sessionId)
    if (handle === undefined) throw new Error('unknown session: ' + sessionId)
    handle.agent.cancel({ kind: 'user' }, { keepInbox: true })
    return { ok: true }
  }

  // ------------------------------------------------------------ shutdown --
  async function performShutdown() {
    shuttingDown = true
    // Cancel any running turns first so dispose never blocks on live work.
    for (const rec of sessions.values()) {
      try { rec.agent.cancel({ kind: 'disposed' }) } catch {}
    }
    await Promise.allSettled([...sessionCreations.values()])
    sessionCreations.clear()
    const records = [...sessions.values()]
    sessions.clear()
    while (disposers.length > 0) {
      try { disposers.pop()?.() } catch {}
    }
    await Promise.allSettled(
      records.map((rec) => Promise.resolve().then(() => rec.dispose())),
    )
    return {}
  }

  function shutdown() {
    shutdownTask ??= performShutdown()
    return shutdownTask
  }

  // The TUI owns the product lifetime in this profile: after the shutdown
  // response is written, flush, dispose the root runtime, and exit.
  let closing = false
  const disposeAndExit = (code) => {
    if (closing) return
    closing = true
    // Hard fallback: the host must exit even if some plugin effect disposes
    // slowly (observed: full fiber dispose can stall the launcher teardown).
    const fallback = setTimeout(() => process.exit(code), 8000)
    void (async () => {
      try {
        await Promise.allSettled([Promise.resolve().then(() => transport.flush())])
        await Promise.race([
          Promise.resolve().then(() => ctx.root.fiber.dispose()),
          new Promise((resolve) => setTimeout(resolve, 5000)),
        ])
      } finally {
        clearTimeout(fallback)
        process.exit(code)
      }
    })()
  }

  // ------------------------------------------------------------ dispatch --
  transport.onRequest(async (method, params) => {
    switch (method) {
      case 'initialize':
        return initialize(params)
      case 'session/prompt':
        return prompt(params)
      case 'session/send-now':
        return sendNow(params)
      case 'tui/execute-command':
        return tuiExecuteCommand(params)
      case 'session/update-queue':
        return updateQueue(params)
      case 'session/load':
        return load(params)
      case 'tui/catalog':
        return tuiCatalog(params)
      case 'tui/select-model':
        return tuiSelectModel(params)
      case 'tui/live-sessions':
        return tuiLiveSessions()
      case 'tui/mode':
        return tuiMode(params)
      case 'tui/compact':
        return tuiCompact(params)
      case 'tui/rewind':
        return tuiRewind(params)
      case 'tui/session-info':
        return tuiSessionInfo(params)
      case 'tui/jobs':
        return tuiJobs()
      case 'session/cancel':
        return cancel(params)
      case 'shutdown': {
        // Answer immediately so the TUI can quit; cleanup runs in the
        // background and a hard timer guarantees the host exits even when a
        // plugin disposer stalls.
        setImmediate(() => {
          const hard = setTimeout(() => process.exit(0), 8000)
          void shutdown()
            .catch(() => {})
            .then(() => {
              clearTimeout(hard)
              disposeAndExit(0)
            })
        })
        return { ok: true }
      }
      default:
        throw new Error('unknown method: ' + method)
    }
  })

  // ------------------------------------------------------ child lifecycle --
  child.on('exit', (code) => {
    if (closing) return
    disposeAndExit(code === null ? 0 : code)
  })
  child.on('error', (err) => {
    if (closing) return
    closing = true
    console.error('dsh-whale-tui runner failed: ' + err.message)
    process.exit(1)
  })

  ctx.effect(() => {
    transport.start()
    return async () => {
      closing = true
      try { child.kill('SIGTERM') } catch {}
      await shutdown().catch(() => {})
      transport.close()
    }
  }, 'dsh-whale.serve')
}

export default { name, inject, apply }
