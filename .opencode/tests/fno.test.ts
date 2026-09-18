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
  createTaskResultTool,
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

test("collectModels folds a provider.list response (data.all shape) into the set", () => {
  // The SDK 200 body nests providers under data.all (with default/connected
  // siblings) — NOT directly under data. Iterating data itself throws.
  const into = new Set<string>()
  collectModels(
    {
      data: {
        all: [
          { id: "anthropic", models: { "claude-haiku-4-5": {}, "claude-opus-4-6": {} } },
          { id: "zai", models: { "glm-5": {} } },
        ],
      },
    },
    into,
  )
  expect([...into].sort()).toEqual([
    "anthropic/claude-haiku-4-5",
    "anthropic/claude-opus-4-6",
    "zai/glm-5",
  ])
  expect(collectModels(undefined, new Set()).size).toBe(0) // missing shape is safe
  expect(collectModels({ data: { all: [{ id: "p" }] } }, new Set()).size).toBe(0) // no models key
  expect(collectModels({ data: {} }, new Set()).size).toBe(0) // no all key
  // malformed entries (null provider / missing id) are skipped, not thrown on
  const guarded = collectModels(
    { data: { all: [null as any, { models: { m: {} } } as any, { id: "ok", models: { m: {} } }] } },
    new Set(),
  )
  expect([...guarded]).toEqual(["ok/m"])
})

test("plugin init tolerates a malformed client (provider missing) — no sync crash (AC1-ERR)", async () => {
  // `.provider.list()` throws a synchronous TypeError; init must not crash
  // bootstrap (the former try/catch guarded this; the fire-and-forget refactor
  // must keep it).
  const hooks = await initPlugin({ client: {}, directory: "/nonexistent" }, true)
  expect(hooks.tool.task).toBeDefined()
})

test("plugin init issues the populate fetch exactly once when activated", async () => {
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
  await new Promise((r) => setTimeout(r, 10)) // let the populate settle
  expect(calls).toBe(1) // single populate per init, no re-fetch
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
