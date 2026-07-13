import { test, expect } from "bun:test"
import fnoPlugin, {
  inferCategory,
  parseFrontmatter,
  toOpencodeAgent,
  extractAssistantText,
  resolveModel,
  collectModels,
  loadFootnoteAgents,
  createTaskTool,
  isActivated,
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

test("parseFrontmatter reads scalars, ignores arrays/nested, returns body", () => {
  const raw = `---
name: archer
description: "TDD executor"
model: sonnet
tools: ["Read", "Write"]
skills:
  - fno:tdd
---
Body line one.
Body line two.`
  const { data, body } = parseFrontmatter(raw)
  expect(data.name).toBe("archer")
  expect(data.description).toBe("TDD executor")
  expect(data.model).toBe("sonnet")
  expect(data.tools).toBeUndefined() // array skipped
  expect(data.skills).toBeUndefined() // nested skipped
  expect(body).toBe("Body line one.\nBody line two.")
})

test("parseFrontmatter with no frontmatter returns raw body", () => {
  const { data, body } = parseFrontmatter("just text")
  expect(data).toEqual({})
  expect(body).toBe("just text")
})

test("toOpencodeAgent drops bare model names, keeps provider/model", () => {
  expect(toOpencodeAgent({ description: "d", model: "sonnet" }, "prompt")).toEqual({
    mode: "subagent",
    prompt: "prompt",
    description: "d",
  })
  expect(toOpencodeAgent({ model: "anthropic/claude-sonnet-4-5" }, "p")).toEqual({
    mode: "subagent",
    prompt: "p",
    model: "anthropic/claude-sonnet-4-5",
  })
})

test("extractAssistantText joins text/reasoning parts, trims", () => {
  expect(
    extractAssistantText([
      { type: "reasoning", text: "thinking" },
      { type: "tool", text: "ignored" },
      { type: "text", text: "answer" },
    ]),
  ).toBe("thinking\nanswer")
  expect(extractAssistantText([])).toBe("")
  expect(extractAssistantText(undefined)).toBe("")
  expect(extractAssistantText([{ type: "tool" }])).toBe("")
})

test("resolveModel returns model only when available", () => {
  const available = new Set(["anthropic/claude-haiku-4-5"])
  // CATEGORY_MODEL is empty by default -> always undefined
  expect(resolveModel("ship", available)).toBeUndefined()
  expect(resolveModel(undefined, available)).toBeUndefined()
})

test("loadFootnoteAgents reads real agents/ dir and namespaces as fno:*", () => {
  const agents = loadFootnoteAgents(`${import.meta.dir}/../..`)
  expect(agents["fno:archer"]).toBeDefined()
  expect(agents["fno:archer"].mode).toBe("subagent")
  expect(agents["fno:archer"].prompt.length).toBeGreaterThan(0)
  expect(agents["fno:archer"].description).toContain("TDD")
})

test("loadFootnoteAgents on a missing dir returns empty", () => {
  expect(loadFootnoteAgents("/nonexistent-xyz")).toEqual({})
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

test("task sync delegation returns child text (AC2-HP)", async () => {
  const t = createTaskTool(baseDeps(mockClient()))
  const out = await t.execute({ prompt: "do X", category: "do" } as any, ctx)
  expect(out).toBe("child result")
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

test("task errors on empty child output (AC8-EDGE)", async () => {
  const client = mockClient({ prompt: async () => ({ data: { parts: [] } }) })
  const t = createTaskTool(baseDeps(client))
  const out = await t.execute({ prompt: "x", category: "do" } as any, ctx)
  expect(out).toContain("no output")
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

test("task times out and aborts (AC6-FR)", async () => {
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
  expect(out).toContain("timed out")
  expect(aborted).toBe(true)
})

// ---- plugin init (deadlock regression, x-c36b) ---------------------------

// The bug: init awaited provider.list(), which reenters the still-bootstrapping
// server and never settles -> permanent hang. These call plugin init directly;
// the module import cannot catch the loader-path hang, so US1 also has a LIVE
// opencode-run check (see the plan). Here we pin the mechanism.

test("plugin init does not await provider.list — never-settling stub resolves promptly (AC1-FR)", async () => {
  const input = { client: { provider: { list: () => new Promise(() => {}) } }, directory: "/nonexistent" }
  // If init awaited the never-settling promise this line would hang to the
  // test-runner timeout; resolving at all is the regression assertion.
  const hooks = await initPlugin(input, true)
  expect(hooks.tool.task).toBeDefined()
  expect(hooks.tool.task_result).toBeDefined()
})

test("plugin init contains a rejecting provider.list — no unhandled rejection (AC1-ERR)", async () => {
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
    await new Promise((r) => setTimeout(r, 10)) // let the rejected populate settle
    expect(unhandled).toBe(false)
    // empty set -> default-model routing
    expect(resolveModel("do", new Set())).toBeUndefined()
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

test("collectModels folds a provider.list response into the set", () => {
  const into = new Set<string>()
  collectModels(
    {
      data: [
        { id: "anthropic", models: { "claude-haiku-4-5": {}, "claude-opus-4-6": {} } },
        { id: "zai", models: { "glm-5": {} } },
      ],
    },
    into,
  )
  expect([...into].sort()).toEqual([
    "anthropic/claude-haiku-4-5",
    "anthropic/claude-opus-4-6",
    "zai/glm-5",
  ])
  expect(collectModels(undefined, new Set()).size).toBe(0) // missing shape is safe
  expect(collectModels({ data: [{ id: "p" }] }, new Set()).size).toBe(0) // no models key
})

test("plugin init issues the populate fetch exactly once when activated", async () => {
  let calls = 0
  const input = {
    client: {
      provider: {
        list: async () => {
          calls++
          return { data: [{ id: "anthropic", models: { "claude-haiku-4-5": {} } }] }
        },
      },
    },
    directory: "/nonexistent",
  }
  await initPlugin(input, true)
  await new Promise((r) => setTimeout(r, 10)) // let the populate settle
  expect(calls).toBe(1) // single populate per init, no re-fetch
})
