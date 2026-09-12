# `fno do target start` — one-verb worktree cold-start

## Why

A background `/target` cold-start has to isolate itself before building. Done by hand that is five non-obvious moves across three competing mechanisms (harness `EnterWorktree`, raw `git worktree add`, the skill's attended worktree offer), and two of the moves are silent killers whose fix used to live only in agent memory:

- Project state used to live inside the tree, so a worktree's `.fno` arrived as a whole-dir symlink to canonical. `fno do target init` refused on what looked like a stale manifest. State moved into the repo's space under `~/.fno/spaces/`, there is nothing to symlink, and the heal step is gone.
- The worktree base is **behind `origin/main`** (branched off local HEAD), so the eventual PR shows phantom deletions of unrelated work, caught only at PR time. The fix is to branch off `origin/main`, never local HEAD.

`fno do target start <node>` collapses all of it into one idempotent verb with a printed receipt, so a memory-less agent (OSS, or a weaker model) succeeds without knowing the folklore.

## What it composes

It does not reimplement worktree mechanics; it sequences pieces that already exist:

1. **Create / reuse the worktree off `origin/main`** via `fno agents workspace worktree ensure`. That verb branches off `origin/main` (never local HEAD) and reuses an existing worktree idempotently. It refuses to nest inside a linked worktree, and prints the worktree path on stdout.
2. **Link shared non-fno state** via `worktree.py`'s `_run_setup_worktree_hook` (the setup-worktree.sh runner that the `shellout-drift` gate explicitly exempts). Project state needs no link: the space resolves identically from every worktree.
3. **Init the session from the worktree** via `fno do target init`. It writes the immutable manifest into the worktree's space slice and claims the node exactly once. `start` re-uses that one-call claim rather than claiming separately.
4. **Print a receipt:** `worktree=<path>  base=origin/main behind=<n>  node=claimed`. The base field carries a MEASURED distance. `start` fetches the remote branch first. `rev-list --count HEAD..origin/main` reads the LOCAL ref, and a stale ref answers 0 for a branch dozens of commits behind. When the fetch or the count fails, the field says `behind=unmeasured:<why>`. Never a silent zero. The whole receipt is whitespace-separated `key=value` tokens. So `<why>` is one hyphenated slug, never a parenthetical. `behind=unmeasured (fetch timed out)` splits into three tokens carrying no key.

## Idempotency

- Run from **inside a valid (linked) worktree** → it prints `already isolated at <path>; nothing created` and never nests a worktree inside a worktree. Then it binds the session to this tree instead of returning empty-handed: a foreign live claim parks; the caller's own claim reports `node=already-claimed`; a dead predecessor's claim is re-acquired; a tree with **no manifest at all** falls through to init against the existing tree, so a session that lost its manifest gets its claim back with this one command. Nothing is created in any branch.
- When the worktree **already has a manifest**, a re-run from canonical skips init (the manifest is write-once). It reports `node=already-claimed holder=<holder> state=<state>`, read from the live claim lockfile. It never double-claims. This path does NO network work. Its base field reads `behind=unmeasured:idempotent-path-does-no-network`. An idempotent re-run that was pure-local must not pay a fetch. Naming what it did not measure costs less than measuring it.

## Re-binding an in_review node

The second half of a stranded session's way back lives in init's `in_review` guard: a fresh dispatch on a node with an open PR is refused exactly as before, but a caller standing in the open PR's own worktree, on the PR's own head branch, is **adopted** — init proceeds, stamps `target_adopted_pr: <n>` on the manifest, and prints an `ADOPTED` receipt naming that no new PR may be minted. Any proof that fails or cannot be read refuses as before. The escape hatch an agent may not touch (`TARGET_ALLOW_IN_REVIEW`) stays human-only; the adopt proof is something the agent's own worktree already is.

## Gate-safety

`start` lives in `cli/src/fno/target_cli.py`, which the `shellout-drift` guard scans. It adds no new repo-root bash shell-out: it exec's `fno` (its own subcommands) for ensure + init, and reaches `setup-worktree.sh` only through the exempt `worktree.py` runner. The guard stays green.

## Placement

`fno agents workspace worktree ensure` lands the worktree at the conductor location (`~/conductor/workspaces/<repo>/<name>`), so `start` inherits that placement. See [.claude/rules/worktrees.md](../../.claude/rules/worktrees.md) for the full worktree-location contract.
