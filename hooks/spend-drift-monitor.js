#!/usr/bin/env node
// Spend + model-drift monitor - PostToolUse hook.
// Guards (a) Layer 2 (the routed model drifted from the attested one) and
// (b) the interactive spend cap. Both read the session transcript directly.
//
// This hook USED to also warn on context pressure, by reading a percentage out
// of ~/.claude/.session-context.json (or a /tmp bridge file) written by a
// statusline wrapper. That path is gone. Nothing in this repo ever wrote either
// file - footnote ships no statusline wrapper - so the read failed and the hook
// exited silently on every fire; it never warned once. It was also a
// second-hand percentage whose denominator we could not see or validate, which
// is how it produced 200K-window numbers on a 1M-context model.
//
// Context pressure has ONE measurement path now: `fno whoami context` (the CLI
// implementation that the skill-local shim delegates to), which counts tokens
// from the transcript itself and owns its own denominator.

const fs = require('fs')
const os = require('os')
const path = require('path')
const { execFileSync } = require('child_process')
const { decideSpendDrift } = require('./lib/spend-drift')

// Guards (a) Layer 2 (model drift) + (b) interactive spend cap.
const SPEND_THROTTLE_MS = 60_000 // at most one cost/drift check per minute

// Read config.budget.interactive.cap_usd from config.toml (local > global).
// This value is intentionally UNMODELED (rides extra="ignore" like budget.attended),
// so `fno config get` rejects it — read the raw TOML directly. Parsed natively in
// JS (no python3 / PyYAML dependency, no per-tick process spawn — gemini review).
// The regex is LINE-ANCHORED so `cost_cap_usd =` (config.budget.attended /
// unattended, which contains "cap_usd" as a substring) cannot false-match the
// interactive `cap_usd =`. Returns a positive number, or null (feature off).
function readInteractiveCap() {
  const files = ['.fno/config.toml', path.join(os.homedir(), '.fno', 'config.toml')]
  for (const p of files) {
    try {
      if (!fs.existsSync(p)) continue
      const content = fs.readFileSync(p, 'utf8')
      const m = content.match(/^[ \t]*cap_usd[ \t]*=[ \t]*([0-9.]+)/m)
      if (m) {
        const n = parseFloat(m[1])
        if (Number.isFinite(n) && n > 0) return n
      }
    } catch (e) {
      /* unreadable — try the next path, else feature off */
    }
  }
  return null
}

// Live spend-so-far ($) from the transcript. null on any failure (fails open).
function probeCost(sessionId) {
  try {
    const out = execFileSync('fno', ['cost', sessionId, '--json'], {
      encoding: 'utf8',
      timeout: 10000,
      stdio: ['ignore', 'pipe', 'ignore'],
    })
    const j = JSON.parse(out)
    const c = typeof j.cost_usd === 'number' ? j.cost_usd : parseFloat(j.cost_usd)
    return Number.isFinite(c) ? c : null
  } catch (e) {
    return null
  }
}

// Actual running model from the transcript via the single CLI implementation.
// Reused instead of an fno whoami cost call so the drift check honors AC6 (no cost
// call when the cap is unset). null on any failure.
function probeModel(transcriptPath) {
  if (!transcriptPath) return null
  try {
    const out = execFileSync('fno', ['context', '--transcript', transcriptPath, '--json'], {
      encoding: 'utf8',
      timeout: 8000,
      stdio: ['ignore', 'pipe', 'ignore'],
    })
    const j = JSON.parse(out)
    return j && typeof j.model === 'string' && j.model ? j.model : null
  } catch (e) {
    return null
  }
}

// Throttled, one-shot spend + drift check. Returns { message, commit } where
// message is the warning to emit (or null) and commit() persists the one-shot
// latch. The caller MUST call commit() only AFTER a successful stdout.write, so
// a failed emit (e.g. EPIPE) re-warns next tick instead of latching the genuine
// first trip into silence. The throttle timestamp is persisted here regardless.
function checkSpendAndDrift(sessionId, transcriptPath, tmpDir) {
  const NOOP = { message: null, commit: () => {} }
  try {
    const spendPath = path.join(tmpDir, `claude-ctx-${sessionId}-spend.json`)
    const persist = (s) => {
      try {
        fs.writeFileSync(spendPath, JSON.stringify(s))
      } catch (e) {}
    }
    let state = { lastTs: 0, budgetWarned: false, budgetEscalated: false, driftWarned: false }
    if (fs.existsSync(spendPath)) {
      try {
        state = { ...state, ...JSON.parse(fs.readFileSync(spendPath, 'utf8')) }
      } catch (e) {
        /* corrupt sidecar — re-check rather than go silent */
      }
    }

    // Throttle: no shell-outs while a prior check ran <60s ago (AC7).
    const now = Date.now()
    if (now - (state.lastTs || 0) < SPEND_THROTTLE_MS) return NOOP

    const capUsd = readInteractiveCap()

    // Drift is checkable only if L1 recorded an intended model and we haven't warned.
    let intendedModel = null
    if (!state.driftWarned) {
      const fnoHome = process.env.FNO_HOME || path.join(os.homedir(), '.fno')
      const attestPath = path.join(fnoHome, 'attest', `${sessionId}.json`)
      if (fs.existsSync(attestPath)) {
        try {
          const a = JSON.parse(fs.readFileSync(attestPath, 'utf8'))
          if (a && typeof a.model === 'string' && a.model) intendedModel = a.model
        } catch (e) {
          /* ignore */
        }
      }
    }

    state.lastTs = now
    // Nothing to check -> record the tick and bail. AC6: with the cap unset AND
    // no drift target, no `fno whoami cost` call is made.
    if (capUsd === null && !intendedModel) {
      persist(state)
      return NOOP
    }

    const actualModel = intendedModel ? probeModel(transcriptPath) : null
    const cost = capUsd !== null ? probeCost(sessionId) : null

    // Snapshot the prior latch flags before deciding.
    const prevFlags = {
      budgetWarned: state.budgetWarned,
      budgetEscalated: state.budgetEscalated,
      driftWarned: state.driftWarned,
    }
    const { message, state: latch } = decideSpendDrift({
      capUsd,
      cost,
      intendedModel,
      actualModel,
      state,
    })

    if (!message) {
      Object.assign(state, latch)
      persist(state)
      return NOOP
    }

    // A warning will be emitted by the caller. Persist the advanced throttle NOW
    // but keep the prior (pre-trip) latch flags, so a failed emit re-warns. The
    // caller's commit() writes the new latch only after stdout.write succeeds.
    persist({ ...state, ...prevFlags })
    const latched = { ...state, ...latch }
    return { message, commit: () => persist(latched) }
  } catch (e) {
    return NOOP
  }
}

let input = ''
// Timeout guard: if stdin doesn't close within 3s, exit silently
const stdinTimeout = setTimeout(() => process.exit(0), 3000)
process.stdin.setEncoding('utf8')
process.stdin.on('data', (chunk) => (input += chunk))
process.stdin.on('end', () => {
  clearTimeout(stdinTimeout)
  try {
    const data = JSON.parse(input)
    const sessionId = data.session_id

    if (!sessionId) {
      process.exit(0)
    }

    // Guards (a) Layer 2 (model drift) + (b) interactive spend cap. Throttled,
    // one-shot; when one fires it preempts this tick's context-pressure warning
    // (both latch, so nothing is lost — the context warning re-fires next tick).
    const spend = checkSpendAndDrift(sessionId, data.transcript_path, os.tmpdir())
    if (spend.message) {
      const payload = JSON.stringify({
        hookSpecificOutput: { hookEventName: 'PostToolUse', additionalContext: spend.message },
      })
      // Synchronous write so an EPIPE throws HERE (caught by the outer try),
      // BEFORE we latch. process.stdout.write() only queues on a pipe and can
      // fail asynchronously, which would latch a warning that never reached the
      // reader. Latch the one-shot only after the bytes are actually written.
      fs.writeSync(1, payload)
      spend.commit()
      process.exit(0)
    }

    // No spend/drift message this tick: nothing else to say.
    process.exit(0)
  } catch (e) {
    // Silent fail — never block tool execution
    process.exit(0)
  }
})
