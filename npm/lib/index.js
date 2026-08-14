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
  const transport = new JsonRpcLineTransport(child.stdio[4], child.stdio[3])

  const defaults = {
    cwd: process.cwd(),
    provider: 'deepseek-official',
    model: 'deepseek-v4-flash',
    maxTokens: undefined,
  }
  /** sessionId -> handle, for sessions this server created. */
  const sessions = new Map()
  /** sessionId -> in-flight creation promise (dedupes concurrent prompts). */
  const sessionCreations = new Map()
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
      case 'session/cancel':
        return cancel(params)
      case 'shutdown': {
        const result = await shutdown()
        setImmediate(() => disposeAndExit(0))
        return result
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
