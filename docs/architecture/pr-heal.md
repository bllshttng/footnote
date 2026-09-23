# `fno do pr heal`

Everything after a push already had a reader. `fno do pr status` names the failing check and step. `fno do pr logs` spools its log. The stop gate knows whether main's HEAD is red on the same check. Nothing acted on what they read. So a red check cost a hand-driven fix-and-repush round every time. `heal` is the actor.

## What it does

heal reads the PR's failing checks over REST, gets each failing job's log, matches it against a signature table, and applies the mechanical fix. `--playbook` prints the table. This page keeps no copy of the table, because a doc copy drifts and the verb's own output cannot.

heal fixes three classes on its own. `rustfmt-drift` runs the pinned `cargo fmt` in each crate that rustfmt named. `ruff-lint` runs `ruff check --fix` over exactly the scope the gate reads. `closure-trailer` adds the generated `Backlog-Closure` trailer to the PR body. That edit re-fires the workflow through its `edited` trigger, so it needs no push.

## The rerun-before-real rule

A red whose verdict a rerun can change is rerun once before anyone calls it real. `cancelled` gets a full `gh run rerun <run>`: it reached no verdict, so the rerun is how it reaches one. The test-shaped classes (`pytest`, `cargo-test`, `shard-rollup`, `smoke-step`) and anything `unknown` get `gh run rerun <run> --failed`. A failure that passes on a second run was never the PR's defect. The deterministic classes (`rustfmt-drift`, `ruff-lint`, `mypy`, `closure-trailer`, `review-gate`, `guard-script`) never rerun: they fail identically every time, and the rerun is pure spend. The dedup key is `(head sha, run id)`, not the check name, because nine failing checks can share one workflow run and one `--failed` rerun covers them all. A second red on the same pair is the verdict: it escalates saying it failed twice on the same sha. A rerun that comes back green lands one `pr_heal_flake` row in the journal. When the log named a failing test, the key is that test. Otherwise the key is the check name. A key's third row files one backlog node. A flake that reruns green forever stays visible by construction. The `--playbook` table carries a `rerunnable` column, and it is the single source for the classification.

Every other signature escalates with the command that reproduces it locally.

## The two rules

**One push, never over a run in flight.** heal commits the fixes once, then reads the checks again. A check still in flight means the commit stays local and the verb exits 2, because a push cancels a run in flight. An unreadable second read holds the commit too. Unreadable is not settled.

**A failure inherited from main is never counted against the PR.** heal reports it and changes nothing. A fix applied here puts main's problem in someone else's diff.

`--apply` also refuses unless two things are true. This checkout must be the PR's own branch. Its tree must be clean. So a remedy never lands in a worktree that was not the target.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | No red check belongs to this PR |
| 1 | Escalations remain. The report names each repro |
| 2 | A run is in flight. heal kept the commit local and did not push |
| 3 | Wrong branch or a dirty worktree. Nothing ran |
| 4 | A read failed |
| 127 | The `fno-agents` binary was not found |

The default is a dry run. `--apply` fixes, commits and pushes. `--all` reports every red open PR. `--all --apply` is the drive loop, described next.

## The drive loop

`--all --apply` iterates every open PR with something red of its own and heals each one. The loop lives in Rust beside the classifier. Each PR is healed from its own worktree. The worktree is located by matching the PR's head ref against `git worktree list`. A PR with no worktree is named and skipped. The loop never clones a repo on its own.

Four refusals gate it:

1. **Claim free.** A PR whose branch names a node with a live or suspect claim is skipped as `claim_held`. A healer pushing under a live worker is the two-writers failure. The claim lockfile is the read, never a stored pid.
2. **Known signature.** Only a failing check the signature table recognizes is healed. An unknown signature becomes one operator question through `fno inbox outstanding ask`, deduplicated on a marker so a 600s tick cannot re-ask it.
3. **One push per PR per cycle.** Each PR is visited once per invocation. The single-PR rules above hold inside it. A run in flight keeps the commit local.
4. **Inherited failures are named and skipped.** A check red on `origin/main` too is main's problem. It is never fixed on the branch.

Two rebase triggers run ahead of classification, because a push restarts CI and the old sha's checks are dead once it lands. A PR whose `mergeable` is `false` is conflicting and stuck by definition. A PR holding the `merge-slot:<base>` claim (the one the merge sweep named as next) is behind by the slot's own verdict. Both run `fno do pr push` from the PR's own worktree. They fetch origin/main. Merge-bearing branches use merge. Other branches use rebase. They preflight, push once, lease against the fetched remote head, and emit one receipt. A conflicting PR whose node a live worker holds is never rebased. The claim refusal runs first. A run is capped at six rebases, so a leaked trigger stops the loop instead of restarting CI on the whole fleet. A conflict files one deduplicated inbox question against the branch's node. It names the conflicting files and applicable door. Rebase conflicts use `fno do pr rebase <n>`. Merge-bearing branches use merge origin/main by hand. Any other push failure falls through to the normal heal path, because a rebase that did not run says nothing about the PR.

One invocation emits one `pr_heal_tick` row into the global `~/.fno/events.jsonl`. The row carries the counts: PRs seen, healed, `rebased`, `reran`, skipped by reason, unknown signatures, escalations, the PRs acted on, and `duration_s`. The `rerun_keys` field is the once-per-(sha, run id) guard's ledger, read back on the next run. Every PR also gets one `pr_heal_pr` row per run. It carries the action taken and the blocking reason, so no PR sits for more than one tick without a receipt saying why. `fno doctor event find --field type=pr_heal_tick --since 24h` reads it. `--all --apply --dry-run` rehearses every refusal and prints the plan without touching a worktree or the inbox.

## Detached from the tick

The tick's heal phase never runs the loop inside its own slice. Armed, the phase calls `pr-heal --all --apply --detach`, and the binary spawns itself (same args minus `--detach`) as a new session with stdio on `/dev/null` and returns 0 at once. The child's pid goes to `<state dir>/pr-heal.<root path>.pid`; a pid file naming a live process (EPERM counts alive) makes the next tick answer `skip_reason=in_flight` instead of spawning a second loop. Every detach decision emits one `control_plane_tick` arm row (`arm=heal`, `acted`, `skip_reason`), and the tick's own gate answers (`unarmed`, `no_binary`, `no_roots`) land in the same row shape, so the journal and the status line agree on why nothing ran.

The pid file is the in-flight guard, so a stale file costs ticks only while the pid it names answers `kill(pid, 0)`: a genuinely dead pid is overwritten on the next spawn. The ceiling: a pid recycled by an unrelated long-lived process reads as alive, and the root keeps answering `in_flight` until that process exits.

## The status line

`fno do pr watch status` prints one `Heal:` line, rendered by `pr-heal --status` (the Python side passes only the arm bit and the journal):

```
Heal: armed; last run 2026-09-17T12:00:00Z (12m ago); healed 1, rebased 2, reran 1, escalated 3; acted on PR 2155, 2162; in-flight none
```

Unarmed it names the arm command. Armed with no `pr_heal_tick` row it reads `Heal: armed; never ran`. `fno do pr watch install` and `refresh` print the same line, so a fresh install shows the arm state.

## Arming

`fno config set auto_heal.enabled true`. The key defaults to false; the measurement that justified arming was taken 2026-09-16 over 15 red open PRs: 1 push, 3 cancelled-run reruns, 11 escalations, 0 failures inherited from main. One tick interval later the journal holds a `pr_heal_tick` row and status prints `last run`.

## How to add a signature

Make two edits, both in `crates/fno-agents/src/heal.rs`. Add one row to the `SIGNATURES` table. Add one test that carries a real log excerpt.

Write the pattern against a log you fetched. The first three rules assumed the output, and they matched nothing. The fmt job is one check whose name carries `(pinned)`, not the crate. Ruff prints its code above the location, not beside it. Pytest names `tests/...`, because it runs from `cli`. Job logs also carry a timestamp on every line, so the classifier removes it first.

A smoke shard needs more care. It runs dozens of guards, and every guard announces itself on success. So a prefix scan names a guard that passed. The shard runner's own fail-fast line names the failing step, and heal reads that line first.

The checks read is REST for a reason. `gh pr checks` is GraphQL, and the quota broker routes every GraphQL PR read away. A heal built on it can never run.
