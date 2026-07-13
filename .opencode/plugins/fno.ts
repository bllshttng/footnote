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
import { readFileSync, readdirSync, existsSync } from "node:fs"
import { join, basename } from "node:path"

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

// category -> preferred model as "providerID/modelID". Best-effort: a model is
// only forced when the provider registry actually has it (see resolveModel);
// otherwise the child session uses opencode's default. Env-specific model names
// are intentionally NOT hardcoded blindly — a missing model must degrade, not
// break delegation (AC5-ERR).
const CATEGORY_MODEL: Record<string, string> = {
  // Left empty by default: routing rides each agent's own `model:` field plus
  // opencode's default. Populate per-environment, e.g.
  //   ship: "anthropic/claude-haiku-4-5",
  //   plan: "anthropic/claude-opus-4-6",
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
export function parseFrontmatter(raw: string): { data: Record<string, string>; body: string } {
  const m = raw.match(/^---\s*\r?\n([\s\S]*?)\r?\n---\s*\r?\n?([\s\S]*)$/)
  if (!m) return { data: {}, body: raw }
  const data: Record<string, string> = {}
  for (const line of m[1].split(/\r?\n/)) {
    const kv = line.match(/^([A-Za-z0-9_]+):\s*(.*)$/)
    if (!kv) continue // skips list items, nested keys, blanks
    let value = kv[2].trim()
    if (
      (value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"))
    ) {
      value = value.slice(1, -1)
    }
    if (value === "" || value.startsWith("[") || value.startsWith("{")) continue
    data[kv[1]] = value
  }
  return { data, body: m[2].trimStart() }
}

/** opencode AgentConfig-shaped object. `model` is a "provider/model" string. */
export type AgentDef = {
  description?: string
  mode: "subagent"
  prompt: string
  model?: string
}

/**
 * Translate a footnote (Claude Code format) agent markdown into an opencode
 * agent definition. Bare CC short model names (sonnet/haiku/opus) are dropped
 * so the child falls back to opencode's default — forcing an unmapped name
 * would fail agent resolution. A "provider/model" string is passed through.
 */
export function toOpencodeAgent(data: Record<string, string>, body: string): AgentDef {
  const def: AgentDef = { mode: "subagent", prompt: body }
  if (data.description) def.description = data.description
  if (data.model && data.model.includes("/")) def.model = data.model
  return def
}

/** Extract the assistant's text from a session.prompt response's parts. */
export function extractAssistantText(parts: Array<{ type?: string; text?: string }> | undefined): string {
  if (!parts) return ""
  return parts
    .filter((p) => p.type === "text" || p.type === "reasoning")
    .map((p) => p.text ?? "")
    .filter(Boolean)
    .join("\n")
    .trim()
}

/**
 * Resolve a "providerID/modelID" for a category, but only if the model exists
 * in the available set. Returns undefined to let opencode use its default.
 */
export function resolveModel(
  category: string | undefined,
  available: Set<string>,
): { providerID: string; modelID: string } | undefined {
  if (!category) return undefined
  const spec = CATEGORY_MODEL[category]
  if (!spec) return undefined
  const slash = spec.indexOf("/")
  if (slash < 0) return undefined
  const providerID = spec.slice(0, slash)
  const modelID = spec.slice(slash + 1)
  if (!available.has(`${providerID}/${modelID}`)) return undefined
  return { providerID, modelID }
}

/** Fold a provider.list() response into the available-model set (in place). */
export function collectModels(
  providers: { data?: Array<{ id: string; models?: Record<string, unknown> }> } | undefined,
  into: Set<string>,
): Set<string> {
  for (const p of providers?.data ?? []) {
    for (const modelID of Object.keys(p.models ?? {})) into.add(`${p.id}/${modelID}`)
  }
  return into
}

// ---------------------------------------------------------------------------
// Agent loading
// ---------------------------------------------------------------------------

/** Read + translate every `agents/*.md` under the project into opencode defs. */
export function loadFootnoteAgents(projectDir: string): Record<string, AgentDef> {
  const dir = join(projectDir, "agents")
  const out: Record<string, AgentDef> = {}
  if (!existsSync(dir)) return out
  for (const file of readdirSync(dir)) {
    if (!file.endsWith(".md")) continue
    const name = basename(file, ".md")
    try {
      const { data, body } = parseFrontmatter(readFileSync(join(dir, file), "utf8"))
      // footnote agents are addressed as `fno:<name>` in the pipeline.
      out[`fno:${name}`] = toOpencodeAgent(data, body)
    } catch {
      // A malformed agent file must not abort registration of the rest.
    }
  }
  return out
}

// ---------------------------------------------------------------------------
// Session-delegation client surface (the slice of the opencode SDK we use)
// ---------------------------------------------------------------------------

type SessionClient = {
  session: {
    create(o: { body: Record<string, unknown>; query?: { directory?: string } }): Promise<{ data?: { id: string }; error?: unknown }>
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

// module-level guard: only SYNC delegations (which hold this turn) are counted.
let inFlightSync = 0

type TaskDeps = {
  client: SessionClient
  directory: string
  knownAgents: () => Set<string>
  availableModels: () => Set<string>
  timeoutMs?: number
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

      const depth = await sessionDepth(deps.client, context.sessionID)
      if (depth >= MAX_DEPTH) {
        return `error: delegation depth limit reached (${MAX_DEPTH}). This session is already ${depth} level(s) deep.`
      }

      const background = args.run_in_background === true
      if (!background && inFlightSync >= MAX_CONCURRENCY) {
        return `error: concurrency limit reached (${MAX_CONCURRENCY} synchronous delegations in flight). Wait for a slot.`
      }

      const model = resolveModel(category, deps.availableModels())
      const title = `${args.description ?? agent} (@${agent})`

      const created = await deps.client.session
        .create({
          body: {
            parentID: context.sessionID,
            title,
            ...(model ? { model: { id: model.modelID, providerID: model.providerID } } : {}),
          },
          query: { directory: deps.directory },
        })
        .catch((err) => ({ error: err, data: undefined }))
      const childId = created?.data?.id
      if (created?.error || !childId) {
        return `error: failed to create child session: ${String(created?.error ?? "no session id")}`
      }

      const body = {
        agent,
        parts: [{ type: "text", text: args.prompt }],
        ...(model ? { model: { providerID: model.providerID, modelID: model.modelID } } : {}),
      }

      if (background) {
        const res = await deps.client.session
          .promptAsync({ path: { id: childId }, body })
          .catch((err) => ({ error: err }))
        if (res?.error) return `error: failed to launch background task: ${String(res.error)}`
        return `task_id: ${childId}\nBackground task launched (@${agent}). Fetch the result later with task_result({ task_id: "${childId}" }).`
      }

      inFlightSync += 1
      try {
        const res = await withTimeout(
          deps.client.session.prompt({ path: { id: childId }, body }),
          timeoutMs,
          () => deps.client.session.abort({ path: { id: childId } }),
        ).catch((err) => ({ error: err, data: undefined }))
        if (res === TIMEOUT) {
          return `error: child session timed out after ${timeoutMs}ms and was aborted.`
        }
        if (res?.error) return `error: child session failed: ${String(res.error)}`
        const text = extractAssistantText(res?.data?.parts)
        if (!text) return "error: child session produced no output."
        return text
      } finally {
        inFlightSync -= 1
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
      if (res?.error) return `error: failed to read task ${args.task_id}: ${String(res.error)}`
      const messages = res?.data ?? []
      const assistant = messages.filter((m) => m.info?.role === "assistant")
      if (assistant.length === 0) return `pending: task ${args.task_id} has not produced output yet.`
      for (let i = assistant.length - 1; i >= 0; i--) {
        const text = extractAssistantText(assistant[i].parts)
        if (text) return text
      }
      return `pending: task ${args.task_id} is running (no assistant text yet).`
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
  const footnoteAgents = loadFootnoteAgents(projectDir)

  // Available models, populated fire-and-forget for best-effort category
  // routing (AC5-ERR). NEVER await a client.* SDK call in plugin init: plugins
  // load inside opencode's bootstrap, which serves no request until every
  // plugin returns, so an awaited provider.list() reenters a server that cannot
  // answer yet and deadlocks startup. The set fills in place once the response
  // lands; a task() firing before then degrades to the default model.
  const available = new Set<string>()
  ;(input.client as unknown as {
    provider: { list(): Promise<{ data?: Array<{ id: string; models?: Record<string, unknown> }> }> }
  }).provider
    .list()
    .then((providers) => collectModels(providers, available))
    .catch(() => {}) // swallow: an unhandled rejection in plugin scope can crash the host

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
    availableModels: () => available,
  })
  const taskResultTool = createTaskResultTool({ client })

  return {
    async config(config: Record<string, unknown>) {
      const agent = (config.agent ?? {}) as Record<string, unknown>
      for (const [name, def] of Object.entries(footnoteAgents)) {
        if (!(name in agent)) agent[name] = def
      }
      config.agent = agent
    },
    async "experimental.chat.system.transform"(_input: unknown, output: { system: string[] }) {
      output.system.unshift(orchestratorPrompt)
    },
    tool: {
      task: taskTool,
      task_result: taskResultTool,
    },
  }
}

export default { id: "fno", server: plugin }
