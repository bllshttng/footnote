// footnote <-> OpenCode bridge plugin (native first-class stop-hook).
//
// Installed by `fno config setup` into ~/.config/opencode/plugins/footnote.js
// (local-file load path; no npm publish required). Plain JS, zero deps, so
// OpenCode/Bun loads it directly with no `bun install`.
//
// Purpose: make OpenCode a first-class footnote harness - replicate /target's
// in-session stop hook (keep the agent working until the world agrees it's
// done) using OpenCode's plugin surface.
//
// One gate body, two contracts. OpenCode 1.x loads plugins that export a
// function returning a hook object; 2.x loads a plain `{ id, setup }` object
// and drives it through a context with hook seams. Both arms adapt their
// version's I/O into one `io` seam - `readAssistantTexts`, `sendPrompt`,
// `sendSynthetic`, `run` - and hand it to the SAME handler, so the scheduler,
// the transcript synthesis, the gate call and the decision branch exist
// exactly once.
//
// On idle (1.x `session.idle`; 2.x `session.status` with `status.type ===
// "idle"` - the 2.x form is the only one listened to, so one idle can never
// run the gate twice) the handler:
//
//   1. resolves the session's target manifest via `fno-agents state path
//      target-state` (the space-resolved manifest; the legacy in-repo
//      .fno/target-state.md is the fallback when the verb prints nothing),
//   2. reads the turn's assistant text,
//   3. synthesizes a minimal claude-shaped transcript jsonl,
//   4. shells `fno-agents loop-check` (the SAME completion gate claude uses)
//      with the idle event's own session id (`--harness opencode
//      --harness-session <sid>`), so the gate binds the session through the
//      registry before any continuation,
//   5. on a refuse decision the asking session is not the target's bound
//   session: send nothing, emit nothing, run the pre-manifest distress scan
//   and stop. On a non-terminal (block) decision, re-drive the SAME session
//   in-context with the continuation the gate named.
//
// loop-check is the SOLE completion authority (shared with claude, no drift):
// the plugin never decides "done" itself, and never fabricates a termination
// when the gate is unavailable.
//
// If the gate refuses, a plain native OpenCode session is unaffected.

import { readFileSync, writeFileSync, unlinkSync } from "node:fs"
import { join } from "node:path"
import { execFile } from "node:child_process"

// The manifest resolver. `state path target-state` is the space-resolved
// truth (manifests live at ~/.fno/spaces/<space>/worktrees/<name>/); the
// legacy in-repo path is the fallback when the verb prints nothing or the
// printed file does not exist. 5s bound: a wedged verb must never hold an
// idle event.
const MANIFEST_VERB_TIMEOUT_MS = 5000

// The 2.x subprocess bound. The gate verbs are reads, but a wedged read must
// never hold the 2.x event subscription forever.
const SUBPROC_TIMEOUT_MS = 120_000

async function resolveManifest(dir, io) {
  let timer
  try {
    const bin = process.env.FNO_AGENTS_BIN || "fno-agents"
    const run = io
      .run([bin, "state", "path", "target-state"], dir)
      .then((out) => String(out || ""))
      .catch(() => "")
    const out = (await Promise.race([
      run,
      new Promise((resolve) => {
        timer = setTimeout(() => resolve(""), MANIFEST_VERB_TIMEOUT_MS)
      }),
    ])) || ""
    clearTimeout(timer)
    const printed = out.trim()
    if (printed) {
      try {
        readFileSync(printed)
        return printed
      } catch {
        // printed a path that does not exist; fall through to legacy
      }
    }
  } catch {
    // verb unavailable (old fno-agents, no PATH): legacy fallback below
  }
  clearTimeout(timer)
  const legacy = join(dir, ".fno", "target-state.md")
  try {
    readFileSync(legacy)
    return legacy
  } catch {
    return null
  }
}

// Build the minimal transcript loop-check scans. Its detect_intent_full filters
// lines on /message/role == "assistant" AND extract_assistant_text reads
// /message/content - BOTH are required, so each line carries both fields.
function synthesizeTranscript(items) {
  const lines = []
  for (const it of items) {
    if (it?.info?.role !== "assistant") continue
    const parts = Array.isArray(it.parts) ? it.parts : []
    const text = parts
      .filter((p) => p && p.type === "text" && typeof p.text === "string")
      .map((p) => p.text)
      .join("")
    if (!text) continue
    lines.push(JSON.stringify({ message: { role: "assistant", content: text } }))
  }
  return lines.length ? lines.join("\n") + "\n" : ""
}

// 2.x message rows are `{ type, content: [{ type, text }] }`; the synthesizer
// reads `{ info: { role }, parts }`. Assistant-only, like the 1.x reader.
function normalizeV2Rows(rows) {
  const list = Array.isArray(rows) ? rows : []
  return list
    .filter((r) => r && r.type === "assistant")
    .map((r) => ({
      info: { role: "assistant" },
      parts: Array.isArray(r.content) ? r.content : [],
    }))
}

// The one gate body. Both arms adapt their version's I/O into `io` and hand
// it here; neither holds a second copy of anything below (AC2-SHARED).
function makeHandler(io, dir) {
  // Per-session gate scheduler (Change 2): one entry per session id. An idle
  // for an idle session runs; an idle for a RUNNING session marks `rerun` and
  // returns; when a gate finishes and its entry reads `rerun`, it runs once
  // more. Different sessions never block each other, and no idle is dropped.
  // Deliberately NOT authority: the gate's binding refusal (Change 1) is what
  // keeps ownership correct across a plugin reload, so a reload costs a
  // redundant gate run and never a wrong continuation.
  const gates = new Map()

  return async function handle(kind, sid) {
    try {
      if (kind === "created") {
        // Presence via the resolver: a plain native session pays nothing.
        if (sid && (await resolveManifest(dir, io))) {
          try {
            const out = await io.run([process.env.FNO_AGENTS_BIN || "fno-agents", "whoami"], dir)
            const crown = (String(out).match(/^crown:.*$/m) || [])[0]
            if (crown) {
              await io.sendSynthetic(
                sid,
                `${crown}\nYou hold this crown. Before you reach for any CLI verb, Read skills/reign/references/cli-commands.md.`,
              )
            }
          } catch (e) {
            console.error(`[footnote] crown inject failed: ${e}`)
          }
        }
        return
      }
      if (kind !== "idle" || !sid) return

      // Per-session serialize: never run two gates for one session, never
      // drop an idle. (AC3-HP: different sessions run concurrently; AC3-ERR:
      // a concurrent idle for a running session is a pending recheck, taken
      // once after the running gate finishes.)
      if (gates.get(sid) === "running") {
        gates.set(sid, "rerun")
        return
      }
      gates.set(sid, "running")
      try {
        await runGate(sid)
      } finally {
        if (gates.get(sid) === "rerun") {
          gates.set(sid, "running")
          runGate(sid)
            .catch(() => {})
            .finally(() => gates.delete(sid))
        } else {
          gates.delete(sid)
        }
      }
    } catch (e) {
      // Any adapter failure names itself once and leaves the session idle;
      // the hook never throws out (AC2-FAILOPEN).
      console.error(`[footnote] bridge handler failed: ${e}`)
    }

    async function runGate(sid) {
      let decision = null
      let items = []
      // Resolved per fire: manifests appear and disappear as targets start
      // and finish. When nothing resolves, the gate still runs against the
      // legacy candidate path so a crowned session reaches its own evidence
      // path and an unbound session gets its refusal (AC2-*).
      const manifestPath = await resolveManifest(dir, io)
      const stateArg = manifestPath || join(dir, ".fno", "target-state.md")
      const synth = join(dir, ".fno", `.opencode-loopcheck-${sid}.jsonl`)
      try {
        // 1. Read this session's assistant messages.
        try {
          items = await io.readAssistantTexts(sid)
          items = Array.isArray(items) ? items : []
        } catch (e) {
          console.error(`[footnote] readAssistantTexts(${sid}) failed: ${e}; leaving session idle`)
          return
        }

        // 2. Synthesize the transcript loop-check reads.
        try {
          writeFileSync(synth, synthesizeTranscript(items))
        } catch (e) {
          console.error(`[footnote] cannot write synth transcript: ${e}; leaving session idle`)
          return
        }

        // 3. Run the full claude completion gate, bound to THIS session.
        //    The gate refuses a session the registry does not bind to this
        //    target (exit 0, decision "refuse"); the bridge then runs the
        //    pre-manifest distress scan and sends nothing (AC1-ERR, AC2-ERR).
        const bin = process.env.FNO_AGENTS_BIN || "fno-agents"
        try {
          const out = await io.run(
            [
              bin,
              "loop-check",
              "--state",
              stateArg,
              "--transcript",
              synth,
              "--cwd",
              dir,
              "--harness",
              "opencode",
              "--harness-session",
              sid,
            ],
            dir,
          )
          decision = JSON.parse(out)
        } catch (e) {
          console.error(`[footnote] loop-check unavailable/failed: ${e}; not re-driving`)
          return
        }
      } finally {
        try {
          unlinkSync(synth)
        } catch {
          // nothing to clean up / already gone
        }
      }

      if (!decision) return
      // Not this session's target (or an unbound session): distress scan
      // only, no prompt, no termination (AC1-ERR, AC2-ERR).
      if (decision.decision === "refuse") {
        await distressScan(sid, synth, items)
        return
      }
      // Terminal: loop-check already emitted `termination`.
      if (decision.termination_reason) return
      // Non-terminal: re-drive the same session in-context with the
      // continuation the gate named. Fire-and-forget; the next turn's idle
      // runs the gate again.
      if (decision.decision === "block") {
        const continuation =
          typeof decision.continuation === "string" && decision.continuation
            ? decision.continuation
            : "/target --resume"
        try {
          io.sendPrompt(sid, continuation).catch((e) =>
            console.error(`[footnote] re-drive prompt(${sid}) failed: ${e}`),
          )
        } catch (e) {
          console.error(`[footnote] re-drive prompt(${sid}) threw: ${e}`)
        }
      }
    }

    // The distress scan for an unbound session, on the SAME payload shape
    // the shell stop hooks use. Side effect only, best-effort, never throws.
    async function distressScan(sid, synth, items) {
      try {
        if (!items) return
        writeFileSync(synth, synthesizeTranscript(items))
        const bin = process.env.FNO_AGENTS_BIN || "fno-agents"
        await io.run(
          [bin, "distress-scan", "--transcript", synth, "--run", sid, "--harness", "opencode", "--cwd", dir],
          dir,
        )
      } catch (e) {
        console.error(`[footnote] distress-scan skipped (non-fatal): ${e}`)
      } finally {
        try {
          unlinkSync(synth)
        } catch {
          // nothing to clean up / already gone
        }
      }
    }
  }
}

// The 1.x arm: a function returning the event hook object, driven by the
// OpenCode 1.x plugin loader.
async function server({ directory, worktree, client, $ }) {
  const dir = directory || worktree || process.cwd()
  const io = {
    readAssistantTexts: async (sid) => {
      const res = await client.session.messages({ path: { id: sid } })
      return Array.isArray(res?.data) ? res.data : []
    },
    sendPrompt: (sid, text) =>
      client.session.prompt({
        path: { id: sid },
        body: { parts: [{ type: "text", text }] },
      }),
    sendSynthetic: (sid, text) =>
      client.session.prompt({
        path: { id: sid },
        body: { noReply: true, parts: [{ type: "text", text }] },
      }),
    run: async (argv, cwd) => (await $`cd ${cwd} && ${argv}`.quiet().text()) ?? "",
  }
  const handle = makeHandler(io, dir)
  return {
    event: async ({ event }) => {
      if (event?.type === "session.created") return handle("created", event.properties?.sessionID)
      if (event?.type !== "session.idle") return
      return handle("idle", event.properties?.sessionID)
    },
  }
}

// The 2.x arm: one event subscription whose idle form is `session.status`
// with `status.type === "idle"`. The deprecated `session.idle` event is
// deliberately NOT listened to, so one idle can never run the gate twice
// (AC2-EDGE).
function makeV2Io(ctx) {
  return {
    readAssistantTexts: async (sid) => {
      const res = await ctx.session.context({ sessionID: sid })
      const rows = Array.isArray(res) ? res : Array.isArray(res?.data) ? res.data : []
      return normalizeV2Rows(rows)
    },
    sendPrompt: (sid, text) => ctx.session.prompt({ sessionID: sid, text }),
    sendSynthetic: (sid, text) => ctx.session.synthetic({ sessionID: sid, text }),
    run: (argv, cwd) =>
      new Promise((resolve, reject) => {
        execFile(argv[0], argv.slice(1), { cwd, timeout: SUBPROC_TIMEOUT_MS }, (err, stdout) => {
          if (err) reject(err)
          else resolve(String(stdout ?? ""))
        })
      }),
  }
}

async function setup(ctx) {
  const dir = ctx.directory || process.cwd()
  const handle = makeHandler(makeV2Io(ctx), dir)
  const controller = new AbortController()
  ;(async () => {
    try {
      for await (const event of ctx.event.subscribe({ signal: controller.signal })) {
        if (event?.type === "session.created") {
          handle("created", event.data?.sessionID)
        } else if (event?.type === "session.status" && event.data?.status?.type === "idle") {
          handle("idle", event.data?.sessionID)
        }
      }
    } catch (e) {
      if (!controller.signal.aborted) console.error(`[footnote] 2.x event loop failed: ${e}`)
    }
  })()
  return () => controller.abort()
}

export default { id: "footnote", server, setup }
