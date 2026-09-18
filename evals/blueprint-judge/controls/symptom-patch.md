---
title: "Fix the groom worker AttributeError on a missing lane field"
status: ready
kind: quick-plan
---

# Fix the groom worker AttributeError on a missing lane field

## Context

The 2026-09-10 overnight run log reports the groom worker crashing:

> AttributeError: 'NoneType' object has no attribute 'lower'

The crash happens in the dispatch lane sort.

## Changes

Guard the call site the report names: in cli/src/fno/backlog/groom.py, wrap
the lane sort so a None lane is replaced with the empty string before
`.lower()` is called.

## Files to Modify

| File | Action |
|------|--------|
| cli/src/fno/backlog/groom.py | Modify: None-guard the lane sort |

## Verification

1. Rerun the overnight groom job and confirm no crash in the log.
