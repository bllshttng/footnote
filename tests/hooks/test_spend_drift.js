#!/usr/bin/env node
// test_spend_drift.js
// Unit tests for hooks/lib/spend-drift.js (pure decision logic for guard (b)
// interactive spend cap and guard (a) Layer 2 model drift). Assert-based; no I/O.

'use strict'
const assert = require('assert')
const path = require('path')
const { modelsMatch, decideSpendDrift } = require(
  path.join(__dirname, '..', '..', 'hooks', 'lib', 'spend-drift')
)

const FRESH = { budgetWarned: false, budgetEscalated: false, driftWarned: false }
let n = 0
const ok = (m) => { n++; console.log('  PASS:', m) }

// --- modelsMatch ---
assert.strictEqual(modelsMatch('claude-opus-4-8', 'claude-opus-4-8[1m]'), true)
ok('window suffix does not count as drift')
assert.strictEqual(modelsMatch('glm-4.6', 'claude-opus-4-8'), false)
ok('glm vs claude is drift')
assert.strictEqual(modelsMatch('claude-opus-4-8', 'claude-sonnet-4-5'), false)
ok('different claude models are drift')
assert.strictEqual(modelsMatch('', 'claude-opus-4-8'), true)
ok('missing intended model -> no drift (fail open)')

// --- guard (b): cap unset -> never warns, even over any spend ---
let r = decideSpendDrift({ capUsd: null, cost: 999, intendedModel: null, actualModel: null, state: { ...FRESH } })
assert.strictEqual(r.message, null)
ok('cap unset -> no spend warning (AC6)')

// --- guard (b): under cap -> no warning ---
r = decideSpendDrift({ capUsd: 5, cost: 4.99, intendedModel: null, actualModel: null, state: { ...FRESH } })
assert.strictEqual(r.message, null)
ok('under cap -> silent')

// --- guard (b): exactly at cap -> warns (>= comparison, AC5) ---
r = decideSpendDrift({ capUsd: 5, cost: 5.0, intendedModel: null, actualModel: null, state: { ...FRESH } })
assert.ok(r.message && r.message.includes('BUDGET CAP') && !r.message.includes('2x'))
assert.strictEqual(r.state.budgetWarned, true)
ok('cost == cap -> one-shot budget warning')

// --- guard (b): one-shot latch -> second call silent ---
r = decideSpendDrift({ capUsd: 5, cost: 5.5, intendedModel: null, actualModel: null, state: r.state })
assert.strictEqual(r.message, null)
ok('budget warning does not repeat once latched')

// --- guard (b): 2x escalation ---
r = decideSpendDrift({ capUsd: 5, cost: 10.5, intendedModel: null, actualModel: null, state: { ...FRESH } })
assert.ok(r.message && r.message.includes('2x'))
assert.strictEqual(r.state.budgetEscalated, true)
ok('cost >= 2x cap -> escalated warning')

// --- guard (a) L2: drift -> warns, preferred over budget ---
r = decideSpendDrift({ capUsd: 5, cost: 99, intendedModel: 'glm-4.6', actualModel: 'claude-opus-4-8', state: { ...FRESH } })
assert.ok(r.message && r.message.includes('MODEL DRIFT'))
assert.strictEqual(r.state.driftWarned, true)
ok('drift fires and preempts budget when both would trip')

// --- guard (a) L2: coherent model -> falls through to budget ---
r = decideSpendDrift({ capUsd: 5, cost: 6, intendedModel: 'claude-opus-4-8', actualModel: 'claude-opus-4-8[1m]', state: { ...FRESH } })
assert.ok(r.message && r.message.includes('BUDGET CAP'))
ok('coherent model -> no drift, budget still checked')

// --- guard (a) L2: drift one-shot ---
r = decideSpendDrift({ capUsd: null, cost: null, intendedModel: 'glm-4.6', actualModel: 'claude-opus-4-8', state: { ...FRESH, driftWarned: true } })
assert.strictEqual(r.message, null)
ok('drift warning does not repeat once latched')

console.log('\nspend-drift:', n, 'passed, 0 failed')
