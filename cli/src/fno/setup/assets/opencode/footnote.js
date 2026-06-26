// footnote <-> OpenCode bridge plugin.
//
// Installed by `fno setup` into ~/.config/opencode/plugins/footnote.js
// (local-file load path; no npm publish required). Plain JS, zero deps, so
// OpenCode/Bun loads it directly with no `bun install`.
//
// Purpose: OpenCode runs as a footnote loop-wrapper harness (see
// scripts/lib/driver-opencode.sh). The Rust loop runtime terminates a unit
// when a `termination` event for its session_id appears in .fno/events.jsonl
// (loop_runtime.rs::find_termination). OpenCode has no stop-hook to emit that
// event, so this plugin is the native bridge: when the agent emits
// `<promise>MISSION COMPLETE ...` in its output, emit the termination event.
//
// Reason is DonePRGreen by design: the loop-wrapper path trusts the agent's
// promise (the same cheap-path model the legacy bash loop's driver_check_promise
// used), and the agent runs the full /target pipeline incl. PR creation before
// promising. It is NOT independently world-verified here (that gate is
// claude-only via fno-agents loop-check).
//
// If there is no footnote session (no .fno/target-state.md in the project),
// the plugin no-ops, so a plain native OpenCode session is unaffected.

import { readFileSync, appendFileSync } from "node:fs"
import { join } from "node:path"

// Mirrors driver_check_promise / loopcheck: <promise> ... MISSION COMPLETE.
const PROMISE_RE = /<promise>[^<]*MISSION COMPLETE/

function fnoSessionId(dir) {
  try {
    const txt = readFileSync(join(dir, ".fno", "target-state.md"), "utf8")
    const m = txt.match(/^session_id:\s*"?([^"\s]+)"?/m)
    return m ? m[1] : null
  } catch {
    return null
  }
}

export const FootnotePlugin = async ({ directory, worktree, $ }) => {
  const dir = directory || worktree || process.cwd()
  let fired = false

  return {
    event: async ({ event }) => {
      if (fired) return
      if (event?.type !== "message.part.updated") return
      const part = event.properties?.part
      if (!part || part.type !== "text" || typeof part.text !== "string") return
      if (!PROMISE_RE.test(part.text)) return

      const sid = fnoSessionId(dir)
      if (!sid) return // no footnote session here -> nothing to terminate

      fired = true
      const data = JSON.stringify({
        session_id: sid,
        reason: "DonePRGreen",
        message: "opencode promise detected (loop-wrapper bridge)",
      })
      try {
        // fno writes the canonical {ts,type,source,data} envelope + dedup.
        await $`cd ${dir} && fno event emit --type termination --data ${data}`.quiet()
      } catch {
        // Best-effort fallback: drop the promise signal file the driver reads.
        try {
          appendFileSync(join(dir, ".fno", "target-promise.signal"), "MISSION COMPLETE\n")
        } catch {
          // give up silently; never throw from a plugin hook
        }
      }
    },
  }
}
