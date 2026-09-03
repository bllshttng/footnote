# `fno do pr heal`

Everything after a push already had a reader. `fno do pr status` names the failing check and step, `fno do pr logs` spools its log, and the stop gate knows whether main's HEAD is red on the same check. Nothing acted on what they read, so a red check cost a hand-driven fix-and-repush round every time. `heal` is the actor.

## What it does

Reads the PR's failing checks over REST, fetches each failing job's log, matches it against a signature table, and applies the mechanical fix. `--playbook` prints the table; there is no copy of it here, because a doc copy drifts and the verb's own output cannot.

Three classes are fixed automatically. `rustfmt-drift` runs the pinned `cargo fmt` in each crate rustfmt named. `ruff-lint` runs `ruff check --fix` over exactly the scope the gate reads. `closure-trailer` appends the generated `Backlog-Closure` trailer to the PR body, which re-fires the workflow through its `edited` trigger and needs no push. Every other signature escalates with the command that reproduces it locally.

## The two rules

**One push, never over a run in flight.** Fixes are committed once, then the checks are re-read. A check still running means the commit stays local and the verb exits 2, because pushing cancels an in-flight run. An unreadable re-read holds the commit too: unreadable is not settled.

**A failure inherited from main is never counted against the PR.** It is reported and left alone. Fixing it here would put main's problem in someone else's diff.

`--apply` also refuses unless this checkout is the PR's own branch with a clean tree, so a remedy never lands in a worktree that was not the target.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Nothing red that belongs to this PR |
| 1 | Escalations remain; the report names each repro |
| 2 | A run is in flight; the fix is committed but not pushed |
| 3 | Wrong branch or a dirty worktree; nothing ran |
| 4 | A read failed |
| 127 | The `fno-agents` binary was not found |

Default is a dry run. `--apply` fixes, commits and pushes. `--all` reports every red open PR and refuses `--apply`, because applying needs the PR's own worktree and this process has one cwd.

## Adding a signature

Two edits, both in `crates/fno-agents/src/heal.rs`: one row in the `SIGNATURES` table, and one test carrying a real log excerpt. Write the pattern against a log you actually fetched. Three of the original rules were written against assumed output and matched nothing: the fmt job is one check whose name carries `(pinned)` rather than the crate, ruff prints its code above the location rather than beside it, and pytest names `tests/...` because it runs from `cli`. Job logs also prefix every line with a timestamp, which is why the classifier strips it before matching.

The checks read is REST on purpose. `gh pr checks` is GraphQL, and the quota broker routes every GraphQL PR read away, so a heal built on it could never run.
