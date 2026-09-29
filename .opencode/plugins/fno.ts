// footnote's repo-local opencode orchestration plugin.
//
// After the global install (footnote's bridge hosts hooks.json, the
// installer writes the agents, and oh-my-openagent is switched off), only
// what is specific to THIS repository remains: the task/task_result
// delegation tools, the orchestrator prompt injection, and the agent set
// they may name. Everything global moved to
// ~/.config/opencode/plugins/footnote.js (installed by
// fno config plugin install opencode).
//
// No build step: opencode auto-scans .opencode/plugins/*.{ts,js} and loads
// this .ts directly.

import { tool, type ToolDefinition } from "@opencode-ai/plugin"
import type { Plugin, PluginInput } from "@opencode-ai/plugin"
import { readFileSync, readdirSync, existsSync } from "node:fs"
import { join, basename } from "node:path"


// subagent_type -> workflow category, when `category` is omitted.
const AGENT_INFERENCE: Record<string, string> = {
  "fno:archer": "do",
  archer: "do",
  "fno:scout": "research",
  scout: "research",
  explore: "research",
  oracle: "think",
  librarian: "research",
  "fno:verifier": "review",
  verifier: "review",
  "fno:code-reviewer": "review",
  "code-reviewer": "review",
}

const MAX_DEPTH = 3
const MAX_CONCURRENCY = 5
const SYNC_TIMEOUT_MS = 120_000

// ---------------------------------------------------------------------------
// Pure helpers (exported for unit tests — no SDK runtime deps)
// ---------------------------------------------------------------------------

/** Infer the workflow category from a subagent_type. */
export function inferCategory(subagentType?: string): string | undefined {
  if (!subagentType) return undefined
  return AGENT_INFERENCE[subagentType]
}



export function extractAssistantText(parts: Array<{ type?: string; text?: string }> | undefined): string {
  if (!parts) return ""
  return parts
    .filter((p) => p.type === "text")
    .map((p) => p.text ?? "")
    .filter(Boolean)
    .join("\n")
    .trim()
}

// ---------------------------------------------------------------------------
// Agent loading
// ---------------------------------------------------------------------------

/** Read + translate every `agents/*.md` under the project. Returns the
 * registerable defs and, separately, the named refusals the config hook


type SessionClient = {
  session: {
    create(o: { body: Record<string, unknown>; query?: { directory?: string } }): Promise<{ data?: { id: string }; error?: unknown }>
    list(o?: { query?: { directory?: string } }): Promise<{ data?: Array<{ id?: string; parentID?: string }>; error?: unknown }>
    get(o: { path: { id: string } }): Promise<{ data?: { parentID?: string }; error?: unknown }>
    prompt(o: { path: { id: string }; body: Record<string, unknown> }): Promise<{ data?: { parts?: Array<{ type?: string; text?: string }> }; error?: unknown }>
    promptAsync(o: { path: { id: string }; body: Record<string, unknown> }): Promise<{ error?: unknown }>
    messages(o: { path: { id: string } }): Promise<{ data?: Array<{ info?: { role?: string }; parts?: Array<{ type?: string; text?: string }> }>; error?: unknown }>
    abort(o: { path: { id: string } }): Promise<unknown>
  }
}

/** Walk the parentID chain to count how deep `sessionId` already is. */
async function sessionDepth(client: SessionClient, sessionId: string): Promise<number> {
  let depth = 0
  let id: string | undefined = sessionId
  const seen = new Set<string>()
  // Stop at MAX_DEPTH: the caller rejects once depth >= MAX_DEPTH, so walking
  // deeper only adds redundant session.get round-trips.
  while (id && !seen.has(id) && depth < MAX_DEPTH) {
    seen.add(id)
    const res: { data?: { parentID?: string } } | null = await client.session
      .get({ path: { id } })
      .catch(() => null)
    const parent: string | undefined = res?.data?.parentID
    if (!parent) break
    depth += 1
    id = parent
  }
  return depth
}

// Per-parent delegation reservations (Change 3). The reservation is inserted
// synchronously under a nonce BEFORE the depth walk and before session.create,
// so two concurrent callers can never both read a pre-increment count; it is
// rekeyed to the child id once create returns. Background children hold a
// reservation like any other child and release it when task_result reads a
// terminal state. In-memory only and deliberately not authority: admission
// reconciles against the live child set on every fire, so a plugin reload
// cannot leak capacity - and an unreadable count refuses as `capacity
// unknown`, never as headroom.
const reservations = new Map<string, Map<string, unknown>>()
let nonceCounter = 0

function reserve(parentId: string): string {
  const token = `nonce-${Date.now()}-${nonceCounter++}`
  let slot = reservations.get(parentId)
  if (!slot) {
    slot = new Map()
    reservations.set(parentId, slot)
  }
  slot.set(token, true)
  return token
}

function rekeyReservation(parentId: string, token: string, childId: string): void {
  const slot = reservations.get(parentId)
  if (!slot || !slot.has(token)) return
  slot.delete(token)
  slot.set(childId, true)
}

function releaseReservation(key: string): void {
  for (const slot of reservations.values()) slot.delete(key)
}

/** Live children of one parent, straight from the server: a child is live
 * while its latest assistant state is pending or running. A finished child
 * stays a session row forever, so counting rows would fill the cap
 * permanently. Returns `null` when any reconciliation read fails - an
 * unreadable count is never read as headroom. */
async function liveChildren(client: SessionClient, parentId: string): Promise<Set<string> | null> {
  const res = await client.session
    .list({})
    .catch(() => null)
  const rows = (res as { data?: Array<{ id?: string; parentID?: string }> } | null)?.data
  if (!Array.isArray(rows)) return null
  const live = new Set<string>()
  for (const row of rows) {
    if (!row?.id || row.parentID !== parentId) continue
    const id = row.id
    const m = await client.session
      .messages({ path: { id } })
      .catch(() => null)
    if (m === null || (m as { error?: unknown }).error) return null
    const state = buildTaskResult(id, (m as { data?: ChildMessage[] }).data).state
    if (state === "pending" || state === "running") live.add(id)
  }
  return live
}

/** Reservations the live set does not already account for: pending nonces and
 * children created after the list was read. */
function pendingReservations(parentId: string, ownToken: string, liveIds: Set<string>): number {
  let n = 0
  for (const key of reservations.get(parentId)?.keys() ?? []) {
    if (key !== ownToken && !liveIds.has(key)) n += 1
  }
  return n
}

type TaskDeps = {
  client: SessionClient
  directory: string
  knownAgents: () => Set<string>
  timeoutMs?: number
}

// ---------------------------------------------------------------------------
// Typed delegation results (Change 4)
// ---------------------------------------------------------------------------

export type TaskResultState = "pending" | "running" | "completed" | "failed" | "aborted" | "blocked"

/** The one delegation envelope both the synchronous and background paths
 * return. `result` is present only on `completed`; `provider_id`/`model_id`
 * are the child's own readback when the child message carries them. */
export type TaskResult = {
  state: TaskResultState
  child_session_id: string
  provider_id?: string
  model_id?: string
  result?: string
}

type AssistantInfo = {
  role?: string
  time?: { created?: number; completed?: number }
  error?: { name?: string }
  providerID?: string
  modelID?: string
}

/** Terminal is `time.completed` present with `error` absent; a present error
 * maps to failed, or aborted for MessageAbortedError. Never guesses. */
export function classifyAssistant(
  info: AssistantInfo | undefined,
): "completed" | "failed" | "aborted" | "running" {
  if (info?.error) return info.error.name === "MessageAbortedError" ? "aborted" : "failed"
  if (typeof info?.time?.completed === "number") return "completed"
  return "running"
}

type ChildMessage = { info?: AssistantInfo; parts?: Array<{ type?: string; text?: string }> }

/** Build the delegation envelope from a child session's messages. Reasoning
 * alone never yields a deliverable (AC5-*). */
export function buildTaskResult(taskId: string, messages: ChildMessage[] | undefined): TaskResult {
  const list = Array.isArray(messages) ? messages : []
  const assistant = list.filter((m) => m?.info?.role === "assistant")
  if (assistant.length === 0) return { state: "pending", child_session_id: taskId }
  const last = assistant[assistant.length - 1]
  const cls = classifyAssistant(last.info)
  const info = last.info ?? {}
  if (cls === "running") return { state: "running", child_session_id: taskId }
  const readback: TaskResult =
    info.providerID || info.modelID
      ? {
          state: cls,
          child_session_id: taskId,
          ...(info.providerID ? { provider_id: info.providerID } : {}),
          ...(info.modelID ? { model_id: info.modelID } : {}),
        }
      : { state: cls, child_session_id: taskId }
  if (cls === "completed") readback.result = extractAssistantText(last.parts)
  return readback
}

/** Build the `task` delegation tool. */
export function createTaskTool(deps: TaskDeps): ToolDefinition {
  const timeoutMs = deps.timeoutMs ?? SYNC_TIMEOUT_MS
  return tool({
    description:
      "Delegate work to a child agent session. Provide `category` (think|plan|do|review|ship|research) " +
      "or `subagent_type` (e.g. fno:archer, explore, oracle, librarian). Returns the child's result " +
      "synchronously, or a task_id when run_in_background is true.",
    args: {
      prompt: tool.schema.string().describe("Full prompt for the child agent."),
      category: tool.schema.string().optional().describe("Workflow category if subagent_type is omitted."),
      subagent_type: tool.schema.string().optional().describe("Explicit agent name if category is omitted."),
      description: tool.schema.string().optional().describe("Short 3-5 word task label."),
      run_in_background: tool.schema.boolean().optional().describe("true = launch async and return a task_id."),
    },
    async execute(args, context) {
      const category = args.category ?? inferCategory(args.subagent_type)
      const agent = args.subagent_type ?? categoryDefaultAgent(category)

      if (!args.category && !args.subagent_type) {
        return "error: task() requires either `category` or `subagent_type` (ambiguous delegation target)."
      }
      if (!agent) {
        return `error: could not resolve an agent for category "${category}". Provide subagent_type explicitly.`
      }
      if (args.subagent_type && !deps.knownAgents().has(args.subagent_type)) {
        const available = [...deps.knownAgents()].sort().join(", ")
        return `error: unknown agent "${args.subagent_type}". Available: ${available}`
      }

      // Reserve synchronously, BEFORE any await: two concurrent callers can
      // never both read a pre-increment count (AC4-HP).
      const parentKey = context.sessionID
      const nonce = reserve(parentKey)
      let resKey = nonce
      const release = () => releaseReservation(resKey)

      // Reconcile against the live child set before admitting.
      const live = await liveChildren(deps.client, parentKey)
      if (live === null) {
        release()
        return "error: capacity unknown (cannot read the live child set); no child created."
      }
      const total = live.size + pendingReservations(parentKey, nonce, live)
      if (total >= MAX_CONCURRENCY) {
        release()
        return `error: concurrency limit reached (cap ${MAX_CONCURRENCY}, ${total} child delegations in flight). Wait for a slot.`
      }

      const depth = await sessionDepth(deps.client, context.sessionID)
      if (depth >= MAX_DEPTH) {
        release()
        return `error: delegation depth limit reached (${MAX_DEPTH}). This session is already ${depth} level(s) deep.`
      }

      const title = `${args.description ?? agent} (@${agent})`

      const created = await deps.client.session
        .create({
          body: {
            parentID: context.sessionID,
            title,
          },
          query: { directory: deps.directory },
        })
        .catch((err) => ({ error: err, data: undefined }))
      const childId = created?.data?.id
      if (created?.error || !childId) {
        release()
        return `error: failed to create child session: ${String(created?.error ?? "no session id")}`
      }
      rekeyReservation(parentKey, nonce, childId)
      resKey = childId

      const body = {
        agent,
        parts: [{ type: "text", text: args.prompt }],
      }

      const background = args.run_in_background === true
      if (background) {
        const res = await deps.client.session
          .promptAsync({ path: { id: childId }, body })
          .catch((err) => ({ error: err }))
        if (res?.error) {
          release()
          return `error: failed to launch background task: ${String(res.error)}`
        }
        // The background child keeps its reservation until task_result reads
        // a terminal state (or the child is gone at the next reconcile).
        return `task_id: ${childId}\nBackground task launched (@${agent}). Fetch the result later with task_result({ task_id: "${childId}" }).`
      }

      try {
        const res = await withTimeout(
          deps.client.session.prompt({ path: { id: childId }, body }),
          timeoutMs,
          () => deps.client.session.abort({ path: { id: childId } }),
        ).catch((err) => ({ error: err, data: undefined }))
        if (res === TIMEOUT) {
          return JSON.stringify({
            state: "aborted" as TaskResultState,
            child_session_id: childId,
          })
        }
        if (res?.error) {
          return JSON.stringify({ state: "failed" as TaskResultState, child_session_id: childId })
        }
        const envelope = res?.data?.info
          ? buildTaskResult(childId, [{ info: res.data.info, parts: res.data.parts }])
          : extractAssistantText(res?.data?.parts)
            ? ({
                state: "completed",
                child_session_id: childId,
                result: extractAssistantText(res?.data?.parts),
              } as TaskResult)
            : ({ state: "running", child_session_id: childId } as TaskResult)
        return JSON.stringify(envelope)
      } finally {
        release()
      }
    },
  })
}

/** Fetch the result of a backgrounded task by its child session id. */
export function createTaskResultTool(deps: Pick<TaskDeps, "client">): ToolDefinition {
  return tool({
    description: "Fetch the result of a background task launched via task({ run_in_background: true }).",
    args: {
      task_id: tool.schema.string().describe("The task_id returned by the background task() call."),
    },
    async execute(args) {
      const res = await deps.client.session
        .messages({ path: { id: args.task_id } })
        .catch((err) => ({ error: err, data: undefined }))
      if (res?.error) {
        return JSON.stringify({ state: "unknown" as TaskResultState, child_session_id: args.task_id })
      }
      const envelope = buildTaskResult(args.task_id, res?.data)
      // A terminal read releases the background child's reservation (AC4-HP:
      // release happens on every outcome, incl. a terminal task_result).
      if (envelope.state !== "pending" && envelope.state !== "running") {
        releaseReservation(args.task_id)
      }
      return JSON.stringify(envelope)
    },
  })
}

function categoryDefaultAgent(category?: string): string | undefined {
  switch (category) {
    case "do":
      return "fno:archer"
    case "research":
      return "explore"
    case "think":
      return "oracle"
    case "review":
      return "fno:verifier"
    case "plan":
    case "ship":
      return "fno:archer"
    default:
      return undefined
  }
}

const TIMEOUT = Symbol("timeout")
async function withTimeout<T>(p: Promise<T>, ms: number, onTimeout: () => void): Promise<T | typeof TIMEOUT> {
  let timer: ReturnType<typeof setTimeout> | undefined
  const timeout = new Promise<typeof TIMEOUT>((resolve) => {
    timer = setTimeout(() => {
      try {
        onTimeout()
      } catch {
        /* abort is best-effort */
      }
      resolve(TIMEOUT)
    }, ms)
  })
  try {
    return await Promise.race([p, timeout])
  } finally {
    if (timer) clearTimeout(timer)
  }
}


// ---------------------------------------------------------------------------
// Plugin wiring
// ---------------------------------------------------------------------------

function loadOrchestratorPrompt(projectDir: string): string {
  // The orchestrator prompt lives at a fixed project-root-relative path, so
  // resolve from projectDir rather than the non-standard import.meta.dir.
  try {
    return readFileSync(join(projectDir, ".opencode", "fno-orchestrator.md"), "utf8")
  } catch {
    return "You are footnote's delivery orchestrator. Set a target, walk away, say f[no] to mostly done."
  }
}

// One body for each side effect both plugin arms share; the V1 and V2 hooks
// only differ in where the result lands.

/** The canon-doc pointer text for one session, or "" when none. Compaction


// ---------------------------------------------------------------------------
// Activation + wiring
// ---------------------------------------------------------------------------

/**
 * Activation gate. The global install replaced oh-my-openagent (its mirror
 * is what the FNO_OPENCODE opt-in used to guard against), so the plugin is
 * on unless explicitly disabled with FNO_OPENCODE=0|false.
 */
export function isActivated(env: Record<string, string | undefined> = process.env): boolean {
  const v = env.FNO_OPENCODE
  return v !== "0" && v !== "false"
}

const plugin = async (input) => {
  if (!isActivated()) return {} // off by explicit env only
  const client = input.client
  const projectDir = input.directory

  const orchestratorPrompt = loadOrchestratorPrompt(projectDir)

  // The agent set the task tool may name: whatever opencode already merged
  // (captured read-only here - the installer writes the agent files) plus
  // this repo's .opencode/agents/*.md.
  const mergedAgents = new Set()
  const nativeAgents = new Set()
  const nativeDir = join(projectDir, ".opencode", "agents")
  if (existsSync(nativeDir)) {
    for (const f of readdirSync(nativeDir)) if (f.endsWith(".md")) nativeAgents.add(basename(f, ".md"))
  }
  const knownAgents = () => new Set([...mergedAgents, ...nativeAgents])

  const taskTool = createTaskTool({ client, directory: projectDir, knownAgents })
  const taskResultTool = createTaskResultTool({ client })

  return {
    async config(config) {
      // Read-only: record the agent names opencode merged (footnote's own
      // installed agents among them) so delegation can validate names.
      for (const name of Object.keys((config.agent ?? {}))) mergedAgents.add(name)
    },
    async "experimental.chat.system.transform"(input, output) {
      output.system.unshift(orchestratorPrompt)
    },
    tool: {
      task: taskTool,
      task_result: taskResultTool,
    },
  }
}

// ---------------------------------------------------------------------------
// The opencode 2 arm. A V2 plugin is a plain { id, setup } object: V2's
// define(plugin) is the identity function, so no package import is needed
// and the file stays loadable on 1.14.50, which has no V2 package installed.
// The context type is structural, like SessionClient.
// ---------------------------------------------------------------------------

/** The slice of the opencode 2 plugin context this plugin uses. */
type V2SystemPart = { type: "text"; text: string }

type V2Context = {
  directory?: string
  session: {
    hook(
      name: "context",
      handler: (e: { system: V2SystemPart[]; sessionID?: string; session?: { id?: string } }) => void | Promise<void>,
    ): void
  }
}

/** The opencode 2 entrypoint: the orchestrator prompt only. Global hooks
 * belong to the installed bridge. opencode 1.18+ also calls setup with a
 * plugin-authoring context (agent, catalog, command, ...) that carries no
 * hook seams and uses no return value; the V1 server arm is the live one
 * there, so a context without the seams registers nothing. */
export function setupV2(ctx: V2Context): () => void {
  const hasV2Seams =
    ctx &&
    ctx.tool &&
    typeof ctx.tool.hook === "function" &&
    ctx.session &&
    typeof ctx.session.hook === "function"
  if (!hasV2Seams) return () => {}
  if (!isActivated()) return () => {}
  const projectDir = ctx.directory ?? process.cwd()

  const orchestratorPrompt = loadOrchestratorPrompt(projectDir)

  // Admission reads the live child set through session.list, which the
  // opencode 2 plugin session API does not expose. A tool that always
  // refused as capacity unknown helps nobody, so neither is offered.
  console.error(
    "[footnote] opencode 2: task delegation is unavailable; it needs a live child count that the OpenCode 2 plugin session API does not expose",
  )

  ctx.session.hook("context", async (event) => {
    event.system.push({ type: "text", text: orchestratorPrompt })
  })
  return () => {}
}

export default { id: "fno", server: plugin, setup: setupV2 }
