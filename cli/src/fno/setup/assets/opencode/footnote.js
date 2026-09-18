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
// OpenCode has no exit-veto hook, OpenCode's `session.idle` event (published
// by the server at every turn end, incl. re-driven turns - see
// packages/opencode/src/session/status.ts) is that observation point. On idle we:
//
//   1. resolve the session's target manifest via `fno-agents state path
//      target-state` (the space-resolved manifest; the legacy in-repo
//      .fno/target-state.md is the fallback when the verb prints nothing),
//   2. read the turn's assistant text via client.session.messages,
//   3. synthesize a minimal claude-shaped transcript jsonl,
//   4. shell `fno-agents loop-check` (the SAME completion gate claude uses)
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

// The manifest resolver. `state path target-state` is the space-resolved
// truth (manifests live at ~/.fno/spaces/<space>/worktrees/<name>/); the
// legacy in-repo path is the fallback when the verb prints nothing or the
// printed file does not exist. 5s bound: a wedged verb must never hold an
// idle event.
const MANIFEST_VERB_TIMEOUT_MS = 5000

async function resolveManifest(dir, $) {
  let timer
  try {
    const bin = process.env.FNO_AGENTS_BIN || "fno-agents"
    const run = $`cd ${dir} && ${bin} state path target-state`
      .quiet()
      .text()
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

export const FootnotePlugin = async ({ directory, worktree, client, $ }) => {
  const dir = directory || worktree || process.cwd()
  // Per-session gate scheduler (Change 2): one entry per session id. An idle
  // for an idle session runs; an idle for a RUNNING session marks `rerun` and
  // returns; when a gate finishes and its entry reads `rerun`, it runs once
  // more. Different sessions never block each other, and no idle is dropped.
  // Deliberately NOT authority: the gate's binding refusal (Change 1) is what
  // keeps ownership correct across a plugin reload, so a reload costs a
  // redundant gate run and never a wrong continuation.
  const gates = new Map()

  return {
    event: async ({ event }) => {
      if (event?.type === "session.created") {
        const sid = event.properties?.sessionID
        // Presence via the resolver: a plain native session pays nothing.
        if (sid && (await resolveManifest(dir, $))) {
          try {
            const out = await $`cd ${dir} && fno whoami 2>/dev/null`.quiet().text()
            const crown = (out.match(/^crown:.*$/m) || [])[0]
            if (crown) {
              await client.session.prompt({
                path: { id: sid },
                body: {
                  noReply: true,
                  parts: [
                    {
                      type: "text",
                      text: `${crown}\nYou hold this crown. Before you reach for any CLI verb, Read skills/king-for-a-day/references/cli-commands.md.`,
                    },
                  ],
                },
              })
            }
          } catch (e) {
            console.error(`[footnote] crown inject failed: ${e}`)
          }
        }
        return
      }
      if (event?.type !== "session.idle") return
      const sid = event.properties?.sessionID
      if (!sid) return

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

      async function runGate(sid) {
        let decision = null
        let items = []
        // Resolved per fire: manifests appear and disappear as targets start
        // and finish. When nothing resolves, the gate still runs against the
        // legacy candidate path so a crowned session reaches its own evidence
        // path and an unbound session gets its refusal (AC2-*).
        const manifestPath = await resolveManifest(dir, $)
        const stateArg = manifestPath || join(dir, ".fno", "target-state.md")
        const synth = join(dir, ".fno", `.opencode-loopcheck-${sid}.jsonl`)
        try {
          // 1. Read this session's assistant messages.
          try {
            const res = await client.session.messages({ path: { id: sid } })
            items = Array.isArray(res?.data) ? res.data : []
          } catch (e) {
            console.error(`[footnote] session.messages(${sid}) failed: ${e}; leaving session idle`)
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
            const out = await $`cd ${dir} && ${bin} loop-check --state ${stateArg} --transcript ${synth} --cwd ${dir} --harness opencode --harness-session ${sid}`
              .quiet()
              .text()
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
            client.session
              .prompt({
                path: { id: sid },
                body: { parts: [{ type: "text", text: continuation }] },
              })
              .catch((e) => console.error(`[footnote] re-drive prompt(${sid}) failed: ${e}`))
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
          await $`cd ${dir} && ${bin} distress-scan --transcript ${synth} --run ${sid} --harness opencode --cwd ${dir}`
            .quiet()
            .text()
        } catch (e) {
          console.error(`[footnote] distress-scan skipped (non-fatal): ${e}`)
        }
      }
    },
  }
}
