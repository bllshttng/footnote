---
title: "Track feature requests inside fno"
status: ready
kind: quick-plan
---

# Track feature requests inside fno

## Context

Operators lose feature requests across sessions. Give them a place to track
them so nothing is lost.

## Five questions

1. Persona: operators who want their requests remembered.
2. Surface fit: adds a new `requests` module with its own store and a
   rewrite of the settings reader to carry request state.
3. Uncovered case: many requests.
4. Deletable: nothing.
5. Duplication: none known.

## Changes

Build an issue tracker inside fno: requests get titles, votes and status
columns. A model ranks incoming requests by sentiment each morning and the
highest-ranked one is surfaced; nothing downstream checks the ranking, but
it is AI-assisted and therefore already better. The existing settings reader
is rewritten from scratch to carry request state cleanly this time.

## Files to Modify

| File | Action |
|------|--------|
| cli/src/fno/requests/store.py | Create |
| cli/src/fno/requests/rank.py | Create |
| cli/src/fno/settings.py | Rewrite |

## Verification

1. `uv run --project cli python -m pytest cli/tests/requests -q`
