import { test, expect } from "bun:test"
import fnoPlugin, {
  inferCategory,
  parseFrontmatter,
  toOpencodeAgent,
  extractAssistantText,
  loadFootnoteAgents,
  createTaskTool,
  createTaskResultTool,
  isActivated,
  resolvePluginRoot,
  buildHookPayload,
  protectionScriptsFor,
  parseHookDecision,
  runProtections,
} from "../plugins/fno.ts"

// Run plugin init with FNO_OPENCODE forced, restoring the prior value.
async function initPlugin(input: any, activated: boolean) {
  const prev = process.env.FNO_OPENCODE
  if (activated) process.env.FNO_OPENCODE = "1"
  else delete process.env.FNO_OPENCODE
  try {
    return await (fnoPlugin as any).server(input)
  } finally {
    if (prev === undefined) delete process.env.FNO_OPENCODE
    else process.env.FNO_OPENCODE = prev
  }
}

test("isActivated is opt-in (off by default)", () => {
  expect(isActivated({})).toBe(false)
  expect(isActivated({ FNO_OPENCODE: "0" })).toBe(false)
  expect(isActivated({ FNO_OPENCODE: "1" })).toBe(true)
  expect(isActivated({ FNO_OPENCODE: "true" })).toBe(true)
})

// ---- pure helpers --------------------------------------------------------

test("inferCategory maps known agents, undefined otherwise", () => {
  expect(inferCategory("fno:archer")).toBe("do")
  expect(inferCategory("explore")).toBe("research")
  expect(inferCategory("oracle")).toBe("think")
  expect(inferCategory("nope")).toBeUndefined()
  expect(inferCategory(undefined)).toBeUndefined()
})

test("parseFrontmatter reads scalars and inline lists, skips nested, returns body", () => {
  const raw = `---
name: archer
description: "TDD executor"
model: sonnet
tools: ["Read", "Write"]
disallowedTools: ["Task", "WebSearch"]
skills:
  - fno:tdd
---
Body line one.
Body line two.`
  const { data, body } = parseFrontmatter(raw)
  expect(data.name).toBe("archer")
  expect(data.description).toBe("TDD executor")
  expect(data.model).toBe("sonnet")
  expect(data.tools).toEqual(["Read", "Write"]) // inline lists survive
  expect(data.disallowedTools).toEqual(["Task", "WebSearch"])
  expect(data.skills).toBeUndefined() // block/nested lists still skipped
  expect(body).toBe("Body line one.\nBody line two.")
})

test("parseFrontmatter with no frontmatter returns raw body", () => {
  const { data, body } = parseFrontmatter("just text")
  expect(data).toEqual({})
  expect(body).toBe("just text")
})

test("toOpencodeAgent drops bare model names, keeps provider/model (AC6-HP)", () => {
  expect(toOpencodeAgent({ description: "d", model: "sonnet" }, "prompt").ok).toBe(true)
  if (toOpencodeAgent({ description: "d", model: "sonnet" }, "prompt").ok) {
    expect(toOpencodeAgent({ description: "d", model: "sonnet" }, "prompt").def).toEqual({
      mode: "subagent",
      prompt: "prompt",
      description: "d",
    })
  }
  expect(toOpencodeAgent({ model: "anthropic/claude-sonnet-4-5" }, "p").ok).toBe(true)
  const t = toOpencodeAgent({ model: "anthropic/claude-sonnet-4-5" }, "p")
  if (t.ok) expect(t.def.model).toBe("anthropic/claude-sonnet-4-5")
})

test("disallowedTools carries into opencode's disable-only tools record (AC6-HP)", () => {
  const t = toOpencodeAgent(
    { disallowedTools: ["Task", "WebSearch", "Write"] },
    "prompt",
    "fno:reviewer",
  )
  expect(t.ok).toBe(true)
  if (t.ok) expect(t.def.tools).toEqual({ task: false, websearch: false, write: false })
})

test("an allowlist tools field refuses the definition by name, field and value (AC6-ERR)", () => {
  const t = toOpencodeAgent(
    { tools: ["Read", "Grep", "Glob", "Bash"] },
    "prompt",
    "fno:archer",
  )
  expect(t.ok).toBe(false)
  if (!t.ok) {
    expect(t.agent).toBe("fno:archer")
    expect(t.field).toBe("tools")
    expect(t.value).toBe(JSON.stringify(["Read", "Grep", "Glob", "Bash"]))
  }
})

test("extractAssistantText returns completed text only, reasoning never joins (AC5-ERR)", () => {
  expect(
    extractAssistantText([
      { type: "reasoning", text: "thinking" },
      { type: "tool", text: "ignored" },
      { type: "text", text: "answer" },
    ]),
  ).toBe("answer")
  expect(extractAssistantText([{ type: "reasoning", text: "thinking" }])).toBe("")
  expect(extractAssistantText([])).toBe("")
  expect(extractAssistantText(undefined)).toBe("")
  expect(extractAssistantText([{ type: "tool" }])).toBe("")
})

test("loadFootnoteAgents registers restriction-free defs and refuses allowlists by name", () => {
  const { agents, refusals } = loadFootnoteAgents(`${import.meta.dir}/../..`)
  // Restriction-free definitions register.
  expect(agents["fno:architect"]).toBeDefined()
  expect(agents["fno:architect"].mode).toBe("subagent")
  expect(agents["fno:architect"].prompt.length).toBeGreaterThan(0)
  // The repo's allowlist-carrying definitions refuse, naming agent+field.
  const archer = refusals.find((r) => !r.ok && r.agent === "fno:archer")
  expect(archer).toBeDefined()
  if (archer && !archer.ok) expect(archer.field).toBe("tools")
})

test("loadFootnoteAgents on a missing dir returns empty agents and refusals", () => {
  const { agents, refusals } = loadFootnoteAgents("/nonexistent-xyz")
  expect(agents).toEqual({})
  expect(refusals).toEqual([])
})

// ---- task tool (mocked client) -------------------------------------------

function mockClient(overrides: Record<string, any> = {}) {
  return {
    session: {
      create: async () => ({ data: { id: "ses_child" } }),
      get: async () => ({ data: {} }), // no parent -> depth 0
      prompt: async () => ({ data: { parts: [{ type: "text", text: "child result" }] } }),
      promptAsync: async () => ({}),
      messages: async () => ({ data: [] }),
      list: async () => ({ data: [] }),
      abort: async () => ({}),
      ...overrides,
    },
  }
}

const baseDeps = (client: any) => ({
  client,
  directory: "/proj",
  knownAgents: () => new Set(["fno:archer", "explore", "oracle"]),
  availableModels: () => new Set<string>(),
})

const ctx = { sessionID: "ses_root" } as any

test("task sync delegation returns a completed envelope (AC5-HP)", async () => {
  const t = createTaskTool(baseDeps(mockClient()))
  const out = await t.execute({ prompt: "do X", category: "do" } as any, ctx)
  const v = JSON.parse(out as string)
  expect(v.state).toBe("completed")
  expect(v.child_session_id).toBe("ses_child")
  expect(v.result).toBe("child result")
})

test("task rejects when neither category nor subagent_type", async () => {
  const t = createTaskTool(baseDeps(mockClient()))
  const out = await t.execute({ prompt: "x" } as any, ctx)
  expect(out).toContain("requires either")
})

test("task rejects unknown subagent_type, lists available (AC4-ERR)", async () => {
  const t = createTaskTool(baseDeps(mockClient()))
  const out = await t.execute({ prompt: "x", subagent_type: "ghost" } as any, ctx)
  expect(out).toContain('unknown agent "ghost"')
  expect(out).toContain("fno:archer")
})

test("task returns a running envelope on empty child output (AC8-EDGE)", async () => {
  const client = mockClient({ prompt: async () => ({ data: { parts: [] } }) })
  const t = createTaskTool(baseDeps(client))
  const out = await t.execute({ prompt: "x", category: "do" } as any, ctx)
  const v = JSON.parse(out as string)
  expect(v.state).toBe("running")
})

test("task enforces depth limit (AC10-EDGE)", async () => {
  // Chain ses_root -> p1 -> p2 -> p3 (depth 3 == MAX)
  const parents: Record<string, string> = { ses_root: "p1", p1: "p2", p2: "p3" }
  const client = mockClient({
    get: async (o: any) => ({ data: { parentID: parents[o.path.id] } }),
  })
  const t = createTaskTool(baseDeps(client))
  const out = await t.execute({ prompt: "x", category: "do" } as any, ctx)
  expect(out).toContain("depth limit")
})

test("task background returns a task_id (AC3-HP)", async () => {
  const t = createTaskTool(baseDeps(mockClient()))
  const out = await t.execute(
    { prompt: "x", subagent_type: "explore", run_in_background: true } as any,
    ctx,
  )
  expect(out).toContain("task_id: ses_child")
})

test("task surfaces child-session creation failure", async () => {
  const client = mockClient({ create: async () => ({ error: "boom" }) })
  const t = createTaskTool(baseDeps(client))
  const out = await t.execute({ prompt: "x", category: "do" } as any, ctx)
  expect(out).toContain("failed to create child session")
})

test("task times out and aborts, returning an aborted envelope (AC6-FR, AC5-*)", async () => {
  let aborted = false
  const client = mockClient({
    prompt: () => new Promise(() => {}), // never resolves
    abort: async () => {
      aborted = true
      return {}
    },
  })
  const t = createTaskTool({ ...baseDeps(client), timeoutMs: 20 })
  const out = await t.execute({ prompt: "x", category: "do" } as any, ctx)
  const v = JSON.parse(out as string)
  expect(v.state).toBe("aborted")
  expect(v.child_session_id).toBe("ses_child")
  expect(aborted).toBe(true)
})

// ---- plugin init (deadlock regression, x-c36b) ---------------------------

// The bug: init awaited provider.list(), which reenters the still-bootstrapping
// server and never settles -> permanent hang. These call plugin init directly;
// the module import cannot catch the loader-path hang, so US1 also has a LIVE
// opencode-run check (see the plan). Here we pin the mechanism.

test("plugin init issues no provider reads at all (AC1-FR)", async () => {
  let calls = 0
  const input = {
    client: { provider: { list: () => { calls++; return new Promise(() => {}) } } },
    directory: "/nonexistent",
  }
  // Category routing rode a fire-and-forget provider.list() once; the empty
  // router is gone, so init touches no provider registry and can never wedge
  // bootstrap on it.
  const hooks = await initPlugin(input, true)
  expect(hooks.tool.task).toBeDefined()
  expect(hooks.tool.task_result).toBeDefined()
  expect(calls).toBe(0)
})

test("plugin init survives a client that rejects every provider read (AC1-ERR)", async () => {
  let unhandled = false
  const onUnhandled = () => {
    unhandled = true
  }
  process.on("unhandledRejection", onUnhandled)
  try {
    const input = {
      client: { provider: { list: () => Promise.reject(new Error("boom")) } },
      directory: "/nonexistent",
    }
    const hooks = await initPlugin(input, true)
    expect(hooks.tool.task).toBeDefined()
    await new Promise((r) => setTimeout(r, 10))
    expect(unhandled).toBe(false)
  } finally {
    process.off("unhandledRejection", onUnhandled)
  }
})

test("plugin is inert when FNO_OPENCODE unset — returns {} and never fetches (AC1-EDGE)", async () => {
  let called = false
  const input = {
    client: { provider: { list: () => { called = true; return Promise.resolve({ data: [] }) } } },
    directory: "/nonexistent",
  }
  const hooks = await initPlugin(input, false)
  expect(hooks).toEqual({})
  expect(called).toBe(false)
})

test("plugin init tolerates a malformed client (provider missing) — no sync crash (AC1-ERR)", async () => {
  // `.provider.list()` throws a synchronous TypeError; init must not crash
  // bootstrap (the former try/catch guarded this; the fire-and-forget refactor
  // must keep it).
  const hooks = await initPlugin({ client: {}, directory: "/nonexistent" }, true)
  expect(hooks.tool.task).toBeDefined()
})

test("plugin init stays inert toward the provider registry when activated", async () => {
  let calls = 0
  const input = {
    client: {
      provider: {
        list: async () => {
          calls++
          return { data: { all: [{ id: "anthropic", models: { "claude-haiku-4-5": {} } }] } }
        },
      },
    },
    directory: "/nonexistent",
  }
  await initPlugin(input, true)
  await new Promise((r) => setTimeout(r, 10))
  expect(calls).toBe(0)
})

test("five concurrent synchronous delegations all admit at zero children (AC4-HP)", async () => {
  const t = createTaskTool(baseDeps(mockClient()))
  const outs = await Promise.all(
    Array.from({ length: 5 }, () => t.execute({ prompt: "x", category: "do" } as any, ctx)),
  )
  for (const out of outs) {
    expect(JSON.parse(out as string).state).toBe("completed")
  }
})

test("a sixth delegation at the cap is refused by name (AC4-ERR)", async () => {
  const client = mockClient({
    list: async () => ({
      data: Array.from({ length: 5 }, (_, i) => ({ id: `ses_live_${i}`, parentID: "ses_root" })),
    }),
  })
  const t = createTaskTool(baseDeps(client))
  const out = await t.execute({ prompt: "x", category: "do" } as any, ctx)
  expect(out).toContain("concurrency limit reached")
})

test("an unreadable live-child read refuses as capacity unknown (AC4-EDGE)", async () => {
  const client = mockClient({ list: async () => ({ error: "read failed" }) })
  const t = createTaskTool(baseDeps(client))
  const out = await t.execute({ prompt: "x", category: "do" } as any, ctx)
  expect(out).toContain("capacity unknown")
})

test("task_result returns a completed readback and releases the slot (AC5-HP)", async () => {
  const resultTool = createTaskResultTool({
    client: mockClient({
      messages: async () => ({
        data: [
          {
            info: {
              role: "assistant",
              time: { created: 1, completed: 2 },
              providerID: "zai",
              modelID: "glm-5.3-flash",
            },
            parts: [{ type: "text", text: "done" }],
          },
        ],
      }),
    }),
  })
  const raw = await resultTool.execute({ task_id: "ses_bg" } as any, ctx)
  const v = JSON.parse(raw as string)
  expect(v.state).toBe("completed")
  expect(v.provider_id).toBe("zai")
  expect(v.model_id).toBe("glm-5.3-flash")
  expect(v.result).toBe("done")
})

test("task_result returns running, never reasoning-as-result (AC5-ERR)", async () => {
  const resultTool = createTaskResultTool({
    client: mockClient({
      messages: async () => ({
        data: [
          {
            info: { role: "assistant", time: { created: 1 } },
            parts: [{ type: "reasoning", text: "still thinking" }],
          },
        ],
      }),
    }),
  })
  const raw = await resultTool.execute({ task_id: "ses_bg" } as any, ctx)
  const v = JSON.parse(raw as string)
  expect(v.state).toBe("running")
  expect(v.result).toBeUndefined()
  expect(String(raw)).not.toContain("still thinking")
})

test("task_result maps an error to failed and an abort to aborted (AC5-EDGE)", async () => {
  const abortedMsg = {
    info: {
      role: "assistant",
      time: { created: 1, completed: 2 },
      error: { name: "MessageAbortedError", data: { message: "stopped" } },
    },
    parts: [{ type: "text", text: "partial" }],
  }
  const failedMsg = {
    info: {
      role: "assistant",
      time: { created: 1, completed: 2 },
      error: { name: "UnknownError" },
    },
    parts: [{ type: "text", text: "boom" }],
  }
  const t1 = createTaskResultTool({
    client: mockClient({ messages: async () => ({ data: [abortedMsg] }) }),
  })
  const v1 = JSON.parse((await t1.execute({ task_id: "ses_a" } as any, ctx)) as string)
  expect(v1.state).toBe("aborted")
  const t2 = createTaskResultTool({
    client: mockClient({ messages: async () => ({ data: [failedMsg] }) }),
  })
  const v2 = JSON.parse((await t2.execute({ task_id: "ses_f" } as any, ctx)) as string)
  expect(v2.state).toBe("failed")
})

test("task_result on a child with no assistant message is pending (AC5-*)", async () => {
  const t = createTaskResultTool({ client: mockClient() })
  const v = JSON.parse((await t.execute({ task_id: "ses_p" } as any, ctx)) as string)
  expect(v.state).toBe("pending")
  expect(v.result).toBeUndefined()
})

// ---- Change 7: policy outcomes on the V1 seams (AC8-*) --------------------

test("the payload the seam builds is the claude shape the scripts already read (AC8-HP)", () => {
  const payload = buildHookPayload("Bash", "ses_x", { command: "rg -uu x" }, "/proj")
  expect(payload.tool_name).toBe("Bash")
  expect(payload.session_id).toBe("ses_x")
  expect(payload.cwd).toBe("/proj")
  expect(payload.hook_event_name).toBe("PreToolUse")
  expect((payload.tool_input as any).command).toBe("rg -uu x")
})

test("tool matching mirrors the claude matchers (AC8-HP)", () => {
  expect(protectionScriptsFor("bash").map((e) => e.script)).toEqual([
    "graph-write-protect.sh",
    "git-protection.py",
    "pipe-guard.sh",
    "recursive-grep-guard.py",
  ])
  expect(protectionScriptsFor("write").map((e) => e.script)).toContain("plan-location-guard.sh")
  expect(protectionScriptsFor("webfetch")).toEqual([])
})

test("parseHookDecision honors deny and reads allow (AC8-HP)", () => {
  const deny = parseHookDecision(
    '{"decision":"block","reason":"no","hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"forbidden surface"}}',
  )
  expect(deny.deny).toBe(true)
  expect(deny.reason).toBe("forbidden surface")
  expect(parseHookDecision("{}").deny).toBe(false)
  expect(parseHookDecision("").deny).toBe(false)
})

test("runProtections denies on the script's decision and throws at the seam (AC8-HP)", async () => {
  const seen: string[] = []
  const out = await runProtections("Write", "ses_w", { file_path: "/x" }, "/proj", async (script, payload) => {
    seen.push(script)
    return JSON.stringify({
      hookSpecificOutput: { hookEventName: "PreToolUse", permissionDecision: "deny", permissionDecisionReason: "protected manifest" },
    })
  })
  expect(out.denied).toBe(true)
  expect(out.reason).toBe("protected manifest")
  expect(seen.length).toBeGreaterThan(0)
})

test("a missing/deciding-nothing script reports once and allows - fail-open (AC8-ERR)", async () => {
  const errors: string[] = []
  const orig = console.error
  console.error = (...a: unknown[]) => errors.push(a.join(" "))
  try {
    const out = await runProtections("Bash", "ses_b", { command: "ls" }, "/proj", async () => "")
    expect(out.denied).toBe(false)
    expect(errors.some((e) => e.includes("no decision"))).toBe(true)
  } finally {
    console.error = orig
  }
})

test("resolvePluginRoot reads the env chain and the plugin-root file", () => {
  expect(resolvePluginRoot({ FNO_PLUGIN_ROOT: "/p1" })).toBe("/p1")
  expect(resolvePluginRoot({ CLAUDE_PLUGIN_ROOT: "/p2" })).toBe("/p2")
  expect(resolvePluginRoot({})).toBeNull()
})

test("a finished child frees its slot; a running one holds it (review fix)", async () => {
  // Five children exist, but all read terminal: none counts against the cap.
  const terminal = {
    info: { role: "assistant", time: { created: 1, completed: 2 } },
    parts: [{ type: "text", text: "done" }],
  }
  const client = mockClient({
    list: async () => ({
      data: Array.from({ length: 5 }, (_, i) => ({ id: `ses_live_${i}`, parentID: "ses_root" })),
    }),
    messages: async () => ({ data: [terminal] }),
  })
  const t = createTaskTool(baseDeps(client))
  const out = await t.execute({ prompt: "x", category: "do" } as any, ctx)
  expect(JSON.parse(out as string).state).toBe("completed")
  // A child still running (no terminal state) counts against the cap.
  const running = {
    info: { role: "assistant", time: { created: 1 } },
    parts: [],
  }
  const client2 = mockClient({
    list: async () => ({
      data: Array.from({ length: 5 }, (_, i) => ({ id: `ses_live_${i}`, parentID: "ses_root" })),
    }),
    messages: async () => ({ data: [running] }),
  })
  const t2 = createTaskTool(baseDeps(client2))
  const refused = await t2.execute({ prompt: "x", category: "do" } as any, ctx)
  expect(refused).toContain("concurrency limit reached")
})
