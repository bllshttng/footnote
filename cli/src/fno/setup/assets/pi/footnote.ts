// footnote <-> pi bridge extension (transport only).
//
// Installed by `fno config setup` into <pi agent dir>/extensions/footnote.ts
// (pi auto-discovers *.ts there; no package publish required). Plain
// TypeScript against node builtins only, so pi loads it with no install step.
//
// This extension is a TRANSPORT and nothing else. Every decision belongs to
// Rust: the gate (`fno-agents loop-check`) decides, the store verbs answer,
// and this file moves bytes. Concretely it carries four abilities:
//
//   1. DISCOVERY: it tells pi where the Footnote skills live, so a native
//      pi session sees `/skill:target` in its own catalog.
//   2. MANIFEST: it asks `fno-agents state path target-state` where the
//      session manifest for this directory lives (the space-resolved path,
//      never a guess like `<cwd>/.fno/target-state.md`).
//   3. BINDING: every settle passes its own session id to the gate with
//      `--harness pi --harness-session <id>`, so the gate answers whether
//      THIS session may drive this target. A foreign session gets a typed
//      refusal, sends nothing, and records why.
//   4. CONTINUATION: on a non-terminal block it sends exactly the string the
//      gate named, with prompt expansion enabled so a skill command reaches
//      the skill. A block without a continuation sends nothing.
//
// Every call is bounded: a wedged child is killed and the settle records one
// `unavailable` entry instead of hanging or fabricating a termination.
// loop-check stays the SOLE completion authority: the extension never
// decides "done", never re-drives on a failure, and never emits a
// termination itself.

import { execFile } from "node:child_process"
import { existsSync, readFileSync, unlinkSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { homedir } from "node:os"
import { join } from "node:path"

type Ctx = {
  sessionManager?: {
    getSessionId?: () => string
    buildContextEntries?: () => unknown[]
    getBranch?: () => unknown[]
  }
  isIdle?: () => boolean
  hasUI?: boolean
  ui?: { notify?: (message: string, level?: string) => unknown }
}

// The gate bound, overridable so a test can drive the timeout path in
// milliseconds instead of five minutes.
function gateTimeoutMs(): number {
  const raw = Number(process.env.FNO_PI_GATE_TIMEOUT_MS || "")
  return Number.isFinite(raw) && raw > 0 ? raw : 300000
}

function distresTimeoutMs(): number {
  const raw = Number(process.env.FNO_PI_DISTRESS_TIMEOUT_MS || "")
  return Number.isFinite(raw) && raw > 0 ? raw : 30000
}

function statePathTimeoutMs(): number {
  const raw = Number(process.env.FNO_PI_STATE_TIMEOUT_MS || "")
  return Number.isFinite(raw) && raw > 0 ? raw : 5000
}

// One bounded child run. Resolves "" when the child failed for any reason
// (spawn error, nonzero exit, timeout) - the caller records why it could not
// read an answer rather than guessing one. An optional stdin payload rides
// to the child (the hook verbs read a claude-shaped payload on stdin).
function runBounded(
  bin: string,
  args: string[],
  timeoutMs: number,
  stdin?: string,
): Promise<string> {
  return new Promise((resolve) => {
    try {
      const child = execFile(
        bin,
        args,
        {
          timeout: timeoutMs,
          killSignal: "SIGKILL",
          maxBuffer: 10 * 1024 * 1024,
        },
        (err, stdout) => resolve(err ? "" : String(stdout)),
      )
      if (stdin !== undefined && child.stdin) {
        child.stdin.write(stdin)
        child.stdin.end()
      }
    } catch {
      resolve("")
    }
  })
}

// The space-resolved manifest for `dir`, when one exists. The verb is the
// only path authority: a directory with no `.fno` of its own still gates,
// because the manifest lives under the session's space root.
async function resolveManifestPath(bin: string, dir: string): Promise<string | null> {
  const out = await runBounded(bin, ["state", "path", "target-state"], statePathTimeoutMs())
  const candidate = out.trim().split("\n").pop()?.trim()
  if (!candidate || !existsSync(candidate)) return null
  void dir
  return candidate
}

// The plugin root the install pointer names, or null with one `[footnote]`
// line naming why. A stale or missing pointer degrades to silence, never to
// an error. The pointer moved under install/ (the state-root tidiness wave);
// an unmigrated install keeps the legacy root name until the daemon migrates.
function pluginRoot(): string | null {
  const base = process.env.FNO_HOME || join(homedir(), ".fno")
  for (const name of ["install/plugin-root", "plugin-root"]) {
    try {
      const root = readFileSync(join(base, name), "utf8").trim()
      if (root && existsSync(join(root, ".claude-plugin", "plugin.json"))) return root
    } catch {
      // try the next spelling
    }
  }
  console.error("[footnote] no plugin-root pointer; hook carriers not offered")
  return null
}

// The skills directory under the plugin root, or null.
function skillsRoot(): string | null {
  const root = pluginRoot()
  if (!root) return null
  if (!existsSync(join(root, "skills"))) {
    console.error("[footnote] plugin root carries no skills dir; skills not offered")
    return null
  }
  return join(root, "skills")
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

// The context text a carrier hook printed: the claude JSON shape's
// additionalContext, else the raw trimmed stdout. A carrier that answered
// nothing injects nothing.
function contextCarrierText(stdout: string): string {
  const t = (stdout || "").trim()
  if (!t) return ""
  try {
    const v = JSON.parse(t)
    const ctx = v?.hookSpecificOutput?.additionalContext
    if (typeof ctx === "string" && ctx) return ctx
    return ""
  } catch {
    return t
  }
}

export default function (pi: {
  on: (
    event: string,
    handler: (event: unknown, ctx: unknown) => Promise<unknown> | void,
  ) => void
  sendUserMessage: (
    content: string,
    options?: {
      deliverAs?: string
      triggerTurn?: boolean
      expandPromptTemplates?: boolean
    },
  ) => void
  appendEntry?: (type: string, data: unknown) => void
}): void {
  // Per-session in-flight guard. This is scheduling, never authority: one
  // loop-check at a time per pi session, and the binding gate still answers
  // who owns the target.
  const busy = new Map<string, boolean>()

  pi.on("session_shutdown", (_event: unknown, ctx: unknown) => {
    const sid = (ctx as Ctx)?.sessionManager?.getSessionId?.()
    if (sid) busy.delete(sid)
  })

  // DISCOVERY: put the Footnote verbs in pi's own catalog as skills.
  pi.on("resources_discover", () => {
    const skills = skillsRoot()
    return skills ? { skillPaths: [skills] } : {}
  })

  // Fleet announcements at the pre-turn boundary.
  // `before_agent_start` fires after a prompt and before the agent loop, and
  // its returned `message` is injected into the session and sent to the LLM -
  // the one pi event that delivers text at a real boundary. One announcement
  // is one bus line; the per-session cursor on the reader side makes this
  // print once and stay silent after. Fail-open: no binary, no output, or a
  // failed read injects nothing.
  // Post-compact re-inject: pi exposes session_compact, so the compaction
  // marks THIS session and the next `before_agent_start` boundary carries
  // the lead's rules again (the same carrier claude runs at SessionStart
  // source=compact). One message per compact; the mark clears on delivery
  // or on a failed carrier - never re-armed mid-turn.
  const crownPending = new Set<string>()
  pi.on("session_compact", (_event: unknown, ctx: unknown) => {
    const sid = (ctx as Ctx)?.sessionManager?.getSessionId?.()
    if (sid) crownPending.add(sid)
  })

  pi.on("before_agent_start", async (_event: unknown, ctx: unknown) => {
    try {
      const sid = (ctx as Ctx)?.sessionManager?.getSessionId?.() || ""
      const sessionKey = process.env.FNO_AGENT_SESSION_ID || sid || `pi:${process.cwd()}`
      const bin = process.env.FNO_AGENTS_BIN || "fno-agents"
      // Crown re-inject first: fresh operating rules outrank an announce.
      // Fail-open: no root, a failed run, or an empty payload injects
      // nothing and drops the mark.
      if (sid && crownPending.delete(sid)) {
        const root = pluginRoot()
        if (root) {
          const out = await runBounded(
            "bash",
            [join(root, "hooks", "king-postcompact-reinject.sh")],
            5000,
            JSON.stringify({
              hook_event_name: "SessionStart",
              source: "compact",
              cwd: process.cwd(),
              session_id: sid,
            }),
          )
          const text = contextCarrierText(out)
          if (text) {
            return {
              message: {
                customType: "fno-crown",
                content: [{ type: "text", text }],
                display: false,
              },
            }
          }
        }
      }
      // Session-start report: the registry holds this session's id (mail and
      // liveness stop guessing). Fire-and-forget, best-effort; a spawned
      // worker passes FNO_AGENT_SELF so the daemon can match its row.
      if (sid) {
        const reportArgs = [
          "report", "--kind", "session",
          "--harness", "pi",
          "--session-id", sid,
        ]
        if (process.env.FNO_AGENT_SELF) {
          reportArgs.push("--agent-self", process.env.FNO_AGENT_SELF)
        }
        runBounded(bin, reportArgs, 2000).catch(() => {})
      }
      const out = await runBounded(
        bin,
        ["announce", "read", "--session-id", sessionKey, "--harness", "pi", "--boundary", "prompt"],
        2000,
      )
      const text = out.trim()
      if (!text) return
      return {
        message: {
          customType: "fno-announce",
          content: [{ type: "text", text }],
          display: true,
        },
      }
    } catch {
      return
    }
  })

  // LEAD GUARD: a crowned pi session does not write repo source. The tool
  // call becomes a claude-shaped PreToolUse payload and the Rust guard
  // answers; its deny vetoed with `block: true` and the guard's reason.
  // Fail-open: no binary, an unreadable answer, or a failed run returns
  // nothing and the tool proceeds - the never-block contract
  // hooks/king-delegation-guard.sh ships under.
  pi.on("tool_call", async (event: unknown, ctx: unknown) => {
    try {
      const ev = event as { toolName?: string; input?: Record<string, unknown> }
      const input = ev?.input || {}
      // pi names paths `path` and shells `command`; the guard reads the
      // claude spellings. Everything else is not judged.
      let toolName = ""
      const shaped: Record<string, unknown> = { ...input }
      if (ev?.toolName === "bash") {
        toolName = "Bash"
      } else if (ev?.toolName === "edit" || ev?.toolName === "write") {
        toolName = ev.toolName === "edit" ? "Edit" : "Write"
        shaped.file_path = typeof input.path === "string" ? input.path : ""
      } else {
        return
      }
      const payload = {
        hook_event_name: "PreToolUse",
        tool_name: toolName,
        tool_input: shaped,
        cwd: process.cwd(),
        session_id: (ctx as Ctx)?.sessionManager?.getSessionId?.() || "",
      }
      const bin = process.env.FNO_AGENTS_BIN || "fno-agents"
      const out = await runBounded(bin, ["hook", "king-guard"], 5000, JSON.stringify(payload))
      if (!out.trim()) return
      let decision: { permissionDecision?: string; reason?: string } | null = null
      try {
        decision = JSON.parse(out)
      } catch {
        return
      }
      if (decision?.permissionDecision === "deny") {
        return {
          block: true,
          reason: decision.reason || "crowned sessions do not write repo source",
        }
      }
    } catch {
      return
    }
  })

  pi.on("agent_settled", async (_event: unknown, ctx: unknown) => {
    const dir = process.cwd()
    const sm = (ctx as Ctx)?.sessionManager
    const sid = sm?.getSessionId?.() || ""
    const bin = process.env.FNO_AGENTS_BIN || "fno-agents"

    // MANIFEST: the verb is the presence guard and the gate's --state.
    const manifestPath = await resolveManifestPath(bin, dir)
    if (!manifestPath) {
      // No footnote session here. A worker that dies before `target init`
      // writes a manifest still carries a distress tag nobody would otherwise
      // read, so the pre-manifest scan keeps the spawned-run requirement
      // (FNO_AGENT_SESSION_ID is the run id it reports under).
      if (!process.env.FNO_AGENT_SESSION_ID) return
      const preSynth = join(
        tmpdir(),
        `.pi-premanifest-${process.pid}-${Date.now()}.jsonl`,
      )
      try {
        const read = sm?.buildContextEntries ?? sm?.getBranch
        const entries = read ? (read.call(sm) as unknown[]) : []
        writeFileSync(preSynth, synthesizeTranscript(entries))
        await runBounded(
          bin,
          [
            "distress-scan",
            "--transcript",
            preSynth,
            "--run",
            String(process.env.FNO_AGENT_SESSION_ID),
            "--harness",
            "pi",
            "--cwd",
            dir,
          ],
          distresTimeoutMs(),
        )
      } catch (e: unknown) {
        console.error(`[footnote] pre-manifest distress-scan skipped (non-fatal): ${e}`)
      } finally {
        try {
          unlinkSync(preSynth)
        } catch {
          // nothing to clean up / already gone
        }
      }
      return
    }

    // ctx.isIdle() is true at agent_settled unless another extension started
    // a run; a busy pi is mid-re-drive or mid-tool and the next settle comes.
    if ((ctx as Ctx)?.isIdle?.() === false) return
    if (!sid || busy.get(sid)) return
    busy.set(sid, true)

    let decision: {
      decision?: string
      termination_reason?: string
      continuation?: string
      message?: string
      reason?: string
    } | null = null
    let unavailableReason = ""
    // Declared before the try so the finally can clean it up on every path.
    const synth = join(tmpdir(), `.pi-loopcheck-${process.pid}-${Date.now()}.jsonl`)
    try {
      // Read this session's assistant messages and synthesize the transcript
      // the gate scans.
      const read = sm?.buildContextEntries ?? sm?.getBranch
      const entries = read ? (read.call(sm) as unknown[]) : []
      writeFileSync(synth, synthesizeTranscript(entries))

      const out = await runBounded(
        bin,
        [
          "loop-check",
          "--state",
          manifestPath,
          "--transcript",
          synth,
          "--cwd",
          dir,
          "--harness",
          "pi",
          "--harness-session",
          sid,
        ],
        gateTimeoutMs(),
      )
      if (!out) {
        unavailableReason = "loop-check failed or timed out"
      } else {
        try {
          decision = JSON.parse(out)
        } catch (parseErr: unknown) {
          unavailableReason = `unparseable gate output: ${parseErr}`
        }
      }
    } catch (e: unknown) {
      // A failed transcript read or write must not throw out of a hook: the
      // session settles ungated (as it would with no extension) rather than
      // dying mid-settle.
      unavailableReason = `settle handling failed: ${e}`
    } finally {
      try {
        unlinkSync(synth)
      } catch {
        // nothing to clean up / early return skipped the write
      }
      busy.delete(sid)
    }

    if (unavailableReason) {
      // Never re-drive on an unreadable gate, and never fabricate a
      // termination: record the fact once, visibly.
      try {
        pi.appendEntry?.("fno-gate", { state: "unavailable", reason: unavailableReason })
      } catch {
        // an appendEntry that throws is the host's problem, not a gate answer
      }
      if ((ctx as Ctx)?.hasUI && (ctx as Ctx)?.ui?.notify) {
        try {
          ;(ctx as Ctx).ui?.notify?.(`[footnote] gate unavailable: ${unavailableReason}`, "warning")
        } catch {
          // notify is best-effort
        }
      }
      return
    }
    if (!decision) return
    // The binding refused this session: send nothing, record why. This turns
    // a wrong-session continuation (the audit's one foreign continuation)
    // into a typed refusal.
    if (decision.decision === "refuse") {
      try {
        pi.appendEntry?.("fno-gate", {
          state: "refused",
          reason: decision.reason || decision.message || "",
        })
      } catch {
        // best-effort record
      }
      return
    }
    // Terminal: loop-check already emitted `termination` - let the session
    // end and emit nothing extra (no duplicate termination event).
    if (decision.termination_reason) return
    // Non-terminal block: send EXACTLY the continuation the gate named, with
    // prompt expansion so a skill command reaches the skill. followUp +
    // triggerTurn covers both an idle pi (sends immediately, triggers a turn)
    // and a still-settling one (queues behind the current run). Fire-and-
    // forget; the next turn's agent_settled runs the gate again. loop-check's
    // NoProgress backstop bounds a stuck loop.
    if (decision.decision === "block") {
      const continuation = decision.continuation
      if (!continuation) {
        try {
          pi.appendEntry?.("fno-gate", {
            state: "unavailable",
            reason: "gate named no continuation",
          })
        } catch {
          // best-effort record
        }
        return
      }
      try {
        pi.sendUserMessage(continuation, {
          deliverAs: "followUp",
          triggerTurn: true,
          expandPromptTemplates: true,
        })
      } catch (e: unknown) {
        console.error(`[footnote] re-drive sendUserMessage threw: ${e}`)
      }
    }
  })
}
