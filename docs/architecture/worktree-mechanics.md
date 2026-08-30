# Worktree mechanics

The parts of the worktree contract that only some sessions reach.
The hook's refusal shape, removal and pruning, and the three enforcement mechanisms.

The always-loaded half lives in [.claude/rules/worktrees.md](../../.claude/rules/worktrees.md).
That file is the authority on placement: where worktrees go, the policy values and their precedence, and the forbidden locations.
This file is the authority on the machinery.

When you edit the hook, remove or prune a worktree, or trace why a location gate fired, read this first.

## The refusal shape

Both creation paths honor `policy = "never"`. The `WorktreeCreate` hook resolves the policy through `fno agents workspace worktree policy`, so there is one resolver and no second precedence implementation.

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

When `worktrees_base` is set, the two paths still diverge on WHERE. Autonomous dispatch (`fno agents workspace worktree ensure`) stays harness-native unless `policy = "external"`.
The hook relocates off `worktrees_base` directly.

## Claude Code's worktree Bash isolation

With `worktree.bgIsolation: "worktree"`, Claude Code runs a static analyzer over a worktree-isolated session's Bash commands. That setting is Claude Code's default since 2.1.222. This repo sets it in `.claude/settings.local.json`. The analyzer is Claude Code's own code. Footnote cannot widen or narrow it, and its refusal text is fixed. What footnote owns is the command its hooks and skills tell an agent to run. Those must be shapes the analyzer admits.

The predicate was probed live on CC 2.1.251 from a worktree session. When a command holds anything the analyzer cannot statically resolve, it is refused as too complex to verify. The refused constructs: environment-variable expansion (`echo "$VAR"`), command substitution (`$(...)`), arithmetic expansion (`$((n+1))`), and loops (`while`, `for`). The admitted constructs: plain commands, literal local variables (`i=2; echo "$i"`), pipes, `;`/`&&` compounds, redirects, and `bash /abs/path/script.sh` file indirection.

Two facts agents keep getting wrong. The refusal's mention of the redirect is generic wording, not the trigger. The trigger is the construct the analyzer cannot parse. The analyzer is also NOT a path boundary. An absolute-path write outside the worktree runs ungated, including into the canonical checkout. Footnote's own hooks are that boundary (`worktree-write-protect.sh`, `git-protection.py`).

The sanctioned escapes, in order. Read env with `printenv VAR` instead of `echo "$VAR"`. Wait on CI or a review with `fno do pr wait <N> --until settled|review` instead of a hand-rolled poll loop. An inline `while`/`$(...)` watcher is refused, so no fno surface can instruct one. For a genuinely complex one-liner, put it in a file and run `bash <file>`.



A harness or Codex substrate with no native worktree transition degrades to the Footnote-owned `<state_dir>/worktrees` fallback, normally `~/.fno/worktrees`. That fallback is Footnote's own allocation. It does not inherit an external allocator configured by `worktrees_base`, so a repo that sets the base still lands there under `harness-native`. For that reason `fno agents workspace worktree ensure` requires `--harness` and never guesses the substrate.

## Removal

```bash
fno agents workspace worktree archive <name|path>           # the public guarded path
bash scripts/setup/archive-worktree.sh <name|path>   # the shared implementation
```

The CLI and compatibility lifecycle entry delegate to the script above. They expose `--force`, `--yes` (skip kill prompt), and `--delete-branch` without copying the checks.

Without `--force`, archival refuses on dirty state, unpushed commits, live sessions, unreadable process snapshots, failed salvage, app ownership, canonical checkout, and removal-time changes.

With `--force`, the script measures and prints every dirty path. It prints each unpushed commit's abbreviated SHA and subject. It prints positive live-session evidence before removal. It still refuses unverifiable evidence. Its final receipt distinguishes discarded worktree state from preserved or deleted branch data.

NEVER `rm -rf` a worktree, which leaves dangling refs.

Post-merge pruning is automated. When the ritual runs from outside the merged worktree, `/fno:pr merged` archives the PR's worktree directly. When it stands inside the merged worktree (the usual worker case), it defers and MINTS a reap order. The order is a TTL claim (`reap:pr-<n>`, 7d) that only a gh-confirmed MERGED state can create. The daemon's daily worktree sweep runs its pass with `--apply` while any order stands, report-only otherwise: a timer tick alone still removes nothing. The sweep's own guards (reapable, live claim, rooted processes) decide tree by tree, so an order never forces a protected tree - it expires instead. `fno agents workspace worktree cleanup --merged` (dry-run by default, both removal modes) sweeps landed ones by hand with `--apply`.

Every removal emits one `worktree_removed` event row (path, caller, claim read, reason). The row mirrors to the machine-global journal. Before x-60de no removal path emitted anything, so a lost tree left no attributable evidence.

## Commit-time salvage refs

`scripts/setup/setup-worktree.sh` installs a shared `post-commit` dispatcher that runs the committing worktree's `hooks/worktree-salvage-ref.sh`. Every commit advances a local `refs/fno/salvage/<worktree>` ref so a detached or provider-killed worktree stays recoverable without a network dependency.

Remote mirroring is off by default because the commit can still be work in progress. A repository can explicitly enable the detached best-effort mirror with `git config --local fno.salvageRemoteMirror true`. Disable it again with `git config --local --unset fno.salvageRemoteMirror`. The local salvage ref remains active in both cases, and a remote failure never blocks the commit.

## Enforcement

Three mechanisms share one read-only verdict helper, `hooks/helpers/check-impl-location.sh`. It emits `verdict=ok|canonical-protected` plus a nested-worktree advisory, and always exits 0.

- **SessionStart heads-up** (`hooks/session-start.sh`): on the canonical protected branch, it prints a non-blocking note.
- **Implementation-entry refusal** (`/target`, `/execute`, `/fix`): on `canonical-protected` these refuse before the first write. The escape is `TARGET_LOCATION_OK=main-acknowledged`.
- **Config-driven relocation** (`hooks/worktree-setup.sh`): refuses outright on `policy = "never"`. With `worktrees_base` set, it relocates `claude --worktree` to `<worktrees_base>/<repo>/<name>`. With the knob unset, it leaves the placement harness-native. `scripts/setup/worktree-create-hook.sh` is the user-global wiring for non-footnote repos and does the same, reading its base from config.

Wire exactly one `WorktreeCreate` hook per repo.
The plugin hook and a user-global one merge across settings levels and race each other.
For non-footnote repos, wire `scripts/setup/worktree-create-hook.sh` into `~/.claude/settings.json` and leave the plugin hook out.
