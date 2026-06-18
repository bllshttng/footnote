# Troubleshooting

The failures below are the ones real runs hit most often, ordered roughly by how much trouble they cause. For first-install verification see [getting-started.md](getting-started.md); for credentials see [auth.md](auth.md); for the habits that prevent most of these see [best-practices.md](best-practices.md).

## A run runs for a long time and never converges

The most disruptive failure mode is a session that keeps working (committing, re-planning, re-polling) while no completion gate ever flips. It runs long and burns many iterations without getting closer to a green PR.

The root cause is almost always one of: the plan is wrong for the size of the task (the loop keeps editing without satisfying any phase contract), or the session is spinning at the review/CI step waiting on something that will not arrive.

What to do:

- Bound runs by time. Set a wall-clock cap with `config.budget.unattended.wall_clock_cap_minutes` (and the `attended` equivalent); caps are opt-in and default to unlimited, so by default nothing stops a runaway. When the cap trips, the loop terminates with reason `Budget`. The time cap is the dependable bound; do not rely on the cost cap as a safety net, since cost accounting has not been reliable. See [configuration-guide.md](configuration-guide.md).
- If it is already running and clearly stuck, cancel it (see "A run will not stop" below).
- Before re-running, shrink the task or split the plan. A run that thrashes is usually being asked to do too much in one pass.

## /target exits early having produced nothing

A run that stalls in its first phase and never emits an artifact gets force-cancelled by a stall detector, so you see it stop quickly with nothing to show. This is common and it is not a crash.

What to check:

- The plan and its acceptance criteria actually exist and are readable. An empty or malformed plan is the usual cause.
- The install is current (`fno doctor`); a stale binary can no-op a phase that a newer one would complete.
- Re-run with a smaller, clearer ask. Vague large asks stall here most.

## `fno: command not found`, or a verb you expect is missing

The deployed `fno` is a snapshot. A verb added to the source after your last install is invisible to the installed binary, and a missing verb often shows up indirectly (for example a loop that will not finish because a capture step silently did nothing). Check for skew first, network-free:

```bash
fno doctor          # fresh / stale / unknown
fno doctor --json   # lists any missing verbs
fno doctor --fix    # refresh a stale install
```

If `fno --version` itself fails, the console script is not on your PATH; reinstall from the repo root with `pip install -e cli/` and re-open the shell.

## Credentials: account, GitHub, and billing

- `claude /status` does not show your account: run `claude login`.
- PR creation fails at the ship phase: `gh` is not authenticated. Check `gh auth status`, fix with `gh auth login`.
- A run billed the wrong account: if more than one Claude.ai account is signed in, footnote uses whichever one Claude Code is active as. Verify with `claude /status` before any long loop. To bill an API account separately, set `ANTHROPIC_API_KEY`, which overrides OAuth. See [auth.md](auth.md).

## A run gets stuck waiting on PR review or CI

Sessions that spin at the review step are usually polling GitHub for review state or CI results. Two things make this worse: GitHub API rate limits during frequent polling, and occasional `gh` output-schema changes that break a checks query. If a run is stuck here:

- Confirm CI is actually progressing on the PR, and that every required reviewer bot has run. Completion needs the PR green and reviewed, so a never-arriving review keeps the loop alive.
- If you are rate-limited, give it time or re-run later; tight polling against a rate-limited token will not make progress.
- Keep `gh` updated, since a stale or mismatched `gh` can return checks JSON the loop cannot parse.

## A run will not stop

This is by design. A target session refuses to stop until completion is proven by external truth: the PR exists, CI is green, every bot in `config.review.required_bots` has reviewed with no unaddressed blocking finding, and any budget cap has not tripped. User-initiated cancel is one of the most common ways real runs end, so when you need to stop one, do it explicitly:

```bash
touch .fno/.target-cancelled    # or:
export TARGET_CANCEL=1
```

Do not try to stop a loop by editing `.fno/target-state.md`. That file is an immutable session manifest; the cancel sentinel is the supported off switch.

## "Where am I?" after a compaction or a long session

Do not grep state files. Ask the agent stack directly:

```bash
fno whoami    # one line: fleet + walker + session + provider
fno status    # gate-by-gate satisfaction, recent events, flagged inconsistencies
```

Both are read-only.

## A backlog node never gets picked

`fno backlog next` skips nodes that are blocked or deferred. Check why:

```bash
fno backlog get <id>      # status, blocked_by, deferred_reason
fno backlog ready         # the nodes actually eligible now
```

A node stays `blocked` while any dependency is open and `deferred` until you `fno backlog undefer <id>`. A node whose PR merged outside the ship gate (a manual GitHub merge) can still look open locally; `fno backlog reconcile` closes that drift.

## Contributor-side failures

These show up when developing footnote itself rather than running it against your own repo.

- **A write is refused on the canonical checkout's protected branch.** Implementation entry refuses to run on `main` in the canonical checkout. Create a worktree at `~/conductor/workspaces/<repo>/<name>` and run `bash scripts/setup/setup-worktree.sh`; the documented escape, when you really mean it, is `TARGET_LOCATION_OK=main-acknowledged`.
- **`git worktree add` fails with "already used by worktree".** You are adding a branch that is already checked out elsewhere, often because you ran the command from inside another worktree. Add from the canonical root with a fresh branch name.
- **A shell command is blocked for its text, not its effect.** Guards block any command whose text contains a dangerous phrase (a literal merge command, a force-push), even inside an echo or a commit message. Route merges through `fno pr merge` and write PR/commit bodies with `--body-file` rather than inline.
- **CI fails on the LOC ratchet.** A positive executable-LOC delta on a control-plane path needs a `loc-exception: <reason>` line in the PR body plus a matching entry in `scripts/ci/loc-ratchet-trajectory.yaml`. The failure output prints the exact delta and steps. Prefer deleting before adding.
- **"Text file busy (os error 26)" or an intermittent test failure.** These are rerun-able flakes, not real failures; re-run the job before investigating.
- **`command not found` for `gtimeout`, `mapfile`, and friends.** Hooks run with a stripped PATH, and macOS lacks some GNU tools. Use portable equivalents and absolute binary paths in scripts.
