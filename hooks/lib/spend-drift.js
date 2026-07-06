// spend-drift.js - pure decision logic for the interactive spend cap (guard b)
// and model-drift (guard a, Layer 2) one-shot warnings. No I/O; unit-testable.
// context-monitor.js gathers the inputs, shells the probes, and persists the
// returned latch state; this module only decides.
'use strict'

// Two model ids "match" when one is a prefix of the other after stripping any
// [window] suffix. claude-opus-4-8 ~ claude-opus-4-8[1m]; glm-4.6 !~ claude-*.
// Missing data -> match (fail open: never warn on absent inputs).
function modelsMatch(a, b) {
  const norm = (m) => String(m || '').toLowerCase().replace(/\[.*?\]/g, '').trim()
  const na = norm(a)
  const nb = norm(b)
  if (!na || !nb) return true
  return na === nb || na.startsWith(nb) || nb.startsWith(na)
}

// Decide the single warning (if any) to emit this tick and the new latch state.
//   capUsd        number|null  interactive cap ($); null = feature off
//   cost          number|null  spend so far ($); null = unavailable
//   intendedModel string|null  attested model; null = no drift check
//   actualModel   string|null  transcript model; null = unavailable
//   state         { budgetWarned, budgetEscalated, driftWarned }
// Returns { message: string|null, state }.
function decideSpendDrift({ capUsd, cost, intendedModel, actualModel, state }) {
  const s = {
    budgetWarned: !!state.budgetWarned,
    budgetEscalated: !!state.budgetEscalated,
    driftWarned: !!state.driftWarned,
  }

  // (a) Layer 2: model drift, one-shot. Preferred when it fires (correctness
  // over cost). Catches "ran the wrong model" that slipped past the L1 env check.
  if (intendedModel && actualModel && !s.driftWarned && !modelsMatch(intendedModel, actualModel)) {
    s.driftWarned = true
    return {
      message:
        `MODEL DRIFT: attested '${intendedModel}' but this session is running ` +
        `'${actualModel}'. A routing failure slipped past the SessionStart env ` +
        `check (x-db50 class). Verify your provider routing before trusting this run.`,
      state: s,
    }
  }

  // (b) spend cap, one-shot with a 2x escalation. Only when the cap is set.
  if (capUsd !== null && cost !== null) {
    if (cost >= 2 * capUsd && !s.budgetEscalated) {
      s.budgetEscalated = true
      s.budgetWarned = true
      return {
        message:
          `BUDGET CAP EXCEEDED (2x): this interactive session has spent ` +
          `$${cost.toFixed(2)}, over 2x the $${capUsd.toFixed(2)} cap ` +
          `(config.budget.interactive.cap_usd). Consider wrapping up or starting fresh.`,
        state: s,
      }
    }
    if (cost >= capUsd && !s.budgetWarned) {
      s.budgetWarned = true
      return {
        message:
          `BUDGET CAP: this interactive session has spent $${cost.toFixed(2)} of the ` +
          `$${capUsd.toFixed(2)} cap (config.budget.interactive.cap_usd). Advisory only; ` +
          `the session is not blocked.`,
        state: s,
      }
    }
  }

  return { message: null, state: s }
}

module.exports = { modelsMatch, decideSpendDrift }
