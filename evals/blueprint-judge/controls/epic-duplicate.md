---
title: "Add a board queue for undriven PRs"
status: ready
kind: quick-plan
---

# Add a board queue for undriven PRs

## Five questions

1. Persona: the operator, who re-checks merged PRs for follow-up work by hand each morning and loses minutes per PR. Source: operator request in the dispatch log.
2. Surface fit: extends `fno do pr merged` with a `--queue` flag. No new verb.
3. Uncovered case: a PR merged while the queue tick sleeps - picked up on the next tick, named as eventual, not instant.
4. Deletable: none; each piece feeds the queue.
5. Duplication: extends the existing pr-watch agent rather than adding a daemon.

## Changes

Add a queue table keyed by PR number, fed by the post-merge ritual.

## Verification

1. Queue replays after a tick restart with no loss.
