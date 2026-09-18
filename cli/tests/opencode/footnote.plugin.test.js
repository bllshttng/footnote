// Behavioral tests for the OpenCode native stop-hook bridge plugin.
// Run: bun test cli/tests/opencode/footnote.plugin.test.js
//
// Stubs the opencode SDK `client` and the shell `$` so the idle-handler
// branches are asserted without a live opencode server. The shell fake
// dispatches on the command it sees: `state path target-state` (the
// space-resolved manifest verb), `loop-check`, and `distress-scan`.

import { describe, test, expect } from "bun:test"
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs"
import { join } from "node:path"
import { tmpdir } from "node:os"
import { FootnotePlugin } from "../../src/fno/setup/assets/opencode/footnote.js"

// The manifest carries footnote's OWN session_id namespace (timestamp-PID-random),
// deliberately DIFFERENT from OpenCode's event sessionID below.
const FNO_SESSION_ID = "20260626T214709Z-12237-a9e3c2"
function makeProject({ footnote = true } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "fno-oc-"))
  mkdirSync(join(dir, ".fno"), { recursive: true })
  if (footnote) {
    writeFileSync(
      join(dir, ".fno", "target-state.md"),
      `session_id: "${FNO_SESSION_ID}"\nplan_path: "x"\n`,
    )
  }
  return dir
}

// Fake shell: a tagged-template that dispatches on the command it sees and
// records every call. `state path target-state` prints nothing (the legacy
// in-repo manifest is the fallback under test); `loop-check` replays the
// canned gate decision; `distress-scan` is recorded and silent.
function fakeShell(map = {}) {
  const calls = []
  const reply = (t) => ({
    quiet() {
      return this
    },
    text: () => (map.reject ? Promise.reject(new Error("boom")) : Promise.resolve(t)),
  })
  const $ = (strings, ...values) => {
    const cmd = strings.reduce((a, s, i) => a + s + (values[i] ?? ""), "")
    calls.push(cmd)
    if (cmd.includes(" state path target-state")) return reply(map.statePath ?? "")
    if (cmd.includes(" loop-check ")) return reply(map.loopCheck ?? "{}")
    if (cmd.includes(" distress-scan ")) return reply("")
    return reply("")
  }
  return { shell: $, calls }
}

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

const OC_SESSION_ID = "ses_7a1b2c3d_opencode"
const idleEvent = { type: "session.idle", properties: { sessionID: OC_SESSION_ID } }
const assistantMsg = (text) => ({ info: { role: "assistant" }, parts: [{ type: "text", text }] })

describe("opencode native stop-hook bridge", () => {
  test("AC1-HP: owner idle + block decision -> re-drives the session with the gate's continuation", async () => {
    const dir = makeProject()
    const client = fakeClient([assistantMsg("working on it, no promise yet")])
    const { shell, calls } = fakeShell({
      loopCheck: JSON.stringify({
        decision: "block",
        termination_reason: null,
        continuation: "/target --resume",
      }),
    })
    const hooks = await FootnotePlugin({ directory: dir, client, $: shell })
    await hooks.event({ event: idleEvent })
    expect(client.prompts.length).toBe(1)
    expect(client.prompts[0].path.id).toBe(OC_SESSION_ID)
    expect(client.prompts[0].body.parts[0].text).toBe("/target --resume")
    // The gate is told which harness session asked.
    const gateCall = calls.find((c) => c.includes(" loop-check "))
    expect(gateCall).toContain("--harness opencode")
    expect(gateCall).toContain(`--harness-session ${OC_SESSION_ID}`)
    expect(gateCall).toContain("--state ")
    rmSync(dir, { recursive: true, force: true })
  })

  test("manifest verb path is used when it prints an existing file; legacy path is the fallback", async () => {
    // (a) verb prints an existing file -> that path is the gate's --state.
    const dirA = makeProject()
    const existing = join(dirA, "space-manifest.md")
    writeFileSync(existing, `session_id: "${FNO_SESSION_ID}"\n`)
    const clientA = fakeClient([assistantMsg("x")])
    const a = fakeShell({
      statePath: existing,
      loopCheck: JSON.stringify({ decision: "allow", termination_reason: "DonePRGreen" }),
    })
    const hooksA = await FootnotePlugin({ directory: dirA, client: clientA, $: a.shell })
    await hooksA.event({ event: idleEvent })
    const gateA = a.calls.find((c) => c.includes(" loop-check "))
    expect(gateA).toContain(`--state ${existing} `)
    rmSync(dirA, { recursive: true, force: true })

    // (b) verb prints nothing -> the legacy in-repo path is the fallback.
    const dirB = makeProject()
    const clientB = fakeClient([assistantMsg("x")])
    const b = fakeShell({
      statePath: "",
      loopCheck: JSON.stringify({ decision: "allow", termination_reason: "DonePRGreen" }),
    })
    const hooksB = await FootnotePlugin({ directory: dirB, client: clientB, $: b.shell })
    await hooksB.event({ event: idleEvent })
    const gateB = b.calls.find((c) => c.includes(" loop-check "))
    expect(gateB).toContain(`--state ${join(dirB, ".fno", "target-state.md")} `)
    rmSync(dirB, { recursive: true, force: true })
  })

  test("AC1-UI: terminal decision -> no re-drive (loop-check already emitted termination)", async () => {
    const dir = makeProject()
    const client = fakeClient([assistantMsg("<promise>MISSION COMPLETE: done</promise>")])
    const { shell, calls } = fakeShell({
      loopCheck: JSON.stringify({ decision: "allow", termination_reason: "DonePRGreen" }),
    })
    const hooks = await FootnotePlugin({ directory: dir, client, $: shell })
    await hooks.event({ event: idleEvent })
    expect(client.prompts.length).toBe(0)
    expect(calls.find((c) => c.includes(" distress-scan "))).toBeUndefined()
    rmSync(dir, { recursive: true, force: true })
  })

  test("AC1-ERR: loop-check substrate failure -> no re-drive, no fabricated termination", async () => {
    const dir = makeProject()
    const client = fakeClient([assistantMsg("no promise")])
    const { shell } = fakeShell({ loopCheck: "{}", reject: true })
    const hooks = await FootnotePlugin({ directory: dir, client, $: shell })
    await hooks.event({ event: idleEvent })
    expect(client.prompts.length).toBe(0)
    rmSync(dir, { recursive: true, force: true })
  })

  test("AC1-ERR/AC2-ERR: gate refusal -> distress scan only, no prompt, no termination", async () => {
    const dir = makeProject() // manifest exists; the session is just not its owner
    const client = fakeClient([assistantMsg("hello from ses_unrelated")])
    const { shell, calls } = fakeShell({
      loopCheck: JSON.stringify({
        decision: "refuse",
        termination_reason: null,
        reason: "no registry row bound to opencode/ses_unrelated",
      }),
    })
    const hooks = await FootnotePlugin({ directory: dir, client, $: shell })
    await hooks.event({ event: idleEvent })
    expect(client.prompts.length).toBe(0)
    expect(calls.find((c) => c.includes(" distress-scan "))).toBeDefined()
    expect(calls.find((c) => c.includes(" loop-check "))).toContain("--harness-session ses_7a1b2c3d_opencode")
    rmSync(dir, { recursive: true, force: true })
  })

  test("AC1-EDGE: no manifest anywhere -> disposition ask, refusal, distress scan, no prompt", async () => {
    const dir = makeProject({ footnote: false })
    const client = fakeClient([assistantMsg("hi")])
    const { shell, calls } = fakeShell({
      loopCheck: JSON.stringify({ decision: "refuse", termination_reason: null, reason: "no registry row" }),
    })
    const hooks = await FootnotePlugin({ directory: dir, client, $: shell })
    await hooks.event({ event: idleEvent })
    expect(client.prompts.length).toBe(0)
    expect(calls.find((c) => c.includes(" loop-check "))).toBeDefined()
    expect(calls.find((c) => c.includes(" distress-scan "))).toBeDefined()
    rmSync(dir, { recursive: true, force: true })
  })

  test("AC3-HP: two sessions idling concurrently each get their own gate", async () => {
    const dir = makeProject()
    const client = fakeClient([assistantMsg("no promise")])
    let release
    const gate = new Promise((r) => (release = r))
    const gateCalls = []
    const reply = (t) => ({
      quiet() {
        return this
      },
      text: () => Promise.resolve(t),
    })
    const $ = (strings, ...values) => {
      const cmd = strings.reduce((a, s, i) => a + s + (values[i] ?? ""), "")
      if (cmd.includes(" state path target-state")) return reply("")
      if (cmd.includes(" loop-check ")) {
        gateCalls.push(cmd)
        return {
          quiet() {
            return this
          },
          text: () => gate.then(() => JSON.stringify({ decision: "allow", termination_reason: "DonePRGreen" })),
        }
      }
      return reply("")
    }
    const hooks = await FootnotePlugin({ directory: dir, client, $ })
    const idleB = { type: "session.idle", properties: { sessionID: "ses_second" } }
    const first = hooks.event({ event: idleEvent })
    await new Promise((r) => setTimeout(r, 5))
    const second = hooks.event({ event: idleB })
    await new Promise((r) => setTimeout(r, 5))
    release()
    await second
    await first
    expect(gateCalls.filter((c) => c.includes("--harness-session ses_second")).length).toBe(1)
    expect(gateCalls.filter((c) => c.includes(`--harness-session ${OC_SESSION_ID}`)).length).toBe(1)
    expect(client.prompts.length).toBe(0)
    rmSync(dir, { recursive: true, force: true })
  })

  test("AC3-ERR: a concurrent idle for a running session becomes one pending recheck", async () => {
    const dir = makeProject()
    const client = fakeClient([assistantMsg("no promise")])
    let release
    const gate = new Promise((r) => (release = r))
    let gateCalls = 0
    const reply = (t) => ({
      quiet() {
        return this
      },
      text: () => Promise.resolve(t)
    })
    const $ = (strings, ...values) => {
      const cmd = strings.reduce((a, s, i) => a + s + (values[i] ?? ""), "")
      if (cmd.includes(" state path target-state")) return reply("")
      if (cmd.includes(" loop-check ")) {
        gateCalls++
        return {
          quiet() {
            return this
          },
          text: () => gate.then(() => JSON.stringify({ decision: "block", termination_reason: null, continuation: "/target --resume" })),
        }
      }
      return reply("")
    }
    const hooks = await FootnotePlugin({ directory: dir, client, $ })
    const first = hooks.event({ event: idleEvent })
    await new Promise((r) => setTimeout(r, 5))
    await hooks.event({ event: idleEvent }) // concurrent same-session idle -> rerun marker
    expect(gateCalls).toBe(1)
    release()
    await first
    // The pending recheck runs once after the running gate finishes.
    await new Promise((r) => setTimeout(r, 10))
    expect(gateCalls).toBe(2)
    expect(client.prompts.length).toBe(2)
    rmSync(dir, { recursive: true, force: true })
  })

  test("session.created injects crown context when the resolver finds a manifest", async () => {
    const dir = makeProject()
    const client = fakeClient([])
    const { shell } = fakeShell({
      statePath: "",
      loopCheck: "{}",
    })
    // The whoami fake must replay a crown line; route it through the map.
    const whoami = fakeShell({ statePath: "", loopCheck: "{}" })
    const $ = (strings, ...values) => {
      const cmd = strings.reduce((a, s, i) => a + s + (values[i] ?? ""), "")
      if (cmd.includes("fno whoami")) {
        return {
          quiet() {
            return this
          },
          text: () => Promise.resolve("crown: opencode:x-4d9b scope=x-4d9b\nregistered: true\n"),
        }
      }
      return whoami.shell(strings, ...values)
    }
    const hooks = await FootnotePlugin({ directory: dir, $, client })
    await hooks.event({ event: { type: "session.created", properties: { sessionID: OC_SESSION_ID } } })
    expect(client.prompts.length).toBe(1)
    expect(client.prompts[0].body.noReply).toBe(true)
    expect(client.prompts[0].body.parts[0].text).toContain("You hold this crown")
    rmSync(dir, { recursive: true, force: true })
  })

  test("ignores non-idle events", async () => {
    const dir = makeProject()
    const client = fakeClient([assistantMsg("x")])
    const { shell, calls } = fakeShell({})
    const hooks = await FootnotePlugin({ directory: dir, client, $: shell })
    await hooks.event({ event: { type: "message.part.updated", properties: {} } })
    expect(calls.length).toBe(0)
    expect(client.prompts.length).toBe(0)
    rmSync(dir, { recursive: true, force: true })
  })
})
