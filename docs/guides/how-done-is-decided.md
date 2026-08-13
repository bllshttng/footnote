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
fno pr status <PR_NUMBER>
```

It prints the CI verdict: green, red, pending, or unknown.
Exit code 0 is green, 1 is red, 2 is pending.
This is the CI verdict alone.
A green CI with a required reviewer still pending still reads green.

Read the run's manifest:

```bash
fno state show
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
Trust `fno pr status` for CI, not the agent's self-report.

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
