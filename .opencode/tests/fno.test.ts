import { test, expect } from "bun:test"
import {
  inferCategory,
  parseFrontmatter,
  toOpencodeAgent,
  extractAssistantText,
  resolveModel,
  loadFootnoteAgents,
  createTaskTool,
  isActivated,
} from "../plugin/fno.ts"

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
