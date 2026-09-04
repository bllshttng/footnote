# `fno do pr heal`

Everything after a push already had a reader. `fno do pr status` names the failing check and step. `fno do pr logs` spools its log. The stop gate knows whether main's HEAD is red on the same check. Nothing acted on what they read. So a red check cost a hand-driven fix-and-repush round every time. `heal` is the actor.

## What it does

heal reads the PR's failing checks over REST, gets each failing job's log, matches it against a signature table, and applies the mechanical fix. `--playbook` prints the table. This page keeps no copy of the table, because a doc copy drifts and the verb's own output cannot.

heal fixes three classes on its own. `rustfmt-drift` runs the pinned `cargo fmt` in each crate that rustfmt named. `ruff-lint` runs `ruff check --fix` over exactly the scope the gate reads. `closure-trailer` adds the generated `Backlog-Closure` trailer to the PR body. That edit re-fires the workflow through its `edited` trigger, so it needs no push. Every other signature escalates with the command that reproduces it locally.

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

`--all --apply` iterates every open PR with something red of its own and heals each one. The loop lives in Rust beside the classifier, and each PR is healed from that PR's own worktree, located by matching the PR's head ref against `git worktree list`. A PR with no worktree is named and skipped; the loop never clones a repo on its own.

Four refusals gate it:

1. **Claim free.** A PR whose branch names a node with a live or suspect claim is skipped as `claim_held`. A healer pushing under a live worker is the two-writers failure. The claim lockfile is the read, never a stored pid.
2. **Known signature.** Only a failing check the signature table recognizes is healed. An unknown signature becomes one operator question through `fno inbox outstanding ask`, deduplicated on a marker so a 600s tick cannot re-ask it.
3. **One push per PR per cycle.** Each PR is visited once per invocation, and the single-PR rules above hold inside it: a run in flight keeps the commit local.
4. **Inherited failures are named and skipped.** A check red on `origin/main` too is main's problem and is never fixed on the branch.

One invocation emits one `pr_heal_tick` row into the global `~/.fno/events.jsonl` carrying the counts: PRs seen, healed, skipped by reason, unknown signatures. `fno doctor event audit --type pr_heal_tick --since 24h` reads it. `--all --apply --dry-run` rehearses every refusal and prints the plan without touching a worktree or the inbox.

## The tick's heal phase

The pr-watch tick (600s launchd cadence) runs the drive loop when `auto_heal.enabled` is set. The key defaults to false and lives with the other tick gates in `config.toml`. The phase sits between `king_wake` and `stranded`, so a PR the healer fixes this tick is not reported stranded in the same breath. The tick starts in `/`, so the phase passes each project root explicitly with `--cwd`.

## How to add a signature

Make two edits, both in `crates/fno-agents/src/heal.rs`. Add one row to the `SIGNATURES` table. Add one test that carries a real log excerpt.

Write the pattern against a log you fetched. The first three rules assumed the output, and they matched nothing. The fmt job is one check whose name carries `(pinned)`, not the crate. Ruff prints its code above the location, not beside it. Pytest names `tests/...`, because it runs from `cli`. Job logs also carry a timestamp on every line, so the classifier removes it first.

A smoke shard needs more care. It runs dozens of guards, and every guard announces itself on success. So a prefix scan names a guard that passed. The shard runner's own fail-fast line names the failing step, and heal reads that line first.

The checks read is REST for a reason. `gh pr checks` is GraphQL, and the quota broker routes every GraphQL PR read away. A heal built on it can never run.
