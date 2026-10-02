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
import footnote, { resolveHookRoot, FOREIGN_SESSION_MARKERS } from "../../src/fno/setup/assets/opencode/footnote.js"

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
 * session.context in the 2.x row shape, and records every hook registration
 * (`registered`, keyed `domain.name`) so tests can fire the callbacks via
 * `fire(domain, name, event)`. `location` exercises ctx.location.directory;
 * `noShell` drops the shell seam to exercise the missing-seam report. */
function stubCtx(rows, opts = {}) {
  const prompts = []
  const synthetics = []
  const registered = new Map()
  const seam = (domain) => async (name, fn) => {
    registered.set(`${domain}.${name}`, fn)
    return { dispose() {} }
  }
  const ctx = {
    directory: opts.directory,
    location: opts.location,
    // The V2 hook seams: setup() registers nothing without them (a context
    // like opencode 1.18's plugin-authoring one is a no-op).
    tool: { hook: seam("tool") },
    session: {
      hook: seam("session"),
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
    shell: opts.noShell ? undefined : { hook: seam("shell") },
  }
  async function fire(domain, name, event) {
    const fn = registered.get(`${domain}.${name}`)
    if (!fn) throw new Error(`no ${domain}.${name} registered`)
    return fn(event)
  }
  return { ctx, prompts, synthetics, registered, fire }
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
    // Root unresolved: the created path must not run the real plugin's
    // SessionStart scripts under test.
    let hooks
    await withEnv({ FNO_PLUGIN_ROOT: undefined, CLAUDE_PLUGIN_ROOT: undefined, FNO_HOME: dir, HOME: dir }, async () => {
      hooks = await FootnotePlugin({ directory: dir, $, client })
      await hooks.event({ event: { type: "session.created", properties: { sessionID: OC_SESSION_ID } } })
    })
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

  test(
    "AC3-EDGE: one idle turn reported as both session.status and session.idle runs the gate once; the next prompt re-arms it",
    async () => {
      const dir = makeProject()
      const log = join(dir, "stub.log")
      const bin = stubBin(GATE_STUB)
      const stream = eventStream()
      const { ctx, prompts, fire } = stubCtx([v2AssistantRow("x")], { directory: dir })
      ctx.event = { subscribe: stream.subscribe }
      await withEnv(
        {
          FNO_AGENTS_BIN: bin,
          FNO_STUB_LOG: log,
          FNO_PLUGIN_ROOT: undefined,
          CLAUDE_PLUGIN_ROOT: undefined,
          CODEX_PLUGIN_ROOT: undefined,
          FNO_HOME: "/nonexistent-fno-home",
        },
        async () => {
          const cleanup = await setupV2(ctx)
          const gateRuns = () => {
            try {
              return (readFileSync(log, "utf8").match(/loop-check/g) || []).length
            } catch {
              return 0
            }
          }
          // The same idle turn arrives as both forms: the gate runs once.
          stream.push(V2_IDLE("ses_d"))
          await until(() => gateRuns() === 1)
          await new Promise((r) => setTimeout(r, 80)) // let the first gate settle
          stream.push({ type: "session.idle", data: { sessionID: "ses_d" } })
          await new Promise((r) => setTimeout(r, 80))
          expect(gateRuns()).toBe(1)
          // A non-idle status and a non-idle event never gate.
          stream.push({ type: "session.status", data: { sessionID: "ses_d", status: { type: "busy" } } })
          stream.push({ type: "message.part.updated", data: { sessionID: "ses_d" } })
          await new Promise((r) => setTimeout(r, 80))
          expect(gateRuns()).toBe(1)
          // The session's next prompt re-arms the latch: the next idle gates again.
          await fire("session", "prompt", { sessionID: "ses_d", prompt: { text: "next turn" } })
          stream.push(V2_IDLE("ses_d"))
          await until(() => gateRuns() === 2)
          cleanup()
          expect(prompts.length).toBeGreaterThan(0) // block decision re-drove the session
        },
      )
      rmSync(dir, { recursive: true, force: true })
    },
  )

  test(
    "AC1-HP/AC2-ERR: a v2 ctx registers the full hook set; the handler cwd is location.directory; a deny throws; an absent seam is reported",
    async () => {
      const loc = mkdtempSync(join(tmpdir(), "fno-loc-"))
      const dir = makeProject()
      const root = mkdtempSync(join(tmpdir(), "fno-hookroot-"))
      mkdirSync(join(root, "hooks"), { recursive: true })
      writeFileSync(
        join(root, "hooks", "hooks.json"),
        JSON.stringify({
          hooks: {
            PreToolUse: [
              {
                matcher: "Bash",
                hooks: [
                  // Captures the claude-shaped payload (hook cwd = the setup dir)...
                  { type: "command", command: "cat > captured-payload.json" },
                  // ...then denies, after the capture has run.
                  {
                    type: "command",
                    command:
                      "echo '{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"v2 no bash\"}}'",
                  },
                ],
              },
            ],
            UserPromptSubmit: [{ matcher: "", hooks: [{ type: "command", command: "echo 'v2 context line'" }] }],
            PreCompact: [{ matcher: "", hooks: [{ type: "command", command: "echo 'v2 compaction line'" }] }],
          },
        }),
      )
      const closedStream = {
        subscribe: () => {
          const iterable = {
            [Symbol.asyncIterator]() {
              return { next: async () => ({ value: undefined, done: true }) }
            },
          }
          return iterable
        },
      }
      const { ctx, registered, fire } = stubCtx([], { location: { directory: loc } })
      ctx.event = closedStream
      await withEnv({ FNO_PLUGIN_ROOT: root }, async () => {
        const cleanup = await setupV2(ctx)
        expect([...registered.keys()].sort()).toEqual([
          "session.compaction",
          "session.context",
          "session.prompt",
          "shell.create.before",
          "tool.execute.after",
          "tool.execute.before",
        ])
        // Deny: the payload is captured first (cwd = location.directory), then
        // the deny throws, like the 1.x arm.
        await expect(
          fire("tool", "execute.before", { tool: "bash", input: { command: "ls" }, sessionID: "ses_v2t" }),
        ).rejects.toThrow("v2 no bash")
        const payload = JSON.parse(readFileSync(join(loc, "captured-payload.json"), "utf8"))
        expect(payload.cwd).toBe(loc)
        expect(payload.tool_name).toBe("Bash")
        expect(payload.session_id).toBe("ses_v2t")
        // PostToolUse runs the hooks and never throws on an allow.
        await fire("tool", "execute.after", { tool: "bash", input: { command: "ls" }, sessionID: "ses_v2t" })
        // Shell env: foreign markers blanked, the proof pair stamped.
        const shellEvent = { command: "sh", cwd: loc, env: { CLAUDE_CODE_SESSION_ID: "claude-parent" } }
        await fire("shell", "create.before", shellEvent)
        expect(shellEvent.env.CLAUDE_CODE_SESSION_ID).toBe("")
        expect(Number(shellEvent.env.FNO_SESSION_PID)).toBeGreaterThan(0)
        expect(shellEvent.env.FNO_SESSION_HARNESS).toBe("opencode")
        // Prompt queues context; the context hook drains it as one text row.
        await fire("session", "prompt", { sessionID: "ses_q", prompt: { text: "hello" } })
        const systemOut = { system: [] }
        await fire("session", "context", { sessionID: "ses_q", system: systemOut.system })
        expect(systemOut.system).toEqual([{ type: "text", text: "v2 context line" }])
        const systemOut2 = { system: [] }
        await fire("session", "context", { sessionID: "ses_q", system: systemOut2.system })
        expect(systemOut2.system).toEqual([])
        // Compaction pushes PreCompact context rows into the compaction system.
        const compOut = { system: [] }
        await fire("session", "compaction", { sessionID: "ses_q", system: compOut.system })
        expect(compOut.system).toEqual([{ type: "text", text: "v2 compaction line" }])
        cleanup()
      })
      // A context without the shell seam registers the rest and reports the gap.
      const errors = []
      const orig = console.error
      console.error = (...a) => errors.push(a.join(" "))
      try {
        const bare = stubCtx([], { location: { directory: loc }, noShell: true })
        bare.ctx.event = closedStream
        await withEnv({ FNO_PLUGIN_ROOT: root }, async () => {
          const cleanup = await setupV2(bare.ctx)
          cleanup()
        })
        expect(errors.some((e) => e.includes("shell.hook create.before"))).toBe(true)
      } finally {
        console.error = orig
      }
      rmSync(dir, { recursive: true, force: true })
      rmSync(loc, { recursive: true, force: true })
      rmSync(root, { recursive: true, force: true })
    },
  )

  test("AC2-CROWN: a crowned Footnote session gets its crown line via ctx.session.synthetic; an uncrowned one gets no call", async () => {
    // (a) crowned
    const dirA = makeProject()
    const binA = stubBin(`if [ "$1" = "whoami" ]; then printf 'crown: opencode:t scope=t\\nregistered: true\\n'; fi`)
    const streamA = eventStream()
    const a = stubCtx([], { directory: dirA })
    a.ctx.event = { subscribe: streamA.subscribe }
    await withEnv({ FNO_AGENTS_BIN: binA, FNO_BIN: binA }, async () => {
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
    await withEnv({ FNO_AGENTS_BIN: binB, FNO_BIN: binB }, async () => {
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

  test("AC2-ONEEXPORT: a default export with id, server and setup, plus the test seams", async () => {
    const m = await import("../../src/fno/setup/assets/opencode/footnote.js")
    expect(Object.keys(m).filter((k) => k !== "default").sort()).toEqual([
      "FOREIGN_SESSION_MARKERS",
      "resolveHookRoot",
    ])
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

describe("opencode hooks.json host", () => {
  function makeHookRoot() {
    const root = mkdtempSync(join(tmpdir(), "fno-hookroot-"))
    mkdirSync(join(root, "hooks"), { recursive: true })
    writeFileSync(join(root, "hooks", "hooks.json"), JSON.stringify({ hooks: {} }))
    return root
  }

  function makeClient() {
    const logs = []
    return {
      logs,
      app: { log: async ({ body }) => { logs.push(body.message) } },
    }
  }

  test("root resolution: env hint with hooks.json wins; a hint without the file does not", async () => {
    const root = makeHookRoot()
    await withEnv({ FNO_PLUGIN_ROOT: root, FNO_HOME: "/nonexistent-home" }, async () => {
      expect(resolveHookRoot()).toBe(root)
    })
    await withEnv({ FNO_PLUGIN_ROOT: join(root, "empty"), FNO_HOME: "/nonexistent-home" }, async () => {
      expect(resolveHookRoot()).toBe(null)
    })
    rmSync(root, { recursive: true, force: true })
  })

  test("PreToolUse deny throws for the mapped tool; an unmatched tool never runs the script", async () => {
    const root = makeHookRoot()
    writeFileSync(
      join(root, "hooks", "hooks.json"),
      JSON.stringify({
        hooks: {
          PreToolUse: [
            {
              matcher: "Bash",
              hooks: [
                {
                  type: "command",
                  command:
                    "echo '{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"no bash here\"}}'",
                },
              ],
            },
          ],
        },
      }),
    )
    const dir = makeProject()
    await withEnv({ FNO_PLUGIN_ROOT: root }, async () => {
      const hooks = await FootnotePlugin({ directory: dir, client: makeClient(), $: async () => "" })
      await expect(
        hooks["tool.execute.before"]({ tool: "bash", sessionID: "ses_A" }, { args: { command: "ls" } }),
      ).rejects.toThrow("no bash here")
      await hooks["tool.execute.before"]({ tool: "read", sessionID: "ses_A" }, { args: {} })
    })
    rmSync(dir, { recursive: true, force: true })
    rmSync(root, { recursive: true, force: true })
  })

  test("SessionStart text queues per session and drains once into that session's system prompt", async () => {
    const root = makeHookRoot()
    writeFileSync(
      join(root, "hooks", "hooks.json"),
      JSON.stringify({
        hooks: {
          SessionStart: [
            { matcher: "", hooks: [{ type: "command", command: "echo 'session-start context line'" }] },
          ],
        },
      }),
    )
    const dir = makeProject()
    await withEnv({ FNO_PLUGIN_ROOT: root }, async () => {
      const hooks = await FootnotePlugin({ directory: dir, client: makeClient(), $: async () => "" })
      await hooks.event({ event: { type: "session.created", properties: { sessionID: "ses_A" } } })
      const outA = { system: [] }
      await hooks["experimental.chat.system.transform"]({ sessionID: "ses_A" }, outA)
      expect(outA.system).toEqual(["session-start context line"])
      const outB = { system: [] }
      await hooks["experimental.chat.system.transform"]({ sessionID: "ses_B" }, outB)
      expect(outB.system).toEqual([])
      const outA2 = { system: [] }
      await hooks["experimental.chat.system.transform"]({ sessionID: "ses_A" }, outA2)
      expect(outA2.system).toEqual([])
    })
    rmSync(dir, { recursive: true, force: true })
    rmSync(root, { recursive: true, force: true })
  })

  test("shell.env stamps OPENCODE_SESSION_ID and blanks the foreign markers", async () => {
    const dir = makeProject()
    await withEnv({ CLAUDE_CODE_SESSION_ID: "claude-parent", CODEX_THREAD_ID: "thread-1", CLAUDECODE: "1" }, async () => {
      const hooks = await FootnotePlugin({ directory: dir, client: makeClient(), $: async () => "" })
      const output = { env: {} }
      await hooks["shell.env"]({ cwd: dir, sessionID: "ses_A" }, output)
      expect(output.env.OPENCODE_SESSION_ID).toBe("ses_A")
      expect(output.env.CLAUDE_CODE_SESSION_ID).toBe("")
      expect(output.env.CODEX_THREAD_ID).toBe("")
      expect(output.env.CLAUDECODE).toBe("")
      // The launcher-stamped proof pair: alive-pid + known-harness name,
      // the only identity a sandboxed tool shell can prove.
      expect(Number(output.env.FNO_SESSION_PID)).toBeGreaterThan(0)
      expect(output.env.FNO_SESSION_HARNESS).toBe("opencode")
    })
    rmSync(dir, { recursive: true, force: true })
  })

  test("no resolvable root: the tool call proceeds", async () => {
    const dir = makeProject()
    await withEnv(
      { FNO_PLUGIN_ROOT: undefined, CLAUDE_PLUGIN_ROOT: undefined, CODEX_PLUGIN_ROOT: undefined, FNO_HOME: dir, HOME: dir },
      async () => {
        expect(resolveHookRoot()).toBe(null)
        const hooks = await FootnotePlugin({ directory: dir, client: makeClient(), $: async () => "" })
        await hooks["tool.execute.before"]({ tool: "bash", sessionID: "ses_A" }, { args: {} })
      },
    )
    rmSync(dir, { recursive: true, force: true })
  })

  test("FOREIGN_SESSION_MARKERS covers harness_identity.py's marker tables", () => {
    const py = readFileSync(
      join(import.meta.dir, "..", "..", "src", "fno", "harness_identity.py"),
      "utf8",
    )
    // The marker tables themselves (HARNESS_SESSION_MARKERS,
    // LEGACY_HARNESS_SESSION_MARKERS, SELF_SET_HARNESS_MARKERS) plus the
    // extra identity table further down; TARGET_SESSION_ID stays out (fno's
    // own plumbing, legitimately inherited by a launched worker).
    const tables = py.split("\n").slice(116, 191).join("\n")
    const names = new Set()
    for (const m of tables.matchAll(/\("([A-Z_]+)", "[a-z]+"\)/g)) names.add(m[1])
    names.delete("OPENCODE_SESSION_ID")
    names.delete("TARGET_SESSION_ID")
    expect(names.size).toBeGreaterThan(4)
    for (const name of names) {
      expect(FOREIGN_SESSION_MARKERS).toContain(name)
    }
  })

  test("a 1.18-style authoring context registers nothing from setup", async () => {
    const dir = makeProject()
    const hooks = await setupV2({ directory: dir })
    expect(typeof hooks).toBe("function")
    rmSync(dir, { recursive: true, force: true })
  })

  test("handled events leave a client.app.log line", async () => {
    const dir = makeProject()
    const client = makeClient()
    await withEnv({ FNO_PLUGIN_ROOT: undefined, FNO_HOME: dir, HOME: dir }, async () => {
      const hooks = await FootnotePlugin({ directory: dir, client, $: async () => "" })
      await hooks.event({ event: { type: "session.created", properties: { sessionID: "ses_A" } } })
      expect(client.logs.some((l) => l.startsWith("session.created"))).toBe(true)
    })
    rmSync(dir, { recursive: true, force: true })
  })
})
