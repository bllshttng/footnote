// footnote <-> pi bridge extension (native agent_settled stop gate).
//
// Installed by `fno config setup` into ~/.pi/agent/extensions/footnote.ts
// (pi auto-discovers *.ts there; no package publish required). Plain
// TypeScript against node builtins only, so pi loads it with no install step.
//
// Purpose: make pi a first-class footnote harness - replicate /target's
// in-session stop hook (keep the agent working until the world agrees it's
// done) using pi's extension surface. pi ships no shell hook at all; its
// lifecycle boundary is in-process, and the fno stop gate maps onto
// `pi.on("agent_settled")`, which fires when no retry, compaction or
// follow-up is left (docs/extensions.md). On settle we:
//
//   1. read the session's assistant messages via ctx.sessionManager,
//   2. synthesize a minimal claude-shaped transcript jsonl,
//   3. shell `fno-agents loop-check` (the SAME completion gate claude uses:
//      promise scan + PR-for-HEAD + CI green + bots reviewed + no blocking
//      finding; it emits the `termination` event itself on a terminal allow),
//   4. on a non-terminal (block/continue) decision, re-drive the SAME session
//      in-context via pi.sendUserMessage (preserves history; beats a
//      loop-wrapper's fresh-process relaunch).
//
// loop-check is the SOLE completion authority (shared with claude, no drift):
// the extension never decides "done" itself, and never fabricates a
// termination when the gate is unavailable.
//
// If there is no footnote session (no .fno/target-state.md in the project),
// the extension no-ops, so a plain native pi session is unaffected. And the
// gate runs ONLY in a process fno spawned into the loop lane: the keeper sets
// FNO_AGENT_SESSION_ID on its pi child, so a native pi session in a directory
// whose stale manifest survives a finished run is never re-driven.

import { execFile } from "node:child_process"
import { readFileSync, unlinkSync, writeFileSync } from "node:fs"
import { join } from "node:path"

function fnoSessionId(dir: string): string | null {
  try {
    const txt = readFileSync(join(dir, ".fno", "target-state.md"), "utf8")
    const m = txt.match(/^session_id:\s*"?([^"\s]+)"?/m)
    return m ? m[1] : null
  } catch (e: unknown) {
    // A missing manifest is the dominant case (plain native pi session) -
    // stay silent. Any OTHER read error (a present-but-unreadable manifest)
    // is a real footnote session going dark, so surface it once instead of
    // vanishing.
    if ((e as { code?: string })?.code !== "ENOENT") {
      console.error(`[footnote] cannot read target-state.md: ${e}`)
    }
    return null
  }
}

// Build the minimal transcript loop-check scans. Its detect_intent_full
// filters lines on /message/role == "assistant" AND extract_assistant_text
// reads /message/content - BOTH are required, so each line carries both
// fields. pi stores content as typed blocks ({type:"text",text}), so the
// text parts are joined per assistant message.
function synthesizeTranscript(entries: unknown[]): string {
  const lines: string[] = []
  for (const entry of entries) {
    const message = (entry as { message?: { role?: string; content?: unknown } })
      ?.message
    if (message?.role !== "assistant") continue
    const content = Array.isArray(message.content) ? message.content : []
    const text = content
      .filter(
        (p): p is { type: "text"; text: string } =>
          !!p &&
          typeof p === "object" &&
          (p as { type?: string }).type === "text" &&
          typeof (p as { text?: unknown }).text === "string",
      )
      .map((p) => p.text)
      .join("")
    if (!text) continue
    lines.push(JSON.stringify({ message: { role: "assistant", content: text } }))
  }
  return lines.length ? lines.join("\n") + "\n" : ""
}

export default function (pi: {
  on: (event: string, handler: (event: unknown, ctx: unknown) => Promise<void>) => void
  sendUserMessage: (
    content: string,
    options?: { deliverAs?: string; triggerTurn?: boolean },
  ) => void
}): void {
  // In-flight guard: never run two loop-checks (or overlap a re-drive) at
  // once. One boolean: the turn lifecycle + loop-check's NoProgress backstop
  // already bound a stuck session; this just prevents concurrent fires.
  let busy = false

  pi.on("agent_settled", async (_event, ctx) => {
    // Spawn binding first: the keeper sets FNO_AGENT_SESSION_ID only on a
    // process fno itself spawned into the loop lane, so a NATIVE pi session
    // never gates here - no matter what cwd it sits in. Without this, any
    // pi session opened in a directory whose stale .fno/target-state.md
    // survives a finished run would be re-driven against a loop it never
    // joined.
    if (!process.env.FNO_AGENT_SESSION_ID) return
    const dir = process.cwd()
    // Presence guard: the manifest is loop-check's state and the marker that
    // a footnote run owns this directory.
    if (!fnoSessionId(dir)) return
    // ctx.isIdle() is true at agent_settled unless another extension started
    // a run; a busy pi is mid-re-drive or mid-tool and the next settle comes.
    if ((ctx as { isIdle?: () => boolean })?.isIdle?.() === false) return
    if (busy) return
    busy = true

    let decision: { decision?: string; termination_reason?: string } | null = null
    // Declared before the try so the finally can clean it up on every path.
    const synth = join(
      dir,
      ".fno",
      `.pi-loopcheck-${process.pid}-${Date.now()}.jsonl`,
    )
    try {
      // 1. Read this session's assistant messages. buildContextEntries is
      //    the active branch with compaction applied; getBranch is the
      //    fallback on an older sessionManager.
      const sm = (ctx as { sessionManager?: Record<string, () => unknown> })
        .sessionManager
      const read = sm?.buildContextEntries ?? sm?.getBranch
      const entries = read ? (read.call(sm) as unknown[]) : []

      // 2. Synthesize the transcript loop-check reads.
      writeFileSync(synth, synthesizeTranscript(entries))

      // 3. Run the full claude completion gate. loop-check exits 0 for both
      //    allow and block; only CLI misuse / a missing binary throws. On
      //    any failure we NEVER re-drive and NEVER fabricate a termination.
      const bin = process.env.FNO_AGENTS_BIN || "fno-agents"
      decision = await new Promise((resolve) => {
        execFile(
          bin,
          [
            "loop-check",
            "--state",
            join(dir, ".fno", "target-state.md"),
            "--transcript",
            synth,
            "--cwd",
            dir,
          ],
          { cwd: dir, maxBuffer: 10 * 1024 * 1024 },
          (err, stdout) => {
            if (err) {
              console.error(
                `[footnote] loop-check unavailable/failed: ${err}; not re-driving`,
              )
              resolve(null)
              return
            }
            try {
              resolve(JSON.parse(String(stdout)))
            } catch (parseErr) {
              console.error(
                `[footnote] loop-check printed unparseable output: ${parseErr}; not re-driving`,
              )
              resolve(null)
            }
          },
        )
      })
    } catch (e) {
      // A failed transcript read or write must not throw out of a hook: the
      // session settles ungated (as it would with no extension) rather than
      // dying mid-settle.
      console.error(`[footnote] agent_settled handling failed: ${e}`)
      return
    } finally {
      // Clean up the synth transcript (loop-check has already read it by
      // now); ignore failures incl. ENOENT when an early return skipped the
      // write.
      try {
        unlinkSync(synth)
      } catch {
        // nothing to clean up / already gone
      }
      // Release before any re-drive so the re-driven turn's agent_settled is
      // not dropped by this guard.
      busy = false
    }

    if (!decision) return
    // Terminal: loop-check already emitted `termination` - let the session
    // end and emit nothing extra (no duplicate termination event).
    if (decision.termination_reason) return
    // Non-terminal (the world has not caught up, or no promise yet): re-drive
    // the same session in-context. followUp + triggerTurn covers both an
    // idle pi (sends immediately, triggers a turn) and a still-settling one
    // (queues behind the current run). Fire-and-forget; the next turn's
    // agent_settled runs the gate again. loop-check's NoProgress backstop
    // bounds a stuck loop.
    if (decision.decision === "block") {
      try {
        pi.sendUserMessage("/target --resume", {
          deliverAs: "followUp",
          triggerTurn: true,
        })
      } catch (e) {
        console.error(`[footnote] re-drive sendUserMessage threw: ${e}`)
      }
    }
  })
}
