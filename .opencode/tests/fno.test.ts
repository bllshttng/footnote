import { test, expect } from "bun:test"
import { mkdtempSync, mkdirSync, writeFileSync, chmodSync, readFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import fnoPlugin, {
  inferCategory,
  extractAssistantText,
  createTaskTool,
  createTaskResultTool,
  isActivated,
  setupV2,
} from "../plugins/fno.ts"

// Run plugin init with the activation gate forced, restoring the prior
// value. The gate is ON by default now: activated means env unset, false
// means an explicit FNO_OPENCODE=0.
async function initPlugin(input: any, activated: boolean) {
  const prev = process.env.FNO_OPENCODE
  if (activated) delete process.env.FNO_OPENCODE
  else process.env.FNO_OPENCODE = "0"
  try {
    return await (fnoPlugin as any).server(input)
  } finally {
    if (prev === undefined) delete process.env.FNO_OPENCODE
    else process.env.FNO_OPENCODE = prev
  }
}

test("isActivated is on by default and off only when explicitly disabled", () => {
  const saved = process.env.FNO_OPENCODE
  try {
    delete process.env.FNO_OPENCODE
    expect(isActivated()).toBe(true)
    process.env.FNO_OPENCODE = "0"
    expect(isActivated()).toBe(false)
    process.env.FNO_OPENCODE = "false"
    expect(isActivated()).toBe(false)
    process.env.FNO_OPENCODE = "1"
    expect(isActivated()).toBe(true)
  } finally {
    if (saved === undefined) delete process.env.FNO_OPENCODE
    else process.env.FNO_OPENCODE = saved
  }
})


test("inferCategory maps known agents, undefined otherwise", () => {
  expect(inferCategory("fno:archer")).toBe("do")
  expect(inferCategory("explore")).toBe("research")
  expect(inferCategory("oracle")).toBe("think")
  expect(inferCategory("nope")).toBeUndefined()
  expect(inferCategory(undefined)).toBeUndefined()
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


test("plugin is active when FNO_OPENCODE is unset, inert when it is 0 (AC1-EDGE)", async () => {
  let called = false
  const input = {
    client: { provider: { list: () => { called = true; return Promise.resolve({ data: [] }) } } },
    directory: "/nonexistent",
  }
  const hooks = await initPlugin(input, true)
  expect(hooks.tool.task).toBeDefined()
  expect(hooks.tool.task_result).toBeDefined()
  expect(called).toBe(false)
  const inert = await initPlugin(input, false)
  expect(Object.keys(inert)).toEqual([])
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

// ---- V2 setup arm (AC1-*, stub ctx records registrations) ------------------

/** A stub V2 context: every hook registration is recorded, nothing runs. */
function stubV2Ctx(directory: string) {
  const calls: Array<{ kind: string; name: string; handler: (e: any) => any }> = []
  const ctx: any = {
    directory,
    session: {
      hook: (name: string, handler: (e: any) => any) => calls.push({ kind: "session", name, handler }),
    },
    tool: {
      hook: (name: string, handler: (e: any) => any) => calls.push({ kind: "tool", name, handler }),
    },
  }
  return { ctx, calls }
}

/** Set env vars for one test body, restoring the prior values after. */
async function withEnv(vars: Record<string, string | undefined>, body: () => unknown) {
  const saved: Record<string, string | undefined> = {}
  for (const [k, v] of Object.entries(vars)) {
    saved[k] = process.env[k]
    if (v === undefined) delete process.env[k]
    else process.env[k] = v
  }
  try {
    await body()
  } finally {
    for (const [k, v] of Object.entries(saved)) {
      if (v === undefined) delete process.env[k]
      else process.env[k] = v
    }
  }
}

function captureStderr() {
  const lines: string[] = []
  const orig = console.error
  console.error = (...a: unknown[]) => lines.push(a.join(" "))
  return {
    lines,
    restore: () => {
      console.error = orig
    },
  }
}

/** A temp project dir carrying one restriction-free footnote agent. */
function v2ProjectDir(): string {
  const dir = mkdtempSync(join(tmpdir(), "fno-v2-"))
  mkdirSync(join(dir, "agents"))
  writeFileSync(join(dir, "agents", "helper.md"), "---\ndescription: helper\n---\nBody.\n")
  return dir
}


test("setup on a stub V2 ctx registers the context hook and returns a safe cleanup (AC1-PORT)", async () => {
  const dir = v2ProjectDir()
  const { ctx, calls } = stubV2Ctx(dir)
  await withEnv({ FNO_OPENCODE: "1" }, async () => {
    const err = captureStderr()
    let cleanup: () => void
    try {
      cleanup = setupV2(ctx)
    } finally {
      err.restore()
    }
    expect(calls.map((c) => `${c.kind}:${c.name}`).sort()).toEqual(["session:context"])
    expect(() => cleanup()).not.toThrow()
  })
})

test("the V2 context hook pushes the orchestrator prompt as a text part (AC1-PORT)", async () => {
  const dir = v2ProjectDir()
  const { ctx, calls } = stubV2Ctx(dir)
  await withEnv({ FNO_OPENCODE: "1", FNO_AGENTS_BIN: "/nonexistent-fno-agents" }, async () => {
    const err = captureStderr()
    let cleanup: () => void
    try {
      cleanup = setupV2(ctx)
    } finally {
      err.restore()
    }
    const event: any = { system: [], sessionID: "ses_v2" }
    await calls.find((c) => c.name === "context")!.handler(event)
    expect(event.system.length).toBe(1)
    expect(event.system[0].type).toBe("text")
    expect(event.system[0].text).toContain("delivery orchestrator")
    cleanup()
  })
})


test("setup with FNO_OPENCODE=0 registers nothing and returns a safe cleanup (AC1-INERT)", async () => {
  const { ctx, calls } = stubV2Ctx("/nonexistent")
  await withEnv({ FNO_OPENCODE: "0" }, () => {
    const err = captureStderr()
    let cleanup: () => void
    try {
      cleanup = setupV2(ctx)
    } finally {
      err.restore()
    }
    expect(calls).toEqual([])
    expect(() => cleanup()).not.toThrow()
  })
})

test("the V2 arm registers neither delegation tool and says why, once (AC1-DELEGATION)", async () => {
  const dir = v2ProjectDir()
  const { ctx, calls } = stubV2Ctx(dir)
  await withEnv({ FNO_OPENCODE: "1", FNO_AGENTS_BIN: "/nonexistent-fno-agents" }, async () => {
    const err = captureStderr()
    let cleanup: () => void
    try {
      cleanup = setupV2(ctx)
    } finally {
      err.restore()
    }
    cleanup()
    const lines = err.lines.filter((l) => l.includes("delegation"))
    expect(lines.length).toBe(1)
    expect(lines[0]).toContain("live child count")
    expect(calls.find((c) => c.name === "task")).toBeUndefined()
    expect(calls.find((c) => c.name === "task_result")).toBeUndefined()
  })
})

test("the V2 definition is a plain dual export with no V2 package import (AC1-NODEP)", () => {
  const source = readFileSync(join(import.meta.dir, "..", "plugins", "fno.ts"), "utf8")
  expect(source.includes("@opencode/plugin")).toBe(false)
  expect((fnoPlugin as any).id).toBe("fno")
  expect(typeof (fnoPlugin as any).server).toBe("function")
  expect(typeof (fnoPlugin as any).setup).toBe("function")
})


test("both arms work side by side: server keeps its tools, setup keeps its hook (AC4-BOTH)", async () => {
  const dir = v2ProjectDir()
  const { ctx, calls } = stubV2Ctx(dir)
  await withEnv({ FNO_OPENCODE: "1" }, async () => {
    const err = captureStderr()
    let hooks: any
    let cleanup: () => void
    try {
      hooks = await (fnoPlugin as any).server({ client: {}, directory: "/nonexistent" })
      cleanup = setupV2(ctx)
    } finally {
      err.restore()
    }
    expect(hooks.tool.task).toBeDefined()
    expect(hooks.tool.task_result).toBeDefined()
    expect(calls.length).toBe(1)
    cleanup!()
  })
})
