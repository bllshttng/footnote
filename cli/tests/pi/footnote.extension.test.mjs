// Behavioral tests for the pi bridge extension (transport contract).
// Run: node --test cli/tests/pi/footnote.extension.test.mjs   (node 24)
//
// A fake pi host records `on`, `sendUserMessage` and `appendEntry` calls, and
// FNO_AGENTS_BIN points at a scratch shell stub that answers `state path`,
// `loop-check` and `distress-scan` from env-selected fixtures and appends its
// argv to a log. The stub IS the seam the assertions read: every test pins
// what the extension sends, records, and never does.

import { test } from "node:test"
import assert from "node:assert/strict"
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import footnote from "../../src/fno/setup/assets/pi/footnote.ts"

// The scratch stub binary: logs its argv, then answers from fixtures.
// `state path target-state` prints $STUB_STATE_PATH when set; `loop-check`
// optionally sleeps past a short gate bound, then prints the fixture file.
const STUB = `#!/bin/bash
echo "$@" >> "\$STUB_LOG"
case "\$1" in
  state)
    if [ -n "\$STUB_STATE_PATH" ]; then echo "\$STUB_STATE_PATH"; fi
    ;;
  loop-check)
    if [ -n "\$STUB_LOOPCHECK_SLEEP" ]; then sleep "\$STUB_LOOPCHECK_SLEEP"; fi
    if [ -f "\$STUB_LOOPCHECK_REPLY" ]; then cat "\$STUB_LOOPCHECK_REPLY"; fi
    ;;
esac
exit 0
`

// One isolated world per test: a fixture dir (manifest, reply file, log), the
// env the extension and stub read, and the fake host.
function makeWorld({
  reply = { decision: "allow", termination_reason: null },
  manifest = true,
  env = {},
} = {}) {
  const dir = mkdtempSync(join(tmpdir(), "fno-pi-ext-"))
  const manifestPath = join(dir, "target-state.md")
  if (manifest) {
    writeFileSync(
      manifestPath,
      "---\nsession_id: fno-own\nharness: pi\nharness_session_id: s-1\n---\n",
    )
  }
  const replyPath = join(dir, "reply.json")
  writeFileSync(replyPath, JSON.stringify(reply))
  const logPath = join(dir, "stub.log")
  writeFileSync(logPath, "")
  const stubPath = join(dir, "stub.sh")
  writeFileSync(stubPath, STUB, { mode: 0o755 })

  const saved = new Map()
  const next = {
    FNO_AGENTS_BIN: stubPath,
    STUB_LOG: logPath,
    STUB_STATE_PATH: manifest ? manifestPath : "",
    STUB_LOOPCHECK_REPLY: replyPath,
    ...env,
  }
  for (const [k, v] of Object.entries(next)) {
    saved.set(k, process.env[k])
    process.env[k] = v
  }
  for (const k of ["FNO_AGENT_SESSION_ID", "FNO_PI_GATE_TIMEOUT_MS"]) {
    if (!(k in next)) {
      saved.set(k, process.env[k])
      delete process.env[k]
    }
  }

  const handlers = {}
  const sent = []
  const entries = []
  const notified = []
  const host = {
    on(event, handler) {
      handlers[event] = handler
    },
    sendUserMessage(content, options) {
      sent.push({ content, options })
    },
    appendEntry(type, data) {
      entries.push({ type, data })
    },
  }
  const ctx = {
    sessionManager: {
      getSessionId: () => "s-1",
      buildContextEntries: () => [],
    },
    isIdle: () => true,
    hasUI: true,
    ui: {
      notify(message, level) {
        notified.push({ message, level })
      },
    },
  }
  footnote(host)

  const world = {
    dir,
    handlers,
    sent,
    entries,
    notified,
    log: () => readLog(logPath),
    async settle(eventCtx = ctx) {
      await handlers["agent_settled"]({}, eventCtx)
    },
    cleanup() {
      for (const [k, v] of saved) {
        if (v === undefined) delete process.env[k]
        else process.env[k] = v
      }
      rmSync(dir, { recursive: true, force: true })
    },
  }
  return world
}

function readLog(logPath) {
  // execFile waits for exit, and the stub appends before answering, so by
  // the time a settle resolves every line is on disk.
  try {
    return readFileSync(logPath, "utf8")
  } catch {
    return ""
  }
}

test("AC12-HP: a bound settle runs the gate and sends the gate's continuation", async () => {
  const world = makeWorld({
    reply: {
      decision: "block",
      termination_reason: null,
      continuation: "/skill:target resume",
    },
  })
  try {
    await world.settle()
    const loopLine = world
      .log()
      .split("\n")
      .find((l) => l.startsWith("loop-check "))
    assert.ok(loopLine, "loop-check must be called")
    assert.ok(loopLine.includes("--harness pi"), loopLine)
    assert.ok(loopLine.includes("--harness-session s-1"), loopLine)
    assert.ok(loopLine.includes("--state "), loopLine)
    assert.equal(world.sent.length, 1)
    assert.equal(world.sent[0].content, "/skill:target resume")
    assert.deepEqual(world.sent[0].options, {
      deliverAs: "followUp",
      triggerTurn: true,
      expandPromptTemplates: true,
    })
  } finally {
    world.cleanup()
  }
})

test("AC12-ERR: a refusal sends nothing and records one refused entry", async () => {
  const world = makeWorld({
    reply: {
      decision: "refuse",
      reason: "wrong session: manifest bound to pi/s-1, asked pi/foreign",
    },
    env: { FNO_AGENT_SESSION_ID: "foreign" },
  })
  try {
    await world.settle()
    assert.equal(world.sent.length, 0, "a refused settle must not re-drive")
    const gates = world.entries.filter((e) => e.type === "fno-gate")
    assert.equal(gates.length, 1)
    assert.equal(gates[0].data.state, "refused")
    assert.ok(String(gates[0].data.reason).includes("wrong session"))
  } finally {
    world.cleanup()
  }
})

test("AC13-ERR: a wedged gate is killed, records unavailable, and notifies", async () => {
  const world = makeWorld({
    reply: { decision: "block", termination_reason: null, continuation: "/skill:target resume" },
    env: { FNO_PI_GATE_TIMEOUT_MS: "300", STUB_LOOPCHECK_SLEEP: "5" },
  })
  try {
    await world.settle()
    assert.equal(world.sent.length, 0, "no re-drive past a wedged gate")
    const gates = world.entries.filter((e) => e.type === "fno-gate")
    assert.equal(gates.length, 1)
    assert.equal(gates[0].data.state, "unavailable")
    assert.equal(world.notified.length, 1, "one visible warning when there is a UI")
    assert.ok(world.notified[0].message.includes("gate unavailable"))
  } finally {
    world.cleanup()
  }
})

test("AC13-EDGE: a block with no continuation sends nothing and records why", async () => {
  const world = makeWorld({
    reply: { decision: "block", termination_reason: null },
  })
  try {
    await world.settle()
    assert.equal(world.sent.length, 0)
    const gates = world.entries.filter((e) => e.type === "fno-gate")
    assert.equal(gates.length, 1)
    assert.equal(gates[0].data.state, "unavailable")
    assert.equal(gates[0].data.reason, "gate named no continuation")
  } finally {
    world.cleanup()
  }
})

test("AC14-HP: a native pi session is no longer excluded by the extension itself", async () => {
  const world = makeWorld({
    reply: {
      decision: "block",
      termination_reason: null,
      continuation: "/skill:target resume",
    },
    env: { FNO_AGENT_SESSION_ID: "" },
  })
  try {
    // No FNO_AGENT_SESSION_ID: the extension still gates, because the
    // manifest-bound session id is the binding now.
    await world.settle()
    const loopLine = world
      .log()
      .split("\n")
      .find((l) => l.startsWith("loop-check "))
    assert.ok(loopLine, "the gate runs for a native session too")
    assert.equal(world.sent.length, 1)
  } finally {
    world.cleanup()
  }
})

test("AC11-ERR: discovery with no plugin-root pointer returns empty, prints one line, throws nothing", async () => {
  const home = mkdtempSync(join(tmpdir(), "fno-pi-home-"))
  const saved = process.env.FNO_HOME
  process.env.FNO_HOME = home
  const lines = []
  const err = console.error
  console.error = (m) => lines.push(String(m))
  try {
    const handlers = {}
    footnote({
      on(event, handler) {
        handlers[event] = handler
      },
      sendUserMessage() {},
    })
    const answer = handlers["resources_discover"]({}, {})
    assert.deepEqual(answer, {})
    const footLines = lines.filter((l) => l.includes("[footnote]"))
    assert.equal(footLines.length, 1, `one [footnote] line, got: ${footLines}`)
  } finally {
    console.error = err
    if (saved === undefined) delete process.env.FNO_HOME
    else process.env.FNO_HOME = saved
    rmSync(home, { recursive: true, force: true })
  }
})

test("the manifest read rides the verb, so a directory with no .fno still gates", async () => {
  const world = makeWorld({
    reply: {
      decision: "block",
      termination_reason: null,
      continuation: "/skill:target resume",
    },
  })
  try {
    // The fixture dir carries no .fno at all: the space-resolved path from
    // the verb is the only manifest the extension knows.
    assert.equal(existsSync(join(world.dir, ".fno")), false)
    await world.settle()
    assert.equal(world.sent.length, 1, "the gate still ran")
  } finally {
    world.cleanup()
  }
})
