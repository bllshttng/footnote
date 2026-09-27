// Behavioral tests for the OpenCode native stop-hook bridge plugin.
// Run: bun test cli/tests/opencode/footnote.plugin.test.js
//
// The plugin speaks both OpenCode contracts through one handler body. The 1.x
// arm is driven with a stubbed SDK `client` and shell `$`; the 2.x arm is
// driven with a stub context and a stub fno-agents binary on PATH so the
// real node:child_process path is exercised hermetically. The shell fake
// dispatches on the command it sees: `state path target-state` (the
// space-resolved manifest verb), `loop-check`, and `distress-scan`.

import { describe, test, expect } from "bun:test"
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, chmodSync, readFileSync } from "node:fs"
import { join } from "node:path"
import { tmpdir } from "node:os"
import footnote from "../../src/fno/setup/assets/opencode/footnote.js"

const FootnotePlugin = footnote.server
const setupV2 = footnote.setup

async function withEnv(vars, body) {
  const saved = {}
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

async function until(fn, ms = 2000) {
  const end = Date.now() + ms
  while (Date.now() < end) {
    if (fn()) return
    await new Promise((r) => setTimeout(r, 10))
  }
  throw new Error("condition not met in time")
}

// A stub fno-agents binary. `log` (set as FNO_STUB_LOG) records every argv
// line, so tests assert on real subprocess dispatch, never on internals.
function stubBin(script) {
  const dir = mkdtempSync(join(tmpdir(), "fno-bin-"))
  const bin = join(dir, "fno-agents-stub")
  writeFileSync(bin, `#!/bin/sh\n${script}\n`)
  chmodSync(bin, 0o755)
  return bin
}

const GATE_STUB = `echo "ARGS: $*" >> "$FNO_STUB_LOG"
if [ "$1" = "loop-check" ]; then cat "$5" >> "$FNO_STUB_LOG" 2>/dev/null; fi
printf '{"decision":"block","termination_reason":null,"continuation":"/target --resume"}'`

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
// records every call. Array substitutions (the argv seam) are joined with
// spaces, the way Bun Shell interpolates arrays. `state path target-state`
// prints nothing (the legacy in-repo manifest is the fallback under test);
// `loop-check` replays the canned gate decision; `distress-scan` is recorded
// and silent.
function fakeShell(map = {}) {
  const calls = []
  const reply = (t) => ({
    quiet() {
      return this
    },
    text: () => (map.reject ? Promise.reject(new Error("boom")) : Promise.resolve(t)),
  })
  const $ = (strings, ...values) => {
    const cmd = strings.reduce(
      (a, s, i) => a + s + (Array.isArray(values[i]) ? values[i].join(" ") : (values[i] ?? "")),
      "",
    )
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

// ---- the 2.x stub context ---------------------------------------------------

/** A queue-backed async-iterable event stream with real abort semantics: the
 * signal handed to subscribe() is the one the iteration honors, exactly like
 * a live ctx.event.subscribe. */
function eventStream() {
  const controller = new AbortController()
  const queue = []
  let wake = null
  let externalSignal = null
  const signalWake = () => {
    if (wake) {
      const w = wake
      wake = null
      w()
    }
  }
  const iterable = {
    [Symbol.asyncIterator]() {
      return {
        async next() {
          for (;;) {
            if (externalSignal?.aborted || controller.signal.aborted) {
              return { value: undefined, done: true }
            }
            if (queue.length) return { value: queue.shift(), done: false }
            await new Promise((r) => (wake = r))
          }
        },
      }
    },
  }
  return {
    subscribe: (opts) => {
      externalSignal = opts?.signal
      return iterable
    },
    push: (evt) => {
      queue.push(evt)
      signalWake()
    },
    abort: () => {
      controller.abort()
      signalWake()
    },
  }
}

/** The 2.x context stub: records prompt/synthetic calls, replays `rows` from
 * session.context in the 2.x row shape. */
function stubCtx(rows, opts = {}) {
  const prompts = []
  const synthetics = []
  const ctx = {
    directory: opts.directory,
    session: {
      async context(o) {
        if (opts.contextError) throw new Error("context read failed")
        return rows
      },
      async prompt(o) {
        prompts.push(o)
        return {}
      },
      async synthetic(o) {
        synthetics.push(o)
        return {}
      },
    },
  }
  return { ctx, prompts, synthetics }
}

const V2_IDLE = (sid) => ({
  type: "session.status",
  data: { sessionID: sid, status: { type: "idle" } },
})
const v2AssistantRow = (text) => ({ type: "assistant", content: [{ type: "text", text }] })

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
      const cmd = strings.reduce(
        (a, s, i) => a + s + (Array.isArray(values[i]) ? values[i].join(" ") : (values[i] ?? "")),
        "",
      )
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
      const cmd = strings.reduce(
        (a, s, i) => a + s + (Array.isArray(values[i]) ? values[i].join(" ") : (values[i] ?? "")),
        "",
      )
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
      const cmd = strings.reduce(
        (a, s, i) => a + s + (Array.isArray(values[i]) ? values[i].join(" ") : (values[i] ?? "")),
        "",
      )
      if (cmd.includes(" whoami")) {
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

// ---- the 2.x arm -----------------------------------------------------------

describe("opencode 2 setup arm", () => {
  test(
    "AC2-PORT: a session.status idle reads context rows, runs loop-check through child_process and re-drives via ctx.session.prompt",
    async () => {
      const dir = makeProject()
      const log = join(dir, "stub.log")
      const bin = stubBin(GATE_STUB)
      const stream = eventStream()
      const { ctx, prompts } = stubCtx([v2AssistantRow("hello from the V2 row")], { directory: dir })
      ctx.event = { subscribe: stream.subscribe }
      await withEnv({ FNO_AGENTS_BIN: bin, FNO_STUB_LOG: log }, async () => {
        const cleanup = await setupV2(ctx)
        stream.push(V2_IDLE("ses_v2"))
        await until(() => prompts.length > 0)
        cleanup()
        expect(prompts[0].sessionID).toBe("ses_v2")
        expect(prompts[0].text).toBe("/target --resume")
        const logText = readFileSync(log, "utf8")
        expect(logText).toContain("loop-check")
        expect(logText).toContain("--harness opencode")
        expect(logText).toContain("--harness-session ses_v2")
        // The transcript the gate read was synthesized from the V2 rows.
        expect(logText).toContain("hello from the V2 row")
      })
      rmSync(dir, { recursive: true, force: true })
    },
  )

  test("AC2-EDGE: a deprecated session.idle event and non-idle status never run the gate", async () => {
    const dir = makeProject({ footnote: true })
    const log = join(dir, "stub.log")
    const bin = stubBin('echo "ARGS: $*" >> "$FNO_STUB_LOG"')
    const stream = eventStream()
    const { ctx, prompts } = stubCtx([v2AssistantRow("x")], { directory: dir })
    ctx.event = { subscribe: stream.subscribe }
    await withEnv({ FNO_AGENTS_BIN: bin, FNO_STUB_LOG: log }, async () => {
      const cleanup = await setupV2(ctx)
      stream.push({ type: "session.idle", data: { sessionID: "ses_v2" } })
      stream.push({ type: "session.status", data: { sessionID: "ses_v2", status: { type: "busy" } } })
      stream.push({ type: "message.part.updated", data: { sessionID: "ses_v2" } })
      await new Promise((r) => setTimeout(r, 80))
      cleanup()
      expect(prompts.length).toBe(0)
      expect(() => readFileSync(log)).toThrow() // no subprocess ran at all
    })
    rmSync(dir, { recursive: true, force: true })
  })

  test("AC2-CROWN: a crowned Footnote session gets its crown line via ctx.session.synthetic; an uncrowned one gets no call", async () => {
    // (a) crowned
    const dirA = makeProject()
    const binA = stubBin(`if [ "$1" = "whoami" ]; then printf 'crown: opencode:t scope=t\\nregistered: true\\n'; fi`)
    const streamA = eventStream()
    const a = stubCtx([], { directory: dirA })
    a.ctx.event = { subscribe: streamA.subscribe }
    await withEnv({ FNO_AGENTS_BIN: binA }, async () => {
      const cleanup = await setupV2(a.ctx)
      streamA.push({ type: "session.created", data: { sessionID: "ses_v2c" } })
      await until(() => a.synthetics.length > 0)
      cleanup()
      expect(a.synthetics[0].sessionID).toBe("ses_v2c")
      expect(a.synthetics[0].text).toContain("crown: opencode:t")
      expect(a.synthetics[0].text).toContain("You hold this crown")
      expect(a.prompts.length).toBe(0)
    })
    rmSync(dirA, { recursive: true, force: true })

    // (b) uncrowned: whoami prints no crown line -> no send at all
    const dirB = makeProject()
    const binB = stubBin('')
    const streamB = eventStream()
    const b = stubCtx([], { directory: dirB })
    b.ctx.event = { subscribe: streamB.subscribe }
    await withEnv({ FNO_AGENTS_BIN: binB }, async () => {
      const cleanup = await setupV2(b.ctx)
      streamB.push({ type: "session.created", data: { sessionID: "ses_v2u" } })
      await new Promise((r) => setTimeout(r, 80))
      cleanup()
      expect(b.synthetics.length).toBe(0)
      expect(b.prompts.length).toBe(0)
    })
    rmSync(dirB, { recursive: true, force: true })
  })

  test("AC2-FAILOPEN: an adapter call that throws logs one [footnote] line and re-drives nothing", async () => {
    const dir = makeProject()
    const bin = stubBin(GATE_STUB)
    const stream = eventStream()
    const { ctx, prompts } = stubCtx(null, { directory: dir, contextError: true })
    ctx.event = { subscribe: stream.subscribe }
    const errors = []
    const orig = console.error
    console.error = (...a) => errors.push(a.join(" "))
    await withEnv({ FNO_AGENTS_BIN: bin }, async () => {
      const cleanup = await setupV2(ctx)
      stream.push(V2_IDLE("ses_fail"))
      await new Promise((r) => setTimeout(r, 80))
      cleanup()
      console.error = orig
      expect(prompts.length).toBe(0) // no re-drive, no fabricated termination
      expect(errors.some((e) => e.includes("[footnote]"))).toBe(true)
      expect(errors.some((e) => e.includes("readAssistantTexts"))).toBe(true)
    })
    console.error = orig
    rmSync(dir, { recursive: true, force: true })
  })

  test("AC2-CLEANUP: the setup return aborts the subscription; later events are ignored", async () => {
    const dir = makeProject()
    const log = join(dir, "stub.log")
    const bin = stubBin('echo "ARGS: $*" >> "$FNO_STUB_LOG"')
    const stream = eventStream()
    const { ctx, prompts } = stubCtx([v2AssistantRow("x")], { directory: dir })
    ctx.event = { subscribe: stream.subscribe }
    await withEnv({ FNO_AGENTS_BIN: bin, FNO_STUB_LOG: log }, async () => {
      const cleanup = await setupV2(ctx)
      cleanup()
      stream.push(V2_IDLE("ses_after"))
      await new Promise((r) => setTimeout(r, 80))
      expect(prompts.length).toBe(0)
      expect(() => readFileSync(log)).toThrow() // the loop exited
    })
    rmSync(dir, { recursive: true, force: true })
  })

  test("AC2-ONEEXPORT: a default export with id, server and setup, and no named export", async () => {
    const m = await import("../../src/fno/setup/assets/opencode/footnote.js")
    expect(Object.keys(m).filter((k) => k !== "default")).toEqual([])
    expect(m.default.id).toBe("footnote")
    expect(typeof m.default.server).toBe("function")
    expect(typeof m.default.setup).toBe("function")
  })

  test("AC2-SHARED: the gate body exists once; both adapters reach it through io", () => {
    const src = readFileSync(
      join(import.meta.dir, "..", "..", "src", "fno", "setup", "assets", "opencode", "footnote.js"),
      "utf8",
    )
    // One scheduler, one transcript synthesizer, one loop-check argv, one
    // decision branch: a second copy in either adapter fails here.
    expect(src.match(/"loop-check"/g).length).toBe(1)
    expect(src.match(/const gates = new Map\(\)/g).length).toBe(1)
    expect(src.match(/function synthesizeTranscript/g).length).toBe(1)
    expect(src.match(/decision === "block"/g).length).toBe(1)
  })
})
