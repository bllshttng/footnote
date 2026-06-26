// footnote <-> OpenCode bridge plugin (native first-class stop-hook).
//
// Installed by `fno setup` into ~/.config/opencode/plugins/footnote.js
// (local-file load path; no npm publish required). Plain JS, zero deps, so
// OpenCode/Bun loads it directly with no `bun install`.
//
// Purpose: make OpenCode a first-class footnote harness - replicate /target's
// in-session stop hook (keep the agent working until the world agrees it's
// done) using OpenCode's plugin surface.
//
// OpenCode has no exit-veto hook, but footnote's stop hook is really
// "observe-when-the-agent-stops, then continue until done". OpenCode's
// `session.idle` event (published by the server at every turn end, incl.
// re-driven turns - see packages/opencode/src/session/status.ts) is that
// observation point. On idle we:
//
//   1. read the turn's assistant text via client.session.messages,
//   2. synthesize a minimal claude-shaped transcript jsonl,
//   3. shell `fno-agents loop-check` (the SAME completion gate claude uses:
//      promise scan + PR-for-HEAD + CI green + bots reviewed + no blocking
//      finding; it emits the `termination` event itself on a terminal allow),
//   4. on a non-terminal (block/continue) decision, re-drive the SAME session
//      in-context via client.session.prompt (preserves history; beats the
//      loop-wrapper's fresh-process relaunch).
//
// This SUPERSEDES x-6007/#47's `message.part.updated` promise-text handler,
// which emitted an UNGATED DonePRGreen termination on the promise text alone.
// loop-check is the SOLE completion authority (shared with claude, no drift):
// the plugin never decides "done" itself, and never fabricates a termination
// when the gate is unavailable.
//
// If there is no footnote session (no .fno/target-state.md in the project),
// the plugin no-ops, so a plain native OpenCode session is unaffected.

import { readFileSync, writeFileSync } from "node:fs"
import { join } from "node:path"

function fnoSessionId(dir) {
  try {
    const txt = readFileSync(join(dir, ".fno", "target-state.md"), "utf8")
    const m = txt.match(/^session_id:\s*"?([^"\s]+)"?/m)
    return m ? m[1] : null
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

export const FootnotePlugin = async ({ directory, worktree, client, $ }) => {
  const dir = directory || worktree || process.cwd()
  // In-flight guard: never run two loop-checks (or overlap a re-drive) at once.
  // ponytail: single boolean - the turn lifecycle + loop-check's NoProgress
  // backstop already bound a stuck session; this just prevents concurrent fires.
  let busy = false

  return {
    event: async ({ event }) => {
      if (event?.type !== "session.idle") return
      const sid = event.properties?.sessionID
      if (!sid) return
      // Only act on footnote sessions; a plain native opencode session no-ops.
      if (!fnoSessionId(dir)) return
      if (busy) return
      busy = true

      let decision = null
      try {
        // 1. Read this session's assistant messages.
        let items = []
        try {
          const res = await client.session.messages({ path: { id: sid } })
          items = res?.data || []
        } catch (e) {
          console.error(`[footnote] session.messages failed: ${e}; leaving session idle`)
          return
        }

        // 2. Synthesize the transcript loop-check reads.
        const synth = join(dir, ".fno", `.opencode-loopcheck-${sid}.jsonl`)
        try {
          writeFileSync(synth, synthesizeTranscript(items))
        } catch (e) {
          console.error(`[footnote] cannot write synth transcript: ${e}; leaving session idle`)
          return
        }

        // 3. Run the full claude completion gate. loop-check exits 0 for both
        //    allow and block; only CLI misuse / a missing binary throws. On any
        //    failure we NEVER re-drive and NEVER fabricate a termination.
        const bin = process.env.FNO_AGENTS_BIN || "fno-agents"
        try {
          const out = await $`cd ${dir} && ${bin} loop-check --state .fno/target-state.md --transcript ${synth} --cwd ${dir}`
            .quiet()
            .text()
          decision = JSON.parse(out)
        } catch (e) {
          console.error(`[footnote] loop-check unavailable/failed: ${e}; not re-driving`)
          return
        }
      } finally {
        // Release before any re-drive so the re-driven turn's session.idle is
        // not dropped by this guard.
        busy = false
      }

      if (!decision) return
      // Terminal: loop-check already emitted `termination` - let the session end
      // and emit nothing extra (no duplicate termination event).
      if (decision.termination_reason) return
      // Non-terminal (the world has not caught up, or no promise yet): re-drive
      // the same session in-context. Fire-and-forget; the next turn's idle runs
      // the gate again. loop-check's NoProgress backstop bounds a stuck loop.
      if (decision.decision === "block") {
        client.session
          .prompt({
            path: { id: sid },
            body: { parts: [{ type: "text", text: "/target --resume" }] },
          })
          .catch((e) => console.error(`[footnote] re-drive prompt failed: ${e}`))
      }
    },
  }
}
