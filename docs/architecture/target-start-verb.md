# `fno do target start` — one-verb worktree cold-start

## Why

A background `/target` cold-start has to isolate itself before building. Done by hand that is five non-obvious moves across three competing mechanisms (harness `EnterWorktree`, raw `git worktree add`, the skill's attended worktree offer), and two of the moves are silent killers whose fix used to live only in agent memory:

- `.fno` arrives as a **whole-dir symlink** to canonical, so `fno do target init` refuses on what looks like a stale manifest. The fix is `rm .fno && mkdir .fno && bash scripts/setup/setup-worktree.sh`.
- The worktree base is **behind `origin/main`** (branched off local HEAD), so the eventual PR shows phantom deletions of unrelated work, caught only at PR time. The fix is to branch off `origin/main`, never local HEAD.

`fno do target start <node>` collapses all of it into one idempotent verb with a printed receipt, so a memory-less agent (OSS, or a weaker model) succeeds without knowing the folklore.

## What it composes

It does not reimplement worktree mechanics; it sequences pieces that already exist:

1. **Create / reuse the worktree off `origin/main`** via `fno workspace worktree ensure`. That verb branches off `origin/main` (never local HEAD) and reuses an existing worktree idempotently. It refuses to nest inside a linked worktree, and prints the worktree path on stdout.
2. **Heal `.fno`** — if it arrived as a whole-dir symlink, `rm` + `mkdir` it — then link shared state via `worktree.py`'s `_run_setup_worktree_hook` (the setup-worktree.sh runner that the `shellout-drift` gate explicitly exempts).
3. **Init the session from the worktree** via `fno do target init`, which writes the immutable manifest and claims the node exactly once. `start` re-uses that one-call claim rather than claiming separately.
4. **Print a receipt:** `worktree=<path>  .fno=healed|ok  base=origin/main behind=<n>  node=claimed`. The base field carries a MEASURED distance. `start` fetches the remote branch first. `rev-list --count HEAD..origin/main` reads the LOCAL ref, and a stale ref answers 0 for a branch dozens of commits behind. When the fetch or the count fails, the field says `behind=unmeasured:<why>`. Never a silent zero. The whole receipt is whitespace-separated `key=value` tokens. So `<why>` is one hyphenated slug, never a parenthetical. `behind=unmeasured (fetch timed out)` splits into three tokens carrying no key.

## Idempotency

- Run from **inside a valid (linked) worktree** → no-op: `already isolated at <path>; nothing created`. It never nests a worktree inside a worktree.
- When the worktree **already has a manifest**, a re-run from canonical skips init (the manifest is write-once). It reports `node=already-claimed holder=<holder> state=<state>`, read from the live claim lockfile. It never double-claims. This path does NO network work. Its base field reads `behind=unmeasured:idempotent-path-does-no-network`. An idempotent re-run that was pure-local must not pay a fetch. Naming what it did not measure costs less than measuring it.

## Gate-safety

`start` lives in `cli/src/fno/target_cli.py`, which the `shellout-drift` guard scans. It adds no new repo-root bash shell-out: it exec's `fno` (its own subcommands) for ensure + init, and reaches `setup-worktree.sh` only through the exempt `worktree.py` runner. The guard stays green.

## Placement

`fno workspace worktree ensure` lands the worktree at the conductor location (`~/conductor/workspaces/<repo>/<name>`), so `start` inherits that placement. See [.claude/rules/worktrees.md](../../.claude/rules/worktrees.md) for the full worktree-location contract.
