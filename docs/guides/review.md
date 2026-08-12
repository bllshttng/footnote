# Review

`/fno:review` puts a second set of eyes on a diff before you ship.
It routes to two reviewers, and the choice is a cost tradeoff.

## sigma versus peer

| Mode | What runs | Cost |
|------|-----------|------|
| sigma (default) | a six-agent internal panel with runtime attribution | high: six model runs on the diff |
| peer | one other model reviews your code (for example, codex on claude) | lower: one model run |

Run sigma for the deepest pass.
It fans out to specialist agents (silent-failure hunting, type design, test coverage, ux) and aggregates their findings.
Use it on a change that is large, security-sensitive, or hard to reason about by hand.

Run peer for a fast second opinion from a model with different blind spots.
A bug two agents of the same model miss, a different model can catch.
Use it for a small or routine diff, or as a check on a sigma pass you already ran.

## How to run each

```
/fno:review                 # sigma, the default
/fno:review sigma           # explicit sigma
/fno:review peer            # cross-model second opinion
/fno:review peer codex      # name the reviewing model
```

The panel reviews your local commits by default.
Aim it at a PR by passing the number.

## Skip review on small diffs

Both modes spend tokens on the whole diff.
For a one-line typo fix, the review costs more than it saves.
Skip review on those, or narrow it with a focused prompt.

## Attestation

peer can stamp a local review attestation the stop gate accepts.
Pass `--attest` to produce one.
sigma does not attest.
It is advisory.
The full gate contract is in [review lanes](../architecture/review-lanes.md).

## Related

- [PR lifecycle](pr-lifecycle.md) covers what happens after review passes.
- [Review lanes](../architecture/review-lanes.md) covers the native-verb paths and the gate.
