# Review

`/fno:review` puts a second set of eyes on a diff before you ship.
It routes to one reviewer, and the choice is a cost tradeoff.

## The lane versus peer

| Mode | What runs | Cost |
|------|-----------|------|
| default (the owned lane) | one inline reviewer works every angle in this session and emits one head-pinned attestation | low: one model pass, zero subagents |
| peer | one other model reviews your code (for example, codex on claude) | lower than a panel, needs a second provider |

Run the default lane when you want a real gate: it runs inline as ordinary tool calls on every harness, verifies each finding (CONFIRMED, PLAUSIBLE, REFUTED), drops what cannot cite its own line, and emits the same attestation a native review produces.
The specialist agents that once rode a panel remain individually invocable as agents.

Run peer for a second opinion from a model with different blind spots.
A bug two passes of the same model miss, a different model can catch.
Use it on a change that is large, security-sensitive, or hard to reason about by hand, or as a check on a lane pass you already ran.

## How to run each

```
/fno:review                 # the owned lane, one inline pass
/fno:review peer            # cross-model second opinion
/fno:review peer codex      # name the reviewing model
```

The lane reviews your current checkout, not a PR number.
peer takes a PR number: `/fno:review peer 657`.

`/fno:review` also carries `prove-it` (runtime evidence at the changed code's real surface) and `cleanup` (apply-or-skip tidy pass) as separate routes; see [review lanes](../architecture/review-lanes.md) for the full menu.

## Skip review on small diffs

Both routes spend tokens on the whole diff.
For a one-line typo fix, the review costs more than it saves.
Skip review on those, or narrow it with a focused prompt.

## Attestation

A review attestation satisfies the stop gate's review requirement.
The lane emits one on a clean pass.
peer produces one with `--attest`.
An advisory one-off run of either does not attest.
The full gate contract is in [review lanes](../architecture/review-lanes.md).

## Related

- [PR lifecycle](pr-lifecycle.md) covers what happens after review passes.
