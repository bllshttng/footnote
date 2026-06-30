# `fno target start` — one-verb worktree cold-start

## Why

A background `/target` cold-start has to isolate itself before building. Done by hand that is five non-obvious moves across three competing mechanisms (harness `EnterWorktree`, raw `git worktree add`, the skill's attended worktree offer), and two of the moves are silent killers whose fix used to live only in agent memory:

- `.fno` arrives as a **whole-dir symlink** to canonical, so `fno target init` refuses on what looks like a stale manifest. The fix is `rm .fno && mkdir .fno && bash scripts/setup/setup-worktree.sh`.
- The worktree base is **behind `origin/main`** (branched off local HEAD), so the eventual PR shows phantom deletions of unrelated work, caught only at PR time. The fix is to branch off `origin/main`, never local HEAD.

`fno target start <node>` collapses all of it into one idempotent verb with a printed receipt, so a memory-less agent (OSS, or a weaker model) succeeds without knowing the folklore.

## What it composes

It does not reimplement worktree mechanics; it sequences pieces that already exist:

1. **Create / reuse the worktree off `origin/main`** via `fno worktree ensure` (x-73ca). That verb branches off `origin/main` (never local HEAD), reuses an existing worktree idempotently, refuses to nest inside a linked worktree, and prints the worktree path on stdout.
2. **Heal `.fno`** — if it arrived as a whole-dir symlink, `rm` + `mkdir` it — then link shared state via `worktree.py`'s `_run_setup_worktree_hook` (the setup-worktree.sh runner that the `shellout-drift` gate explicitly exempts).
3. **Init the session from the worktree** via `fno target init`, which writes the immutable manifest and claims the node exactly once. `start` re-uses that one-call claim rather than claiming separately.
4. **Print a receipt:** `worktree=<path>  .fno=healed|ok  base=origin/main  node=claimed`.

## Idempotency

- Run from **inside a valid (linked) worktree** → no-op: `already isolated at <path>; nothing created`. It never nests a worktree inside a worktree.
- Re-run from canonical when the worktree **already has a manifest** → skip init (the manifest is write-once) and report `node=already-claimed`; it never double-claims.

## Gate-safety

`start` lives in `cli/src/fno/target_cli.py`, which the `shellout-drift` guard scans. It adds no new repo-root bash shell-out: it exec's `fno` (its own subcommands) for ensure + init, and reaches `setup-worktree.sh` only through the exempt `worktree.py` runner. The guard stays green.

## Placement

`fno worktree ensure` lands the worktree at the conductor location (`~/conductor/workspaces/<repo>/<name>`), so `start` inherits that placement. See [.claude/rules/worktrees.md](../../.claude/rules/worktrees.md) for the full worktree-location contract.
