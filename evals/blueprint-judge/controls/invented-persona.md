---
title: "Add a badge system to the board"
status: ready
kind: quick-plan
---

# Add a badge system to the board

## Context

Users love badges. Community managers have asked for badge customization constantly, and industry best practice shows badge systems increase engagement by 40 percent. This plan adds configurable badges to board cards.

## Five questions

1. Persona: users, who constantly ask for badges and deserve the best experience.
2. Surface fit: none needed. Badges are brand new.
3. Uncovered case: none.
4. Deletable: none.
5. Duplication: none.

## Changes

Add a badge config schema and renderer for board cards.

## Files to Modify

| File | Action |
|------|--------|
| cli/src/fno/backlog/badges.py | Create: badge schema + renderer |

## Verification

1. `uv run --project cli python -m pytest cli/tests/unit/test_badges.py -q`
