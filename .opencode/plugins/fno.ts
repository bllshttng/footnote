// footnote's self-contained opencode orchestration plugin.
//
// Gives footnote native opencode orchestration with no external dependency.
// Built on opencode's plugin API:
// a config hook (register footnote's agents), a system-prompt transform
// (inject the orchestrator identity), and a `task` delegation tool. opencode's
// NATIVE machinery does the rest — it auto-loads `.opencode/agents/*.md` and
// discovers `skills/**/SKILL.md`, so this plugin only supplies what opencode
// can't infer: footnote's identity, its existing agents, and delegation.
//
// No build step: opencode auto-scans `.opencode/plugins/*.{ts,js}` and loads
// this .ts directly. See .opencode/README.md for the dogfood/cutover contract.

import { tool, type ToolDefinition } from "@opencode-ai/plugin"
import type { Plugin, PluginInput } from "@opencode-ai/plugin"
import { execFile } from "node:child_process"
import { readFileSync, readdirSync, existsSync } from "node:fs"
import { join, basename } from "node:path"

// Fleet announcements at the system-prompt boundary. One bus line,
// one per-session cursor; a session that already read the id hears silence.
// Fail-open: any error or missing binary injects nothing.
async function injectAnnouncements(
  input: unknown,
  output: { system: string[] },
): Promise<void> {
  try {
    const session = input as { session?: { id?: string }; sessionID?: string } | null
    const sessionId = session?.session?.id ?? session?.sessionID
    if (!sessionId) return
    const bin = process.env.FNO_AGENTS_BIN || "fno-agents"
    const out = await new Promise<string>((resolve) => {
      execFile(
        bin,
        ["announce", "read", "--session-id", sessionId, "--harness", "opencode", "--boundary", "prompt"],
        { timeout: 2000 },
        (err, stdout) => resolve(err ? "" : String(stdout)),
      )
    })
    const text = out.trim()
    if (text) output.system.push(text)
  } catch {
    // A hook must never block a session on announcement state.
  }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

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

/**
 * Minimal frontmatter reader: extracts scalar `key: value` pairs from the
 * leading `---` block and returns the body. Deliberately scalar-only — the
 * only fields this plugin needs are `description` and `model`; complex YAML
 * (tool arrays, nested skills) is ignored, not parsed. A full YAML dependency
 * would be over-engineering for three fields.
 */
export function parseFrontmatter(
  raw: string,
): { data: Record<string, string | string[]>; body: string } {
  const m = raw.match(/^---\s*\r?\n([\s\S]*?)\r?\n---\s*\r?\n?([\s\S]*)$/)
  if (!m) return { data: {}, body: raw }
  const data: Record<string, string | string[]> = {}
  const unquote = (v: string) => v.replace(/^["']/, "").replace(/["']$/, "")
  for (const line of m[1].split(/\r?\n/)) {
    const kv = line.match(/^([A-Za-z0-9_]+):\s*(.*)$/)
    if (!kv) continue // skips list items, nested keys, blanks
    const value = kv[2].trim()
    if (
      (value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"))
    ) {
      data[kv[1]] = value.slice(1, -1)
      continue
    }
    // Inline list values (`tools: ["Read", "Write"]`) survive as arrays - the
    // restriction fields die one call before the translator when dropped here.
    if (value.startsWith("[") && value.endsWith("]")) {
      const items = value
        .slice(1, -1)
        .split(",")
        .map((x) => unquote(x.trim()))
        .filter(Boolean)
      if (items.length) data[kv[1]] = items
      continue
    }
    if (value === "" || value.startsWith("{")) continue
    data[kv[1]] = value
  }
  return { data, body: m[2].trimStart() }
}

/** opencode AgentConfig-shaped object. `model` is a "provider/model" string;
 * `tools` is opencode's disable-only record: a key present with false is
 * withheld from the agent. */
export type AgentDef = {
  description?: string
  mode: "subagent"
  prompt: string
  model?: string
  tools?: Record<string, boolean>
}

/**
 * The translation of one footnote agent: either an opencode definition or a
 * named refusal. A refusal means the definition declares a restriction this
 * vocabulary cannot express; the agent is NOT registered and no unrestricted
 * fallback is registered in its place.
 */
export type AgentTranslation =
  | { ok: true; def: AgentDef }
  | { ok: false; agent: string; field: string; value: string }

/**
 * Translate a footnote (Claude Code format) agent markdown into an opencode
 * agent definition. Bare CC short model names (sonnet/haiku/opus) are dropped
 * so the child falls back to opencode's default — forcing an unmapped name
 * would fail agent resolution. A "provider/model" string is passed through.
 *
 * Restrictions: `disallowedTools` carries into opencode's disable-only tools
 * record (`{ name: false }`). An allowlist `tools` CANNOT be expressed there
 * — the record withholds only what it names false, everything unlisted stays
 * enabled — so a definition carrying one is refused outright rather than
 * registered as though it had asked for no restriction at all.
 */
export function toOpencodeAgent(
  data: Record<string, string | string[]>,
  body: string,
  name = "agent",
): AgentTranslation {
  const def: AgentDef = { mode: "subagent", prompt: body }
  if (typeof data.description === "string") def.description = data.description
  if (typeof data.model === "string" && data.model.includes("/")) def.model = data.model
  if (Array.isArray(data.tools)) {
    return { ok: false, agent: name, field: "tools", value: JSON.stringify(data.tools) }
  }
  if (Array.isArray(data.disallowedTools)) {
    const tools: Record<string, boolean> = {}
    for (const t of data.disallowedTools) {
      if (typeof t === "string" && t) tools[t.toLowerCase()] = false
    }
    def.tools = tools
  }
  return { ok: true, def }
}

/** Extract the assistant's COMPLETED text from a message's parts. Reasoning
 * parts never join a deliverable on any path (AC5-ERR, AC5-EDGE). */
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
 * prints - registration is where a restriction would be lost, so the refusal
 * is decided before it. */
export function loadFootnoteAgents(projectDir: string): {
  agents: Record<string, AgentDef>
  refusals: AgentTranslation[]
} {
  const dir = join(projectDir, "agents")
  const agents: Record<string, AgentDef> = {}
  const refusals: AgentTranslation[] = []
  if (!existsSync(dir)) return { agents, refusals }
  for (const file of readdirSync(dir)) {
    if (!file.endsWith(".md")) continue
    const name = basename(file, ".md")
    try {
      const { data, body } = parseFrontmatter(readFileSync(join(dir, file), "utf8"))
      // footnote agents are addressed as `fno:<name>` in the pipeline.
      const t = toOpencodeAgent(data, body, `fno:${name}`)
      if (t.ok) agents[`fno:${name}`] = t.def
      else refusals.push(t)
    } catch {
      // A malformed agent file must not abort registration of the rest.
    }
  }
  return { agents, refusals }
}

// ---------------------------------------------------------------------------
// Session-delegation client surface (the slice of the opencode SDK we use)
// ---------------------------------------------------------------------------

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

/** Live children of one parent, straight from the server. `null` when the
 * read fails - an unreadable count is never read as headroom. */
async function liveChildren(
  client: SessionClient,
  parentId: string,
): Promise<Array<{ id?: string; parentID?: string }> | null> {
  const res = await client.session
    .list({})
    .catch(() => null)
  const rows = (res as { data?: Array<{ id?: string; parentID?: string }> } | null)?.data
  if (!Array.isArray(rows)) return null
  return rows.filter((s) => s?.parentID === parentId)
}

function pendingNonceCount(parentId: string, ownToken: string): number {
  let n = 0
  for (const key of reservations.get(parentId)?.keys() ?? []) {
    if (key !== ownToken && key.startsWith("nonce-")) n += 1
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
      const total = live.length + pendingNonceCount(parentKey, nonce)
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

/**
 * Activation gate. The plugin auto-loads (opencode scans `.opencode/plugins/`),
 * but stays INERT unless explicitly opted in — so merely opening this repo in
 * opencode while another orchestration plugin is still globally active never
 * collides on the `task` tool. Dogfood with `FNO_OPENCODE=1 opencode`; flip the
 * global cutover on your own schedule. See .opencode/README.md.
 */
export function isActivated(env: Record<string, string | undefined> = process.env): boolean {
  const v = env.FNO_OPENCODE
  return v === "1" || v === "true"
}

const plugin: Plugin = async (input: PluginInput) => {
  if (!isActivated()) return {} // inert until opted in
  const client = input.client as unknown as SessionClient
  const projectDir = input.directory

  const orchestratorPrompt = loadOrchestratorPrompt(projectDir)
  const { agents: footnoteAgents, refusals } = loadFootnoteAgents(projectDir)

  // Registered-agent set: footnote's translated agents plus the native
  // `.opencode/agents/*.md` (explore/oracle/librarian) opencode auto-loads.
  const nativeAgents = new Set<string>()
  const nativeDir = join(projectDir, ".opencode", "agents")
  if (existsSync(nativeDir)) {
    for (const f of readdirSync(nativeDir)) if (f.endsWith(".md")) nativeAgents.add(basename(f, ".md"))
  }
  const knownAgents = () => new Set<string>([...Object.keys(footnoteAgents), ...nativeAgents])

  const taskTool = createTaskTool({
    client,
    directory: projectDir,
    knownAgents,
  })
  const taskResultTool = createTaskResultTool({ client })

  return {
    async config(config: Record<string, unknown>) {
      // A refused definition is printed once, by name, and nothing is
      // registered in its place - an unexpressible restriction never
      // degrades into an unrestricted agent.
      for (const r of refusals) {
        if (!("agent" in r)) continue
        console.error(
          `[footnote] agent "${r.agent}" NOT registered: field ${r.field} = ${r.value} ` +
            `cannot be expressed in opencode's agent vocabulary ` +
            `(the tools record is disable-only). Convert the allowlist to ` +
            `disallowedTools, or narrow the definition.`,
        )
      }
      const agent = (config.agent ?? {}) as Record<string, unknown>
      for (const [name, def] of Object.entries(footnoteAgents)) {
        if (!(name in agent)) agent[name] = def
      }
      config.agent = agent
    },
    async "experimental.chat.system.transform"(input: unknown, output: { system: string[] }) {
      output.system.unshift(orchestratorPrompt)
      await injectAnnouncements(input, output)
    },
    tool: {
      task: taskTool,
      task_result: taskResultTool,
    },
  }
}

export default { id: "fno", server: plugin }
