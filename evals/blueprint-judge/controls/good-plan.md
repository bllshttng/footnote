---
title: "Retire the groom worker's duplicate lock check"
status: ready
kind: quick-plan
---

# Retire the groom worker's duplicate lock check

## Context

Since the 2026-08-14 dispatch change, overnight groom runs re-acquire the groom lock twice per run, and each worker waits 30 s (groom-2026-08-20 run log). `fno agents claim` already refuses a second acquirer.

## Five questions

1. Persona: the operator, who reads groom reports each morning. Each run wastes 30 s of wall clock they wait on. Source: groom-2026-08-20 run log.
2. Surface fit: extends `fno agents claim`'s existing `--wait` flag. No new verb.
3. Uncovered case: a claim holder whose pid died between acquire and check - handled by reusing claim's existing liveness probe.
4. Deletable: the second `claim acquire` call in groom.py:280. The first already holds the lock.
5. Duplication: extends the existing claim lockfile reader in `fno/agents/claim.py` rather than adding one.

## Changes

Delete the duplicate lock acquisition at cli/src/fno/backlog/groom.py:280.

## Files to Modify

| File | Action |
|------|--------|
| cli/src/fno/backlog/groom.py | Modify: drop the duplicate acquire |

## Verification

1. `uv run --project cli python -m pytest cli/tests/unit/test_groom.py -q`
