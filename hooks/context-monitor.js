#!/usr/bin/env node
// Context Monitor - PostToolUse hook
// Reads context metrics from ~/.claude/.session-context.json (written by statusline)
// and injects warnings when context usage is high. This makes the AGENT aware of
// context limits (the statusline only shows the user).
//
// How it works:
// 1. The statusline writes metrics to ~/.claude/.session-context.json on every update
// 2. This hook reads those metrics after each tool use
// 3. Validates session_id matches (handles concurrent sessions)
// 4. When remaining context drops below thresholds, it injects a warning
//    as additionalContext, which the agent sees in its conversation
//
// Thresholds:
//   WARNING  (remaining <= 35%): Agent should wrap up current task
//   CRITICAL (remaining <= 25%): Agent should stop immediately and save state
//
// Debounce: 5 tool uses between warnings to avoid spam
// Severity escalation bypasses debounce (WARNING -> CRITICAL fires immediately)

const fs = require('fs')
const os = require('os')
const path = require('path')
const { execFileSync } = require('child_process')
const { decideSpendDrift } = require('./lib/spend-drift')

const WARNING_THRESHOLD = 35 // remaining_percentage <= 35%
const CRITICAL_THRESHOLD = 25 // remaining_percentage <= 25%
const DEBOUNCE_CALLS = 5 // min tool uses between warnings
const SESSION_CONTEXT_PATH = path.join(os.homedir(), '.claude', '.session-context.json')

// Guards (a) Layer 2 (model drift) + (b) interactive spend cap.
const SPEND_THROTTLE_MS = 60_000 // at most one cost/drift check per minute

// Read config.budget.interactive.cap_usd from settings.yaml (local > global).
// This value is intentionally UNMODELED (rides extra="ignore" like budget.attended),
// so `fno config get` rejects it — read the raw YAML the same way loopcheck.rs does.
// Returns a positive number, or null when unset / unreadable (feature off).
const PY_READ_CAP = `
import yaml, os
def rd(p):
    try:
        d = yaml.safe_load(open(p)) or {}
    except Exception:
        return None
    c = d.get('config') or {}
    b = c.get('budget') or {}
    i = b.get('interactive') or {}
    return i.get('cap_usd')
for p in ['.fno/settings.yaml', os.path.expanduser('~/.fno/settings.yaml')]:
    v = rd(p)
    if v is not None:
        print(v)
        break
`

function readInteractiveCap() {
  try {
    const out = execFileSync('python3', ['-c', PY_READ_CAP], {
      encoding: 'utf8',
      timeout: 5000,
      stdio: ['ignore', 'pipe', 'ignore'],
    }).trim()
    const n = parseFloat(out)
    return Number.isFinite(n) && n > 0 ? n : null
  } catch (e) {
    return null
  }
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

// Actual running model from the transcript via the sanctioned probe. Reused
// instead of an fno cost call so the drift check honors AC6 (no cost call when
// the cap is unset). null on any failure.
function probeModel(transcriptPath) {
  if (!transcriptPath) return null
  const root = process.env.CLAUDE_PLUGIN_ROOT || path.join(__dirname, '..')
  const probe = path.join(root, 'skills', 'target', 'scripts', 'context-probe.sh')
  if (!fs.existsSync(probe)) return null
  try {
    const out = execFileSync('bash', [probe, transcriptPath], {
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
      const attestPath = path.join(os.homedir(), '.claude', `.fno-attest-${sessionId}.json`)
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
    // no drift target, no `fno cost` call is made.
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

/**
 * Parse the mode field from target-state.md.
 * Returns null if the state file is stale (owner PID dead) — callers then
 * treat the session as non-target, avoiding target-flavored warnings for
 * ghost state left over from a prior session.
 * @param {string} statePath - Path to target-state.md
 * @returns {string|null} 'interactive', 'autonomous', or null if no live session
 */
function parseTargetMode(statePath) {
  try {
    const content = fs.readFileSync(statePath, 'utf8')

    const ownerPidMatch = content.match(/^owner_pid:\s*(\d+)/m)
    if (ownerPidMatch) {
      const ownerPid = parseInt(ownerPidMatch[1], 10)
      try {
        process.kill(ownerPid, 0)
      } catch (e) {
        // ESRCH = no such process → state is orphaned.
        if (e.code === 'ESRCH') return null
      }
    }

    const match = content.match(/^mode:\s*(\w+)/m)
    return match ? match[1] : 'interactive'
  } catch (e) {
    return null
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
      process.stdout.write(
        JSON.stringify({
          hookSpecificOutput: { hookEventName: 'PostToolUse', additionalContext: spend.message },
        })
      )
      // Latch the one-shot only after the warning actually reached stdout.
      spend.commit()
      process.exit(0)
    }

    // Read metrics: try .session-context.json first, fall back to bridge file
    let remaining, usedPct

    if (fs.existsSync(SESSION_CONTEXT_PATH)) {
      try {
        const ctx = JSON.parse(fs.readFileSync(SESSION_CONTEXT_PATH, 'utf8'))
        if (ctx.session_id === sessionId) {
          const cw = ctx.context_window || {}
          remaining = cw.remaining_percentage
          usedPct = cw.used_percentage || cw.used_pct
        }
      } catch (e) {
        /* fall through to bridge file */
      }
    }

    // Fallback: per-session bridge file (handles concurrent sessions)
    if (remaining === undefined) {
      const bridgePath = path.join(os.tmpdir(), `claude-ctx-${sessionId}.json`)
      if (!fs.existsSync(bridgePath)) {
        process.exit(0)
      }
      try {
        const bridge = JSON.parse(fs.readFileSync(bridgePath, 'utf8'))
        remaining = bridge.remaining_percentage
        usedPct = bridge.used_percentage || bridge.used_pct
      } catch (e) {
        process.exit(0)
      }
    }

    if (remaining === undefined || remaining === null) {
      process.exit(0)
    }

    // No warning needed
    if (remaining > WARNING_THRESHOLD) {
      process.exit(0)
    }

    // Debounce: check if we warned recently
    const tmpDir = os.tmpdir()
    const warnPath = path.join(tmpDir, `claude-ctx-${sessionId}-warned.json`)
    let warnData = { callsSinceWarn: 0, lastLevel: null }
    let firstWarn = true

    if (fs.existsSync(warnPath)) {
      try {
        warnData = JSON.parse(fs.readFileSync(warnPath, 'utf8'))
        firstWarn = false
      } catch (e) {
        // Corrupted file, reset
      }
    }

    warnData.callsSinceWarn = (warnData.callsSinceWarn || 0) + 1

    const isCritical = remaining <= CRITICAL_THRESHOLD
    const currentLevel = isCritical ? 'critical' : 'warning'

    // Emit immediately on first warning, then debounce subsequent ones
    // Severity escalation (WARNING -> CRITICAL) bypasses debounce
    const severityEscalated = currentLevel === 'critical' && warnData.lastLevel === 'warning'
    if (!firstWarn && warnData.callsSinceWarn < DEBOUNCE_CALLS && !severityEscalated) {
      // Update counter and exit without warning
      fs.writeFileSync(warnPath, JSON.stringify(warnData))
      process.exit(0)
    }

    // Reset debounce counter
    warnData.callsSinceWarn = 0
    warnData.lastLevel = currentLevel
    fs.writeFileSync(warnPath, JSON.stringify(warnData))

    // Detect if target is active and which mode
    const cwd = data.cwd || process.cwd()
    const targetStatePath = path.join(cwd, '.fno', 'target-state.md')
    const isTargetActive = fs.existsSync(targetStatePath)
    const targetMode = isTargetActive ? parseTargetMode(targetStatePath) : null

    // Build mode-aware warning message
    let message

    if (isCritical && targetMode === 'autonomous') {
      message =
        `CONTEXT CRITICAL: ${usedPct}% used, ${remaining}% remaining. ` +
        'Save current progress to target-state.md and output <restart> signal. ' +
        'The external loop will start a fresh session.'
    } else if (isCritical && targetMode === 'interactive') {
      message =
        `CONTEXT CRITICAL: ${usedPct}% used, ${remaining}% remaining. ` +
        'You should wrap up your current task and inform the user that context is low. ' +
        'The user can type /clear at the next natural breakpoint.'
    } else if (isCritical) {
      message =
        `CONTEXT CRITICAL: ${usedPct}% used, ${remaining}% remaining. ` +
        'Context is nearly exhausted. Inform the user that context is low and ask how ' +
        'they want to proceed.'
    } else if (targetMode === 'autonomous') {
      message =
        `CONTEXT WARNING: ${usedPct}% used, ${remaining}% remaining. ` +
        'Context is getting limited. If the current task/wave is nearly complete, finish it. ' +
        'Otherwise, consider outputting <restart> to get a fresh session.'
    } else if (targetMode === 'interactive') {
      message =
        `CONTEXT WARNING: ${usedPct}% used, ${remaining}% remaining. ` +
        'Context is getting limited. Avoid starting new complex work. If not between ' +
        'defined plan steps, inform the user so they can prepare to pause.'
    } else {
      message =
        `CONTEXT WARNING: ${usedPct}% used, ${remaining}% remaining. ` +
        'Be aware that context is getting limited. Avoid unnecessary exploration or ' +
        'starting new complex work.'
    }

    const output = {
      hookSpecificOutput: {
        hookEventName: 'PostToolUse',
        additionalContext: message,
      },
    }

    process.stdout.write(JSON.stringify(output))
  } catch (e) {
    // Silent fail — never block tool execution
    process.exit(0)
  }
})
