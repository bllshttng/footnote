# Best practices

These are the habits that separate clean runs from stuck ones, drawn from how the system actually behaves across many runs. For the failures these habits prevent, see [troubleshooting.md](troubleshooting.md).

## Size the first run small

The fastest path to a successful run is a feature a single iteration can ship in a few minutes: a `/health` endpoint, a `--version` flag, a small doc. Prove the pipeline end to end before pointing it at something large. Most runaway runs are runs asked to do too much in one pass, where the plan never fits the task and the loop thrashes.

## Think and plan before you target on anything ambiguous

For underspecified work, run `/think` (or approve a plan in Claude Code's Plan Mode) before `/target`. The stuck runs that burn the most time share a root cause: planning that does not match the size of the task, so the loop keeps changing code without satisfying any phase contract. The design and acceptance criteria you settle up front are what keep an autonomous run from wandering. `/target` on a clear, small ask is fine; `/target` on a vague large one is where thrash comes from.

## Bound long runs by wall-clock time

Caps are opt-in and default to unlimited, so a stuck run has no ceiling unless you set one. Set `config.budget.unattended.wall_clock_cap_minutes` before unattended loops. Use the time cap, not the cost cap, as your bound: cost accounting has not been reliable, so a cost cap is not a safety net you should depend on. A wall-clock cap reliably ends a run that would otherwise spin.

## Verify the account before long loops

One line, every time, before a multi-hour walk: `claude /status`. If more than one Claude.ai account is signed in, footnote uses whichever Claude Code is active as, and a wrong-account run is silent until the bill. See [auth.md](auth.md).

## Keep the install fresh

Run `fno doctor` when something behaves unexpectedly, and especially after pulling. Skew between the source and the installed binary is a common cause of confusing behavior, including loops that will not finish because a verb the loop depends on silently did nothing. `fno doctor --fix` refreshes a stale install.

## Let it run, and watch for its call for help

The loop is built to survive compactions and provider hiccups; resist babysitting it. The healthy failure mode is a clean escalation: when a session hits something only a human can decide (an architecture call, a missing dependency, access you must grant) it emits a help signal and stops rather than thrashing.

```
<help reason="architecture-decision" evidence="...">what it needs</help>
```

Watch for that, not for the absence of output. A run that escalates cleanly is healthier than one that keeps churning.

## Capture left-out work as you go

The moment you consciously defer a decision or spot an out-of-scope bug, record it so it does not evaporate when the session ends:

```bash
fno carveout add --kind deferred --need "<open question>" "<what + why>"
fno carveout add --kind oos-bug --priority p2 "<what + why>"
```

The retro-triage harvest at merge turns surviving items into backlog nodes.

## One worktree per feature, in the right place

Isolate parallel work in its own worktree so sessions do not fight over the same checkout or shared state. Most worktree pain is placement: create at `~/conductor/workspaces/<repo>/<name>` and run `bash scripts/setup/setup-worktree.sh`, rather than adding a worktree from inside another one or working on a protected branch. The implementation-entry gates will refuse a write on a protected branch and point you at the fix.

## Route ships through the fno verbs

Drive merges and PR operations through `fno pr merge` and the ship gate rather than raw `gh`/`git`. The dangerous-command guards block merge and force-push commands (by their text, even inside an echo or a commit body), so hand-running them tends to get blocked anyway. Writing PR and commit bodies with `--body-file` avoids tripping the same guards.

## Trust external truth for "done"

Completion is decided by the PR being green and reviewed, not by the agent declaring success. If you want a different bar, change `config.review.required_bots` and the wall-clock cap rather than trying to talk the loop into stopping. The completion model is the contract.
