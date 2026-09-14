---
title: "Ship a second backlog board renderer"
status: ready
kind: quick-plan
---

# Ship a second backlog board renderer

## Context

This plan adds a new Kanban board renderer with its own column logic, rank handling and lane prefixes, invoked as `fno backlog board2`.

## Five questions

1. Persona: none.
2. Surface fit: none.
3. Uncovered case: none.
4. Deletable: none.
5. Duplication: none.

## Changes

Add `fno backlog board2`, a second renderer with its own column logic and rank ordering.

## Files to Modify

| File | Action |
|------|--------|
| cli/src/fno/backlog/board2.py | Create: the renderer |

## Verification

1. `uv run --project cli python -m pytest cli/tests/unit/test_board2.py -q`
