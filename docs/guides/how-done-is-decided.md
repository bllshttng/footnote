# How "done" is decided

A target run keeps going until the world proves it is done.
The agent does not self-certify.
This page is the user view.
For the internals, see [the control plane loop](../architecture/control-plane-loop.md).

## The short version

Three external facts decide done:

1. A PR exists for your commit.
2. CI is green on that PR.
3. Every required reviewer has approved it.

The agent cannot vouch for any of these.
It reads them from GitHub through one decision verb.

## Watch a run that is still going

Read the PR's CI from outside the session:

```bash
fno do pr status <PR_NUMBER>
```

It prints the CI verdict: green, red, pending, or unknown.
Exit code 0 is green, 1 is red, 2 is pending.
This is the CI verdict alone.
A green CI with a required reviewer still pending still reads green.

`checks.total` counts the whole rollup, which holds two different kinds of row. GitHub check-runs carry a `name` and come from Actions or a Checks-API app. Commit statuses carry a `context` and are posted to the statuses endpoint. `checks.check_runs` and `checks.statuses` split the total, so a total that disagrees with `gh api repos/OWNER/REPO/commits/SHA/check-runs` is legible rather than alarming: that endpoint returns only the first kind. On PR 994 the tally said 15, the endpoint named 13, and the two extra rows were fno's own `stacked-base-guard` and `fno/review-coverage` statuses.

That gap has a second cause, and it runs the other way. Every `checks` count is taken over the latest run per name, while the endpoint returns every attempt. So a PR that re-ran or force-pushed a job carries superseded rows the endpoint reports and the tally does not. Measured on this PR: 18 rows from the endpoint against a 14-row tally, where the extra four were re-runs a body edit had already replaced. Subtract `checks.statuses` alone and a gap survives. Read the endpoint deduped by name, keeping the newest, before either number is worth comparing.

A red is named for the kind of row that failed. Once a real check-run reaches a failing conclusion, `ready_blockers` carries `ci_red`. Once every failing row is `CANCELLED` or `STALE`, it carries `ci_cancelled_retrigger`: the verdict and exit code remain red, but the name tells the caller to trigger a newer run instead of fixing code. Once every failing row is a commit status and no job failed at all, it carries `commit_status_red`. All three block. A concluded failure dominates a cancelled or stale sibling, so a mixed result remains `ci_red`.

The answer can come from the coalescing cache. That cache keys one row per repo, PR and head, so a verdict never answers for a commit it was not computed at. A served answer says so in `cached`. It says how second-hand it is in `cached_at` and `cached_age_seconds`. The `head` field names the commit the verdict was computed at. To refuse the cache for one read, pass `--refresh` (`--no-cache` is the same flag). When a verdict looks wrong, use that flag by hand. Do not put it in a poll loop: the coalescing is what keeps a fleet of watchers under GitHub's REST secondary limit.

Read the run's manifest:

```bash
fno do state show
```

It prints the node, the plan path, and the merge flags.
For the live claim holder, check the claim:

```bash
fno claim status node:<id>
```

For full run orientation:

```bash
fno status
```

## What "still going" usually means

A run that has not stopped is usually waiting on something real.
CI is pending, a review bot has not posted, or a budget cap has not tripped.
None of those is the agent stuck in a loop.
It is waiting because the world has not caught up.

## Read the verdict, not the mood

The agent's own text is not evidence.
"Almost done" in the transcript does not move the gate.
The decision verb reads GitHub, not the transcript.
Trust `fno do pr status` for CI, not the agent's self-report.

## Stop it yourself

A run refuses to stop until done is proven, by design.
To stop one on purpose:

```bash
touch .fno/.target-cancelled
```

This is the supported off switch.
Do not edit `.fno/target-state.md`.
It is an immutable manifest.

## The decision verb

The verb behind all of this is `fno-agents loop-check`.
It runs inside the session at the stop hook and reads only external truth.
The full mechanism (the manifest, the shim, the done reads, the budget cap) is in [the control plane loop](../architecture/control-plane-loop.md).
