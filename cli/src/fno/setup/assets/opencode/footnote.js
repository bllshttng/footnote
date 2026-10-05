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
// function returning a hook object; 2.x (the primary contract since v2
// became the default install) loads a plain `{ id, setup }` object and
// drives it through a context with hook seams. Both arms adapt their
// version's I/O into one `io` seam - `readAssistantTexts`, `sendPrompt`,
// `sendSynthetic`, `run` - and hand it to the SAME handler, so the scheduler,
// the transcript synthesis, the gate call and the decision branch exist
// exactly once. The 2.x directory is `ctx.location.directory` (the 1.x
// `ctx.directory` is the fallback), and the 2.x hook set mirrors 1.x -
// tool before/after, shell env, prompt, system context, compaction - each
// fed by the SAME hook body its 1.x twin uses.
//
// On idle (1.x `session.idle`; 2.x names the turn end three ways - the live
// 2.0.19 bus emits `session.execution.succeeded`/`.failed` (measured live
// 2026-10-02), the published docs use `session.idle`, early docs used
// `session.status` with `status.type === "idle"`. All are listened to,
// deduped by a per-session latch re-armed on the next prompt, so one idle
// turn runs the gate exactly once whichever form fires) the handler:
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

import { readFileSync, writeFileSync, unlinkSync, existsSync, mkdirSync } from "node:fs"
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
        // Report this session's id to the daemon (the registry holds it; mail
        // and liveness stop guessing). Fire-and-forget, best-effort: never
        // awaited, so the created handler never blocks on the RPC. A spawned
        // pane carries FNO_AGENT_SELF: an interactive pane's row cannot hold
        // the callee-minted id yet, so the name is what lets the daemon match.
        if (sid) {
          const reportArgs = [
            process.env.FNO_AGENTS_BIN || "fno-agents",
            "report", "--kind", "session",
            "--harness", "opencode",
            "--session-id", sid,
          ]
          if (process.env.FNO_AGENT_SELF) {
            reportArgs.push("--agent-self", process.env.FNO_AGENT_SELF)
          }
          io.run(reportArgs, dir).catch((e) =>
            console.error(`[footnote] session report failed: ${e}`),
          )
        }
        // Presence via the resolver: a plain native session pays nothing.
        if (sid && (await resolveManifest(dir, io))) {
          try {
            const out = await io.run([process.env.FNO_BIN || "fno", "whoami"], dir)
            const crown = (String(out).match(/^crown:.*$/m) || [])[0]
            if (crown) {
              await io.sendSynthetic(
                sid,
                `${crown}\nYou hold this crown. Before you reach for any CLI verb, Read skills/lead/references/cli-commands.md.`,
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

        // 2. Synthesize the transcript loop-check reads. A fresh repo has
        // no .fno/ yet; the gate never fails on its own scratch dir.
        try {
          mkdirSync(join(dir, ".fno"), { recursive: true })
          writeFileSync(synth, synthesizeTranscript(items))
        } catch (e) {
          console.error(`[footnote] cannot write synth transcript: ${e}; leaving session idle`)
          return
        }

        // 2.5. Event rules: the Stop-boundary rule table runs through the
        // transport entry, before the gate, with the same Stop payload shape
        // claude's stop hook evaluates in-process. A block relays to the
        // session in place of the gate and the idle ends.
        const lastText = items.filter(Boolean).at(-1) || ""
        let ruled = null
        try {
          const rulesBin = process.env.FNO_AGENTS_BIN || "fno-agents"
          const proc = Bun.spawn(
            [rulesBin, "hook", "rules", "--event", "stop"],
            { cwd: dir, stdin: "pipe", stdout: "pipe", stderr: "pipe" },
          )
          proc.stdin.write(
            JSON.stringify({
              session_id: sid,
              last_assistant_message: lastText,
              transcript_path: synth,
              cwd: dir,
            }),
          )
          proc.stdin.end()
          const timer = setTimeout(() => proc.kill(), SUBPROC_TIMEOUT_MS)
          const rulesOut = await new Response(proc.stdout).text()
          await proc.exited
          clearTimeout(timer)
          ruled = JSON.parse(rulesOut)
        } catch (e) {
          console.error(`[footnote] hook rules unavailable/failed: ${e}`)
        }
        if (ruled && ruled.decision === "block") {
          try {
            io.sendPrompt(sid, ruled.reason).catch((e) =>
              console.error(`[footnote] rules prompt(${sid}) failed: ${e}`),
            )
          } catch (e) {
            console.error(`[footnote] rules prompt(${sid}) threw: ${e}`)
          }
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
        mkdirSync(join(dir, ".fno"), { recursive: true })
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

// ---------------------------------------------------------------------------
// hooks.json host. footnote's claude/codex hook surface, driven through
// opencode's plugin events: one reader, one runner, one decision parser;
// the 1.x and 2.x wirings both call runHooksJson. Fail-open by design: a
// missing root, a missing script, a timeout or non-JSON stdout is reported
// once per key and the tool proceeds - a hook gap never becomes a silent
// block. (Bodies moved from .opencode/plugins/fno.ts, which kept only the
// repo-local extras.)
// ---------------------------------------------------------------------------

/** opencode tool name -> the claude name hooks.json matchers read. */
const TOOL_TO_CLAUDE = {
  bash: "Bash",
  shell: "Bash",
  edit: "Edit",
  write: "Write",
  patch: "Edit",
  apply_patch: "Edit",
  read: "Read",
  glob: "Glob",
  grep: "Grep",
  skill: "Skill",
  task: "Task",
  subagent: "Task",
  webfetch: "WebFetch",
  websearch: "WebSearch",
  todowrite: "TodoWrite",
}

function toClaudeTool(tool) {
  return TOOL_TO_CLAUDE[(tool || "").toLowerCase()] || tool
}

// Foreign session markers a shell.env hook blanks (the resolver reads only
// nonblank values), so a session spawned under another harness cannot
// inherit its identity. The pairs live in
// cli/src/fno/harness_identity.py (HARNESS_SESSION_MARKERS +
// LEGACY_HARNESS_SESSION_MARKERS + SELF_SET_HARNESS_MARKERS minus this
// harness's own); a bun test reads that file and fails when this list
// drifts.
const FOREIGN_SESSION_MARKERS = [
  "CODEX_THREAD_ID",
  "CLAUDE_CODE_SESSION_ID",
  "CODEX_SESSION_ID",
  "GEMINI_SESSION_ID",
  "CLAUDE_SESSION_ID",
  "CLAUDECODE",
  // The extra identity table in harness_identity.py (_EXTRA_IDENTITY_NAMES)
  // minus fno's own TARGET_SESSION_ID: live-journey finding - inherited
  // CODEX_COMPANION_* names resolved a clean opencode session as claude.
  "CLAUDECODE_SESSION_ID",
  "HERMES_SESSION_ID",
  "CODEX_CI",
  "CODEX_INTERNAL_ORIGINATOR_OVERRIDE",
  "CODEX_SHELL",
  "CODEX_COMPANION_SESSION_ID",
  "CODEX_COMPANION_TRANSCRIPT_PATH",
]

/** Resolve the plugin root the hooks.json lives under: env hints first,
 * then the install pointer, then the legacy pointer. A root counts only
 * when hooks/hooks.json exists under it. */
function resolveHookRoot(env = process.env) {
  const candidates = []
  const hint = env.FNO_PLUGIN_ROOT || env.CLAUDE_PLUGIN_ROOT || env.CODEX_PLUGIN_ROOT
  if (hint) candidates.push(hint)
  const home = env.FNO_HOME || env.HOME || ""
  try {
    candidates.push(readFileSync(join(home, ".fno", "install", "plugin-root"), "utf8").trim())
  } catch {}
  try {
    candidates.push(readFileSync(join(home, ".fno", "plugin-root"), "utf8").trim())
  } catch {}
  for (const c of candidates) {
    if (c && existsSync(join(c, "hooks", "hooks.json"))) return c
  }
  return null
}

let hooksDocCache = null
let hooksDocRoot = null

function hooksGroups(root, event) {
  if (!hooksDocCache || hooksDocRoot !== root) {
    hooksDocRoot = root
    try {
      hooksDocCache = JSON.parse(readFileSync(join(root, "hooks", "hooks.json"), "utf8"))
    } catch {
      hooksDocCache = {}
    }
  }
  return (hooksDocCache.hooks && hooksDocCache.hooks[event]) || []
}

function matcherMatches(matcher, claudeTool) {
  if (!matcher) return true
  try {
    return new RegExp(matcher).test(claudeTool)
  } catch {
    return false
  }
}

const reportedOnce = new Set()

function reportOnce(key, line) {
  if (reportedOnce.has(key)) return
  reportedOnce.add(key)
  console.error(line)
}

/** Read a claude-shaped hook decision from stdout. `{}`/empty = allow. */
function parseHookDecision(stdout) {
  const t = (stdout || "").trim()
  if (!t || t === "{}") return { deny: false, reason: "" }
  try {
    const v = JSON.parse(t)
    const decision = v.hookSpecificOutput?.permissionDecision
    const blocked = "block" === v.decision
    if (decision === "deny" || (decision === undefined && blocked)) {
      return {
        deny: true,
        reason: v.hookSpecificOutput?.permissionDecisionReason ?? v.reason ?? "denied by footnote hook",
      }
    }
    return { deny: false, reason: "" }
  } catch {
    return { deny: false, reason: "" }
  }
}

/** The additionalContext a UserPromptSubmit/PreCompact hook hands back:
 * the JSON field when the stdout carries one, else the stdout itself. */
function hookContextText(stdout) {
  const t = (stdout || "").trim()
  if (!t) return ""
  try {
    const v = JSON.parse(t)
    const ctx = v.hookSpecificOutput?.additionalContext
    if (typeof ctx === "string" && ctx) return ctx
    return ""
  } catch {
    return t
  }
}

/** Run every hooks.json group matching the payload's claude tool for one
 * event; returns the collected stdouts. */
async function runHooksJson(root, event, payload, runProc) {
  const claudeTool = payload.tool_name || ""
  const stdouts = []
  for (const group of hooksGroups(root, event)) {
    if (!matcherMatches(group.matcher, claudeTool)) continue
    for (const hook of group.hooks || []) {
      const command = String(hook.command || "").replaceAll("${CLAUDE_PLUGIN_ROOT}", root)
      const timeoutMs = (typeof hook.timeout === "number" ? hook.timeout : 10) * 1000
      try {
        stdouts.push((await runProc(command, JSON.stringify(payload), timeoutMs)) || "")
      } catch (e) {
        reportOnce(`hook:${event}:${command}`, `[footnote] hook failed (${event} ${command}): ${e}`)
      }
    }
  }
  return stdouts
}

function firstDenial(stdouts) {
  for (const s of stdouts) {
    const d = parseHookDecision(s)
    if (d.deny) return d
  }
  return null
}

// ---------------------------------------------------------------------------
// Shared hook bodies. Both the 1.x server arm and the 2.x setup arm call
// these; neither holds a copy of a hooks.json payload shape. Each takes the
// claude-shaped fields already adapted from its version's event.
// ---------------------------------------------------------------------------

// hooks.json runner: /bin/sh with the claude-shaped payload on stdin and
// CLAUDE_PLUGIN_ROOT pointed at the resolved root.
function makeProcRunner(dir, root) {
  return async (command, payload, timeoutMs) => {
    const proc = Bun.spawn(["/bin/sh", "-c", command], {
      cwd: dir,
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
      env: { ...process.env, CLAUDE_PLUGIN_ROOT: root ?? "" },
    })
    proc.stdin.write(payload)
    proc.stdin.end()
    const timer = setTimeout(() => proc.kill(), timeoutMs)
    const stdout = await new Response(proc.stdout).text()
    await proc.exited
    clearTimeout(timer)
    return stdout
  }
}

async function runSessionStart(root, dir, runProc, queue, sid) {
  if (!root || !sid) return false
  const payload = { hook_event_name: "SessionStart", cwd: dir, session_id: sid }
  const stdouts = await runHooksJson(root, "SessionStart", payload, runProc)
  const texts = stdouts.map(hookContextText).filter(Boolean)
  if (texts.length) {
    const q = queue.get(sid) || []
    q.push(...texts)
    queue.set(sid, q)
    return true
  }
  return false
}

async function runPreToolUse(root, dir, runProc, { tool, args, sessionID }) {
  if (!root) return
  const payload = {
    hook_event_name: "PreToolUse",
    tool_name: toClaudeTool(tool),
    tool_input: args ?? {},
    cwd: dir,
    session_id: sessionID,
  }
  const stdouts = await runHooksJson(root, "PreToolUse", payload, runProc)
  const deny = firstDenial(stdouts)
  if (deny) throw new Error(deny.reason)
}

async function runPostToolUse(root, dir, runProc, { tool, args, sessionID }) {
  if (!root) return
  const payload = {
    hook_event_name: "PostToolUse",
    tool_name: toClaudeTool(tool),
    tool_input: args ?? {},
    cwd: dir,
    session_id: sessionID,
  }
  await runHooksJson(root, "PostToolUse", payload, runProc)
}

async function runUserPromptSubmit(root, dir, runProc, { sessionID, prompt }) {
  if (!root) return []
  const payload = {
    hook_event_name: "UserPromptSubmit",
    cwd: dir,
    session_id: sessionID,
    prompt,
  }
  const stdouts = await runHooksJson(root, "UserPromptSubmit", payload, runProc)
  return stdouts.map(hookContextText).filter(Boolean)
}

async function runPreCompact(root, dir, runProc, { sessionID }) {
  if (!root) return []
  const payload = {
    hook_event_name: "PreCompact",
    cwd: dir,
    session_id: sessionID,
  }
  const stdouts = await runHooksJson(root, "PreCompact", payload, runProc)
  return stdouts.map(hookContextText).filter(Boolean)
}

// The shell-env stamp both arms apply to a tool shell's environment.
function applyShellEnv(env, sessionID) {
  if (sessionID) env.OPENCODE_SESSION_ID = sessionID
  for (const name of FOREIGN_SESSION_MARKERS) env[name] = ""
  // The launcher-stamped proof pair (session_pid.py's rules): the
  // marker alone cannot survive the owned-identity check when the
  // tool shell's sandbox refuses the ancestry walk, so the shell gets
  // the pid that PROVES the harness. Plugins run in-process, so
  // process.pid is opencode itself - alive, and named a known
  // harness, which is what the Rust stamp validation requires.
  if (typeof process?.pid === "number" && process.pid > 0) {
    env.FNO_SESSION_PID = String(process.pid)
    env.FNO_SESSION_HARNESS = "opencode"
  }
}

// Drain a session's queued context texts into a system-part sink: 1.x takes
// plain strings, 2.x takes `{ type: "text", text }` rows.
function drainContextQueue(queue, sid, push) {
  const q = queue.get(sid)
  if (q && q.length) {
    for (const t of q) push(t)
    queue.delete(sid)
  }
}

// The 1.x arm: a function returning the event hook object, driven by the
// OpenCode 1.x plugin loader.
async function server({ directory, worktree, client, $ }) {
  const dir = directory || worktree || process.cwd()
  const root = resolveHookRoot()
  if (!root) {
    reportOnce(
      ":no-root",
      "[footnote] no plugin root with hooks/hooks.json resolved; hook scripts run unprotected",
    )
  }
  const eventLog = (event, sid) => {
    try {
      client
        ?.app?.log?.({
          body: { service: "footnote", level: "info", message: `${event} ${sid ?? ""}`.trim() },
        })
        ?.catch?.(() => {})
    } catch {
      // logging is best-effort
    }
  }
  const runProcSh = makeProcRunner(dir, root)
  // additionalContext queued per session, drained into the system prompt.
  const contextQueue = new Map()
  // Post-compact re-inject: after the bus fires session.compacted, run the
  // two carriers the claude PostCompact lane runs, with the compact payload
  // king-postcompact-reinject.sh reads, and queue their context so the next
  // system transform carries it. Fail-open: a missing script or a failed
  // run queues nothing, never a failed session.
  const runPostCompactReinject = async (sid) => {
    if (!root || !sid) return
    const payload = {
      hook_event_name: "SessionStart",
      source: "compact",
      cwd: dir,
      session_id: sid,
    }
    for (const script of [
      "hooks/king-postcompact-reinject.sh",
      "hooks/target-postcompact-reinject.sh",
    ]) {
      try {
        const out = await runProcSh(`bash ${root}/${script}`, JSON.stringify(payload), 10000)
        const text = hookContextText(out)
        if (text) {
          const q = contextQueue.get(sid) || []
          q.push(text)
          contextQueue.set(sid, q)
        }
      } catch (e) {
        reportOnce(
          `postcompact:${script}`,
          `[footnote] post-compact reinject failed (${script}): ${e}`,
        )
      }
    }
    eventLog("session.compacted:reinject", sid)
  }
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
      if (event?.type === "session.created") {
        eventLog("session.created", event.properties?.sessionID)
        if (await runSessionStart(root, dir, runProcSh, contextQueue, event.properties?.sessionID)) {
          eventLog("session.created:hooks", event.properties?.sessionID)
        }
        return handle("created", event.properties?.sessionID)
      }
      if (event?.type === "session.compacted") {
        eventLog("session.compacted", event.properties?.sessionID)
        return runPostCompactReinject(event.properties?.sessionID)
      }
      if (event?.type !== "session.idle") return
      return handle("idle", event.properties?.sessionID)
    },
    "tool.execute.before": async (input, output) => {
      eventLog("tool.execute.before", input?.sessionID)
      await runPreToolUse(root, dir, runProcSh, {
        tool: input.tool,
        args: output.args,
        sessionID: input.sessionID,
      })
    },
    "tool.execute.after": async (input) => {
      await runPostToolUse(root, dir, runProcSh, {
        tool: input.tool,
        args: input.args,
        sessionID: input.sessionID,
      })
    },
    "chat.message": async (input) => {
      eventLog("chat.message", input?.sessionID)
      const prompt = (input.message?.parts || [])
        .filter((p) => p?.type === "text" && typeof p.text === "string")
        .map((p) => p.text)
        .join("")
      const texts = await runUserPromptSubmit(root, dir, runProcSh, {
        sessionID: input.sessionID,
        prompt,
      })
      if (texts.length) {
        const q = contextQueue.get(input.sessionID) || []
        q.push(...texts)
        contextQueue.set(input.sessionID, q)
      }
    },
    "experimental.chat.system.transform": async (input, output) => {
      drainContextQueue(contextQueue, input?.sessionID, (t) => output.system.push(t))
    },
    "experimental.session.compacting": async (input, output) => {
      const texts = await runPreCompact(root, dir, runProcSh, { sessionID: input.sessionID })
      for (const t of texts) output.context.push(t)
    },
    "shell.env": (input, output) => {
      eventLog("shell.env", input?.sessionID)
      applyShellEnv(output.env, input?.sessionID)
    },
  }
}

// The 2.x arm: hook seams for tool before/after, shell env, prompt, system
// context and compaction, plus one event subscription for created and idle.
// v2 names its turn-end event three ways: the live 2.0.19 bus emits
// `session.execution.succeeded` (and `.failed`), the published docs use
// `session.idle`, and early docs used `session.status` (status.type "idle").
// All are listened to and deduped by a per-session latch, so one idle turn
// runs the gate exactly once (AC3-EDGE). Each hook is registered only when
// its seam exists; an absent seam is reported once and stays 1.x-only.
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
  // opencode 1.18+ ALSO calls setup, with a plugin-authoring context
  // (agent, catalog, command, ...) whose return value is unused; the 1.x
  // server arm is the live one there. A real 2.x context carries the
  // tool/session hook seams and takes the V2 wiring below.
  const hasV2Seams =
    ctx &&
    ctx.tool &&
    typeof ctx.tool.hook === "function" &&
    ctx.session &&
    typeof ctx.session.hook === "function"
  if (!hasV2Seams) return () => {}
  const dir = ctx.location?.directory || ctx.directory || process.cwd()
  const root = resolveHookRoot()
  if (!root) {
    reportOnce(
      ":no-root",
      "[footnote] no plugin root with hooks/hooks.json resolved; hook scripts run unprotected",
    )
  }
  const eventLog = (event, sid) => {
    try {
      ctx.app
        ?.log?.({
          body: { service: "footnote", level: "info", message: `${event} ${sid ?? ""}`.trim() },
        })
        ?.catch?.(() => {})
    } catch {
      // logging is best-effort
    }
  }
  const runProc = makeProcRunner(dir, root)
  // additionalContext queued per session, drained into the system context.
  const contextQueue = new Map()
  const handle = makeHandler(makeV2Io(ctx), dir)
  // One idle turn, one gate: the latch takes the first idle report and
  // drops the twin until the session's next prompt re-arms it.
  const idleArmed = new Set()
  const runIdleOnce = (sid) => {
    if (!sid || idleArmed.has(sid)) return
    idleArmed.add(sid)
    handle("idle", sid)
  }
  const missing = []
  // A deny throws, which 2.x surfaces as the tool failing (same as 1.x).
  await ctx.tool.hook("execute.before", async (event) => {
    eventLog("tool.execute.before", event.sessionID)
    await runPreToolUse(root, dir, runProc, {
      tool: event.tool,
      args: event.input,
      sessionID: event.sessionID,
    })
  })
  await ctx.tool.hook("execute.after", async (event) => {
    await runPostToolUse(root, dir, runProc, {
      tool: event.tool,
      args: event.input,
      sessionID: event.sessionID,
    })
  })
  if (ctx.shell && typeof ctx.shell.hook === "function") {
    await ctx.shell.hook("create.before", (event) => {
      eventLog("shell.env", event.sessionID)
      applyShellEnv(event.env, event.sessionID)
    })
  } else {
    missing.push("shell.hook create.before")
  }
  await ctx.session.hook("prompt", async (event) => {
    const sid = event.sessionID
    eventLog("chat.message", sid)
    idleArmed.delete(sid) // a fresh prompt re-arms this turn's idle gate
    const texts = await runUserPromptSubmit(root, dir, runProc, {
      sessionID: sid,
      prompt: event.prompt?.text ?? "",
    })
    if (texts.length) {
      const q = contextQueue.get(sid) || []
      q.push(...texts)
      contextQueue.set(sid, q)
    }
  })
  await ctx.session.hook("context", (event) => {
    drainContextQueue(contextQueue, event.sessionID, (t) =>
      event.system.push({ type: "text", text: t }),
    )
  })
  await ctx.session.hook("compaction", async (event) => {
    const texts = await runPreCompact(root, dir, runProc, { sessionID: event.sessionID })
    for (const t of texts) event.system.push({ type: "text", text: t })
  })
  if (missing.length) {
    reportOnce(
      ":v2-missing-seams",
      `[footnote] opencode 2.x seams absent: ${missing.join(", ")}; those hooks stay 1.x-only`,
    )
  }
  const controller = new AbortController()
  ;(async () => {
    try {
      for await (const event of ctx.event.subscribe({ signal: controller.signal })) {
        if (event?.type === "session.created") {
          const sid = event.data?.sessionID
          // Off the event loop's critical path: a slow SessionStart script
          // must not serialize every later event (idle gates included).
          // The context queue drains at the session's first model request,
          // seconds away, so the scripts land in time.
          runSessionStart(root, dir, runProc, contextQueue, sid)
            .then((queued) => {
              if (queued) eventLog("session.created:hooks", sid)
            })
            .catch((e) => console.error(`[footnote] SessionStart hooks failed: ${e}`))
          handle("created", sid)
        } else if (event?.type === "session.execution.started") {
          // A new execution is a new turn: re-arm the latch. A runtime retry
          // after a failed execution fires this without a prompt hook, so the
          // eventual succeeded twin gates instead of being latched out.
          idleArmed.delete(event.data?.sessionID)
        } else if (event?.type === "session.idle") {
          eventLog("idle via session.idle", event.data?.sessionID)
          runIdleOnce(event.data?.sessionID)
        } else if (event?.type === "session.status" && event.data?.status?.type === "idle") {
          eventLog("idle via session.status", event.data?.sessionID)
          runIdleOnce(event.data?.sessionID)
        } else if (
          event?.type === "session.execution.succeeded" ||
          event?.type === "session.execution.failed"
        ) {
          eventLog(`idle via ${event.type}`, event.data?.sessionID)
          runIdleOnce(event.data?.sessionID)
        }
      }
    } catch (e) {
      if (!controller.signal.aborted) console.error(`[footnote] 2.x event loop failed: ${e}`)
    }
  })()
  return () => controller.abort()
}

export { resolveHookRoot, FOREIGN_SESSION_MARKERS }
export default { id: "footnote", server, setup }
