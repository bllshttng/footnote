// Behavioral test for the OpenCode native stop-hook plugin (x-23d6).
// Run: bun test cli/tests/opencode/footnote.plugin.test.js
//
// Stubs the opencode SDK `client` and Bun `$` so we can assert the idle-handler
// branches without a live opencode server.

import { describe, test, expect } from "bun:test"
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs"
import { join } from "node:path"
import { tmpdir } from "node:os"
import { FootnotePlugin } from "../../src/fno/setup/assets/opencode/footnote.js"

function makeProject({ footnote = true } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "fno-oc-"))
  mkdirSync(join(dir, ".fno"), { recursive: true })
  if (footnote) {
    writeFileSync(
      join(dir, ".fno", "target-state.md"),
      'session_id: "sess-123"\nplan_path: "x"\n',
    )
  }
  return dir
}

// Fake Bun `$`: a tagged-template that ignores the command and replays a canned
// loop-check result. `out` is the stdout string; pass `{ throws: true }` to
// simulate a substrate failure / missing binary (non-zero exit -> Bun throws).
function fakeShell(out, { throws = false } = {}) {
  return () => ({
    quiet() {
      return this
    },
    async text() {
      if (throws) throw new Error("loop-check exited 2")
      return out
    },
  })
}

// Fake opencode client recording prompt() calls.
function fakeClient(messages) {
  const prompts = []
  return {
    prompts,
    session: {
      async messages() {
        return { data: messages }
      },
      async prompt(opts) {
        prompts.push(opts)
        return { data: {} }
      },
    },
  }
}

const idleEvent = { type: "session.idle", properties: { sessionID: "sess-123" } }
const assistantMsg = (text) => ({ info: { role: "assistant" }, parts: [{ type: "text", text }] })

describe("opencode native stop-hook plugin", () => {
  test("AC1-HP: idle + continue decision -> re-drives the same session", async () => {
    const dir = makeProject()
    const client = fakeClient([assistantMsg("working on it, no promise yet")])
    const hooks = await FootnotePlugin({
      directory: dir,
      client,
      $: fakeShell(JSON.stringify({ decision: "block", termination_reason: null })),
    })
    await hooks.event({ event: idleEvent })
    expect(client.prompts.length).toBe(1)
    expect(client.prompts[0].path.id).toBe("sess-123")
    expect(client.prompts[0].body.parts[0].text).toBe("/target --resume")
    rmSync(dir, { recursive: true, force: true })
  })

  test("AC1-UI: terminal decision -> no re-drive (loop-check already emitted termination)", async () => {
    const dir = makeProject()
    const client = fakeClient([assistantMsg("<promise>MISSION COMPLETE: done</promise>")])
    const hooks = await FootnotePlugin({
      directory: dir,
      client,
      $: fakeShell(JSON.stringify({ decision: "allow", termination_reason: "DonePRGreen" })),
    })
    await hooks.event({ event: idleEvent })
    expect(client.prompts.length).toBe(0)
    rmSync(dir, { recursive: true, force: true })
  })

  test("AC1-ERR: loop-check substrate failure -> no re-drive, no fabricated termination", async () => {
    const dir = makeProject()
    const client = fakeClient([assistantMsg("no promise")])
    const hooks = await FootnotePlugin({
      directory: dir,
      client,
      $: fakeShell("", { throws: true }),
    })
    await hooks.event({ event: idleEvent })
    expect(client.prompts.length).toBe(0)
    rmSync(dir, { recursive: true, force: true })
  })

  test("AC1-EDGE: no target-state.md (plain native session) -> full no-op", async () => {
    const dir = makeProject({ footnote: false })
    const client = fakeClient([assistantMsg("hi")])
    let shellCalled = false
    const hooks = await FootnotePlugin({
      directory: dir,
      client,
      $: () => {
        shellCalled = true
        return { quiet() { return this }, async text() { return "" } }
      },
    })
    await hooks.event({ event: idleEvent })
    expect(shellCalled).toBe(false)
    expect(client.prompts.length).toBe(0)
    rmSync(dir, { recursive: true, force: true })
  })

  test("AC1-FR: re-drive is non-overlapping - a second idle while busy is ignored", async () => {
    const dir = makeProject()
    const client = fakeClient([assistantMsg("no promise")])
    // Slow shell so the first fire is still in-flight when the second arrives.
    let resolve
    const gate = new Promise((r) => (resolve = r))
    const slowShell = () => ({
      quiet() { return this },
      async text() {
        await gate
        return JSON.stringify({ decision: "block", termination_reason: null })
      },
    })
    const hooks = await FootnotePlugin({ directory: dir, client, $: slowShell })
    const first = hooks.event({ event: idleEvent })
    const second = hooks.event({ event: idleEvent }) // should early-return on busy
    await second
    resolve()
    await first
    expect(client.prompts.length).toBe(1) // only the first fire re-drove
    rmSync(dir, { recursive: true, force: true })
  })

  test("ignores a session.idle whose sessionID != the target session", async () => {
    const dir = makeProject() // manifest session_id is "sess-123"
    const client = fakeClient([assistantMsg("x")])
    let shellCalled = false
    const hooks = await FootnotePlugin({
      directory: dir,
      client,
      $: () => { shellCalled = true; return { quiet() { return this }, async text() { return "" } } },
    })
    await hooks.event({ event: { type: "session.idle", properties: { sessionID: "other-session" } } })
    expect(shellCalled).toBe(false)
    expect(client.prompts.length).toBe(0)
    rmSync(dir, { recursive: true, force: true })
  })

  test("ignores non-idle events", async () => {
    const dir = makeProject()
    const client = fakeClient([assistantMsg("x")])
    let shellCalled = false
    const hooks = await FootnotePlugin({
      directory: dir,
      client,
      $: () => { shellCalled = true; return { quiet() { return this }, async text() { return "" } } },
    })
    await hooks.event({ event: { type: "message.part.updated", properties: {} } })
    expect(shellCalled).toBe(false)
    expect(client.prompts.length).toBe(0)
    rmSync(dir, { recursive: true, force: true })
  })
})
