# Worktree mechanics

The parts of the worktree contract that only some sessions reach.
The hook's refusal shape, removal and pruning, and the three enforcement mechanisms.

The always-loaded half lives in [.claude/rules/worktrees.md](../../.claude/rules/worktrees.md).
That file is the authority on placement: where worktrees go, the policy values and their precedence, and the forbidden locations.
This file is the authority on the machinery.

When you edit the hook, remove or prune a worktree, or trace why a location gate fired, read this first.

## The refusal shape

Both creation paths honor `policy = "never"`.
The `WorktreeCreate` hook resolves the policy through `fno worktree policy`, so there is one resolver and no second precedence implementation.

The refusal SHAPE is load-bearing and counter-intuitive.
It differs by payload shape.

**path-present** (CC sends `.path`): the hook pre-creates the directory and is reaped.
A non-zero exit falls back to CC's default flow.
So exiting non-zero creates the very worktree you meant to block.
The supported abort is **exit 0 with empty stdout**, which CC reads as "no successful output".

**name-only** (no `.path`, as EnterWorktree sends): the hook does not pre-create, and `test -d` finds nothing at fire time.
A non-zero exit defers.
That fallback does NOT hold here.
The caller gets a hard failure and no worktree, which is why the rule file says to `git worktree add` first and enter by path.

The gate runs before the hook's own `cd`.
An absent path fails at that `cd` first and takes the fallback branch, so gate placement is load-bearing too.
The gate fails open on anything but an affirmative `never`, because a stale `fno` must not break interactive `claude --worktree`.

An in-session `claude --worktree` spawn is a child (`CLAUDE_CODE_CHILD_SESSION`) and never fires `WorktreeCreate`.
Test with a top-level run.

When `worktrees_base` is set, the two paths still diverge on WHERE.
Autonomous dispatch (`fno worktree ensure`) stays harness-native unless `policy = "external"`.
The hook relocates off `worktrees_base` directly.

## The harness-native fallback

A harness or Codex substrate with no native worktree transition degrades to the Footnote-owned `<state_dir>/worktrees` fallback, normally `~/.fno/worktrees`.
That fallback is Footnote's own allocation.
It does not inherit an external allocator configured by `worktrees_base`, so a repo that sets the base still lands there under `harness-native`.
For that reason `fno worktree ensure` requires `--harness` and never guesses the substrate.

## Removal

```bash
bash scripts/setup/archive-worktree.sh <name|path>   # checks: clean tree, no unpushed commits, no live session
```

Flags: `--force`, `--yes` (skip kill prompt), `--delete-branch`.
Or use `git worktree remove <path>`.
NEVER `rm -rf` a worktree, which leaves dangling refs.

Post-merge pruning is automated.
`/fno:pr merged` archives the PR's worktree, and `fno worktree cleanup --merged --apply` sweeps landed ones.

## Enforcement

Three mechanisms share one read-only verdict helper, `hooks/helpers/check-impl-location.sh`.
It emits `verdict=ok|canonical-protected` plus a nested-worktree advisory, and always exits 0.

- **SessionStart heads-up** (`hooks/session-start.sh`): on the canonical protected branch, it prints a non-blocking note.
- **Implementation-entry refusal** (`/target`, `/do`, `/fix`): on `canonical-protected` these refuse before the first write. The escape is `TARGET_LOCATION_OK=main-acknowledged`.
- **Config-driven relocation** (`hooks/worktree-setup.sh`): refuses outright on `policy = "never"`. With `worktrees_base` set, it relocates `claude --worktree` to `<worktrees_base>/<repo>/<name>`. With the knob unset, it leaves the placement harness-native. `scripts/setup/worktree-create-hook.sh` is the user-global wiring for non-footnote repos and does the same, reading its base from config.

Wire exactly one `WorktreeCreate` hook per repo.
The plugin hook and a user-global one merge across settings levels and race each other.
For non-footnote repos, wire `scripts/setup/worktree-create-hook.sh` into `~/.claude/settings.json` and leave the plugin hook out.
