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

const WARNING_THRESHOLD = 35 // remaining_percentage <= 35%
const CRITICAL_THRESHOLD = 25 // remaining_percentage <= 25%
const DEBOUNCE_CALLS = 5 // min tool uses between warnings
const SESSION_CONTEXT_PATH = path.join(os.homedir(), '.claude', '.session-context.json')

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
