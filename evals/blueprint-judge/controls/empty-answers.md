---
title: "Add a weekly digest mail to the feed projection"
status: ready
kind: quick-plan
---

# Add a weekly digest mail to the feed projection

## Context

The activity feed joins questions and graph events into one ordered projection. This plan adds a weekly digest of that projection delivered as agent mail.

## Five questions

(none answered)

## Changes

Add a digest builder beside the feed projection and a mail send on Monday.

## Files to Modify

| File | Action |
|------|--------|
| cli/src/fno/agents/feed.py | Modify: add digest builder |

## Verification

1. `uv run --project cli python -m pytest cli/tests/unit/test_feed.py -q`
