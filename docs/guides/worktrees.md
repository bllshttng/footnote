# Worktrees

footnote runs each feature in its own git worktree, off the main branch.
This keeps the main checkout clean and lets more than one feature ship in parallel.
This guide is the short, hands-on version.
The full policy contract is [.claude/rules/worktrees.md](../../.claude/rules/worktrees.md), and it stays canonical.
When this guide and that rule disagree, the rule wins.

## Why worktrees

A worktree is a second checkout of the same repo on its own branch.
You edit, commit, and open a PR from inside it.
The main checkout never sees your uncommitted changes.
Two features in two worktrees do not collide.

## What happens by default

`target` creates a worktree before it writes code.
`do` expects you to already be in one.
For a normal target run, you do not create it yourself.
The location depends on your worktree policy, which is config-driven.

Check your resolved policy:

```bash
fno workspace worktree policy --repo . --harness claude
```

Pass your harness, or the result degrades to `external`.
It prints one of three values: `never` (work in place), `harness-native` (the default for Claude), or `external` (a configured base).
The rule file defines each one and what sets them.

## Start one by hand

You rarely need to.
When you want a scratch branch outside a target run, reach for this.

```bash
fno workspace worktree ensure --repo . --name my-feature
```

It prints the worktree path and creates the branch `feature/my-feature`.
Move into it, then link shared state:

```bash
cd <printed-path>
bash scripts/setup/setup-worktree.sh
```

The setup script links config and the agent files from the main checkout.
Work from there.

## Clean up after a merge

Leave a merged worktree around and the list grows fast.
Two ways to remove one:

```bash
fno workspace worktree cleanup --merged --apply     # reap every landed worktree, with safety checks
fno workspace worktree archive <name>               # force-remove one worktree, keep its branch
```

Never remove a worktree with `rm -rf`.
That leaves dangling git refs.
The cleanup verb runs the safety checks for you: clean tree, no unpushed commits, no live session.
The archive verb force-removes with no checks, so confirm the tree is clean first.

## Who holds what

A worktree is tied to a work claim.
Before you do manual work in a node, check no other session already holds it:

```bash
fno agents claim status node:<id>
fno agents claim list
```

A `live` claim means a session is active on it.
A `stale` claim means the holder died.
Starting work through `target` reclaims a stale claim for you.

## Where the full rules live

Every detail (policy values, refusal shapes, forbidden locations) lives in the rule file:
[.claude/rules/worktrees.md](../../.claude/rules/worktrees.md).
This guide intentionally does not repeat it.
