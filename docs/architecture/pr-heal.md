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

The default is a dry run. `--apply` fixes, commits and pushes. `--all` reports every red open PR and refuses `--apply`, because a remedy needs the PR's own worktree and this process has one cwd.

## How to add a signature

Make two edits, both in `crates/fno-agents/src/heal.rs`. Add one row to the `SIGNATURES` table. Add one test that carries a real log excerpt.

Write the pattern against a log you fetched. The first three rules assumed the output, and they matched nothing. The fmt job is one check whose name carries `(pinned)`, not the crate. Ruff prints its code above the location, not beside it. Pytest names `tests/...`, because it runs from `cli`. Job logs also carry a timestamp on every line, so the classifier removes it first.

A smoke shard needs more care. It runs dozens of guards, and every guard announces itself on success. So a prefix scan names a guard that passed. The shard runner's own fail-fast line names the failing step, and heal reads that line first.

The checks read is REST for a reason. `gh pr checks` is GraphQL, and the quota broker routes every GraphQL PR read away. A heal built on it can never run.
