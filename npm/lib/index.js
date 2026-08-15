/**
 * dsh-whale-tui runner (skeleton) — cordis plugin that mounts the native TUI.
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
 * Planned (docs/02-openma-teardown.md section 12):
 *   approval/request + ask_user_question bridging (bidirectional channel),
 *   tui/catalog + tui/select-model (model picker), tui/permission (presets).
 *
 * The agent, tools, persistence, and providers come from the surrounding dsh
 * profile. Stdout of the host process is the TUI screen — keep stdout
 * loggers out of the profile.
 */

import { spawn } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import { JsonRpcLineTransport } from '@deepseek-ai/dsh-sdk-protocol'
import { createUserMessage } from '@deepseek-ai/dsh-llm'
import { SessionId } from '@deepseek-ai/dsh-session'

const name = 'dsh-whale-runner'
const inject = ['agents']

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
  /** sessionId -> permission preset staged before the session exists. */
  const pendingPermissions = new Map()
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
  disposers.push(ctx.on('session/created', (session) => {
    const parentSession = session.header.parentSession
    if (parentSession === undefined) return
    transport.notify('subagent.started', {
      parentSessionId: String(parentSession),
      childSessionId: String(session.id),
    })
  }))

  // ------------------------------------------ interactive dialogs --------
  // Approval: answer the approval/request waterfall from the TUI. On any
  // transport failure we delegate (fail-closed via the default answerer).
  disposers.push(ctx.on('approval/request', async (req, next) => {
    try {
      const signal = AbortSignal.timeout(120000)
      const result = await transport.request('ui/approve', {
        id: String(req.id),
        toolName: req.toolName,
        reason: req.reason ?? null,
        input: req.input ?? null,
        options: ['allowed-once', 'always-allow', 'rejected'],
      }, signal)
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
        const signal = AbortSignal.timeout(120000)
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
        }, signal)
        return { answers: result.answers }
      },
    }))
  }

  // ---------------------------------------------------------- sessions --
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
    sessions.set(sessionId, handle)
    const staged = pendingPermissions.get(sessionId)
    if (staged !== undefined) {
      pendingPermissions.delete(sessionId)
      const svc = ctx.get('permissionPresets')
      if (svc !== undefined) {
        try { svc.set(handle.agent.session, staged) } catch {}
      }
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
    defaults.provider = String(params.provider)
    defaults.model = String(params.model)
    defaults.maxTokens = params.maxTokens
    return { serverInfo: { name: 'dsh-whale-tui-shim', version: '0.1.5' } }
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
    sessions.set(sessionId, handle)
    return { sessionId }
  }

  // -------------------------------------------------------- model routes --
  // Model catalog for the picker (providers + models + current selection).
  async function tuiCatalog() {
    const llm = ctx.get('llm')
    if (llm === undefined) throw new Error('no llm service is composed in this profile')
    const providers = llm.listProviders()
    const models = []
    await Promise.all(providers.map(async (p) => {
      try {
        for (const m of await llm.listModels(p.id)) models.push(m)
      } catch {} // tolerate one provider's listing failure
    }))
    let permissionPresets
    const perm = ctx.get('permissionPresets')
    if (perm !== undefined && Array.isArray(perm.names)) permissionPresets = perm.names
    return {
      permissionPresets: permissionPresets ?? null,
      providers: providers.map((p) => ({ id: p.id, name: p.name ?? p.id })),
      models: models.map((m) => ({
        provider: m.provider,
        id: m.id,
        name: m.name ?? m.id,
        vision: !!(m.inputModalities || []).includes('image'),
      })),
      current: { provider: defaults.provider, model: defaults.model },
    }
  }

  // Per-session model switch. Future sessions inherit the new defaults;
  // session-level hot switch (openma's installModelSelection) is a later step.
  async function tuiSelectModel(params) {
    const llm = ctx.get('llm')
    if (llm === undefined) throw new Error('no llm service is composed in this profile')
    const provider = params.provider === undefined ? defaults.provider : String(params.provider)
    const model = params.model === undefined ? defaults.model : String(params.model)
    const effort = params.reasoningEffort === undefined
      ? defaults.reasoningEffort
      : (params.reasoningEffort ?? undefined)
    const next = {
      provider,
      model,
      ...(effort === undefined ? {} : { reasoningEffort: effort }),
    }
    await llm.resolveCallConfig(next)
    defaults.provider = provider
    defaults.model = model
    defaults.reasoningEffort = effort
    return { ok: true, current: next }
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

  // Switch the live session's permission preset (Shift+Tab cycling). A
  // preset chosen before the first prompt is staged and applied at creation.
  async function tuiPermission(params) {
    const svc = ctx.get('permissionPresets')
    if (svc === undefined) throw new Error('no permission-presets service in this profile')
    const sessionId = String(params.sessionId)
    const preset = String(params.preset)
    const names = Array.isArray(svc.names) ? svc.names : undefined
    if (names !== undefined && !names.includes(preset)) {
      throw new Error('unknown permission preset ' + preset + ' (known: ' + names.join(', ') + ')')
    }
    const handle = sessions.get(sessionId)
    if (handle === undefined) {
      pendingPermissions.set(sessionId, preset)
      return { ok: true, applied: preset, staged: true }
    }
    svc.set(handle.agent.session, preset)
    return { ok: true, applied: preset }
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
    sessions.set(String(childId), childHandle)
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

  // Our protocol extension: hard-cancel the running turn.
  async function cancel(params) {
    const sessionId = String(params.sessionId)
    const handle = sessions.get(sessionId)
    if (handle === undefined) throw new Error('unknown session: ' + sessionId)
    handle.agent.cancel({ kind: 'user' })
    return { ok: true }
  }

  // ------------------------------------------------------------ shutdown --
  async function performShutdown() {
    shuttingDown = true
    // Cancel any running turns first so dispose never blocks on live work.
    for (const rec of sessions.values()) {
      try { rec.agent.cancel({ kind: 'shutdown' }) } catch {}
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
      case 'session/load':
        return load(params)
      case 'tui/catalog':
        return tuiCatalog()
      case 'tui/select-model':
        return tuiSelectModel(params)
      case 'tui/live-sessions':
        return tuiLiveSessions()
      case 'tui/permission':
        return tuiPermission(params)
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
