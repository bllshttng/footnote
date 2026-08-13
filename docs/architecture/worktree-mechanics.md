# Worktree mechanics

The parts of the worktree contract that only some sessions reach: the `WorktreeCreate` hook's refusal shape, removal and pruning, and the three enforcement mechanisms.

The always-loaded half (where worktrees go, the policy values and their precedence, forbidden locations) lives in [.claude/rules/worktrees.md](../../.claude/rules/worktrees.md). That file is the authority on placement; this one is the authority on the machinery.

Read this when you are editing the hook, removing or pruning a worktree, or tracing why a location gate fired.

## The refusal shape

Both creation paths honor `policy = "never"`: the `WorktreeCreate` hook resolves the policy through `fno worktree policy` (one resolver, no second precedence implementation) and refuses.

The refusal SHAPE is load-bearing and counter-intuitive, and it differs by payload shape:

- **path-present** (CC sends `.path`): pre-creates and is reaped. A **non-zero exit falls back to CC's default flow**, so exiting non-zero creates the very worktree you meant to block. The supported abort is **exit 0 with empty stdout** ("no successful output").
- **name-only** (no `.path`, e.g. EnterWorktree): does NOT pre-create (`test -d` absent at fire) and defers non-zero, but that fallback does NOT hold here. The caller gets a hard failure and no worktree, which is why the rule file says to `git worktree add` first and enter by path.

The gate runs before the hook's own `cd` (an absent path would fail there first and take the fallback branch), and fails open on anything but an affirmative `never`, since a stale `fno` must not break interactive `claude --worktree`.

An in-session `claude --worktree` spawn is a child (`CLAUDE_CODE_CHILD_SESSION`) and never fires `WorktreeCreate`; test with a top-level run.

The two paths still diverge on WHERE, when `worktrees_base` is set: autonomous dispatch (`fno worktree ensure`) stays harness-native unless `policy = "external"`, while the hook relocates off `worktrees_base` directly.

## The harness-native fallback

A harness or Codex substrate with no native worktree transition degrades to the Footnote-owned `<state_dir>/worktrees` fallback, normally `~/.fno/worktrees`. That fallback is Footnote's own allocation and does not inherit an external allocator configured by `worktrees_base`, so a repo that sets the base still lands there under `harness-native`. `fno worktree ensure` requires `--harness` for this reason and never guesses the substrate.

## Removal

```bash
bash scripts/setup/archive-worktree.sh <name|path>   # checks: clean tree, no unpushed commits, no live session
```

Flags: `--force`, `--yes` (skip kill prompt), `--delete-branch`. Or `git worktree remove <path>`; NEVER `rm -rf` (dangling refs).

Post-merge pruning is automated: `/fno:pr merged` archives the PR's worktree; `fno worktree cleanup --merged --apply` sweeps landed ones.

## Enforcement

Three mechanisms share one read-only verdict helper, `hooks/helpers/check-impl-location.sh` (`verdict=ok|canonical-protected` plus a nested-worktree advisory; always exits 0):

- **SessionStart heads-up** (`hooks/session-start.sh`): non-blocking note when on the canonical protected branch.
- **Implementation-entry refusal** (`/target`, `/do`, `/fix`): on `canonical-protected` they refuse before the first write, with the `TARGET_LOCATION_OK=main-acknowledged` escape.
- **Config-driven relocation:** the `WorktreeCreate` hook (`hooks/worktree-setup.sh`) refuses outright on `policy = "never"`, else relocates `claude --worktree` to `<worktrees_base>/<repo>/<name>` when the knob is set; unset leaves harness-native. `scripts/setup/worktree-create-hook.sh` (user-global wiring for non-footnote repos) does the same, reading its base from config.

Wire exactly one `WorktreeCreate` hook per repo. The plugin hook and a user-global one merge across settings levels and race each other, so for non-footnote repos wire `scripts/setup/worktree-create-hook.sh` into `~/.claude/settings.json` and leave the plugin hook out.
