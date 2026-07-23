# Worktree convention

The single place that says where git worktrees go and what to do after creating one. Loaded every session via `AGENTS.md`. Skill defaults that place worktrees elsewhere lose to this rule.

## The rule

**The worktree root is config-driven via `config.paths.worktrees_base`. Set nothing and the defaults work.**

- **Unset (OSS-neutral default):** harness-native `<repo>/.claude/worktrees/<name>` (gitignored, search-clean). No config needed.
- **`config.paths.worktrees_base: <dir>`:** worktrees land at `<dir>/<repo>/<name>` (`<repo>` = `basename $(git rev-parse --show-toplevel)`).
- **`worktree.use_conductor_canonical: true` is DEPRECATED:** behaves as `worktrees_base = ~/conductor/workspaces`; prefer the single `worktrees_base` knob. Honored by the `WorktreeCreate` hook, `cli/src/fno/worktree.py`, and `cli/src/fno/worktree_paths.py` (whose own neutral default is `~/.fno/worktrees/{proj}-{name}`).

After `git worktree add`, always run the setup script from inside the worktree:

```bash
git worktree add <location>/<name> <branch>
cd <location>/<name>
bash scripts/setup/setup-worktree.sh
```

The setup script links shared state from canonical: `internal/` (absolute symlink to the vault), per-file `.fno/` state (config, tasks, ledger, codemap, wake-signals), the gitignored `.claude/` subdirs (agents/commands/skills/settings.local.json/scheduled_tasks/plans/local notes), and the `.agents/`/`.codex/`/`.gemini/` per-CLI config roots (skip-if-missing). It warns and skips any real (non-symlink) file at a target; it never overwrites real state. Tracked files come from git checkout, never copied.

**Enter the worktree in-session.** A shell `cd` does not persist across tool calls; a `/target` cold-start reads the worktree path from the `fno target start` receipt and calls the harness **EnterWorktree** tool with that path. Any path in `git worktree list` is enterable on first entry.

## Per-project worktree policy

Every code-payload dispatch routes through `fno worktree ensure`, which resolves a `worktree` policy.
Precedence: per-project `work.workspaces.<slug>.projects[].worktree` > global `config.worktree.policy` > built-in `harness-native`.

- **`never`** - launch in place on the canonical checkout (for projects whose working tree IS the product, e.g. an Obsidian vault). ensure prints the repo root, exit 0; callers skip `setup-worktree.sh`; the location gate treats the protected branch as `ok`.
- **`harness-native`** (default) - the harness's own location: claude lands at `<repo>/.claude/worktrees/<name>`, **always**, ignoring `worktrees_base`. A harness with no native mechanism degrades to `external`; ensure needs `--harness` and never guesses.
- **`external`** - fno-managed at `<worktrees_base>/<repo>/<name>`.

The per-project policy outranks `worktrees_base`: setting the base alone does NOT relocate a claude default; you must also set `worktree.policy = "external"`. "conductor" is a `worktrees_base` value, not a policy value. A config parse error or out-of-enum value REFUSES creation (fail closed): ensure exits non-zero with empty stdout so the caller never auto-isolates on a misconfig.

Note the two creation paths diverge when `worktrees_base` is set: autonomous dispatch (`fno worktree ensure`) stays harness-native unless `policy = "external"`; the `claude --worktree` `WorktreeCreate` hook relocates off `worktrees_base` directly and does not read the policy.

## Removal

```bash
bash scripts/setup/archive-worktree.sh <name|path>   # checks: clean tree, no unpushed commits, no live session
```

Flags: `--force`, `--yes` (skip process-kill prompt), `--delete-branch`. Plain alternative: `git worktree remove <path>`; NEVER `rm -rf` (dangling refs). Pruning after merge is automated: `/fno:pr merged` archives the merged PR's worktree, and `fno worktree cleanup --merged --apply` sweeps already-landed worktrees; you rarely prune by hand.

## Forbidden locations (regardless of config)

- `~/.warp/worktrees/...` (setup script never runs there).
- `<repo>/worktrees/` or any non-`.claude` path inside the checkout.
- `../<name>` or any sibling-of-canonical path.

Exception: `/speculate` keeps its own `.claude/worktrees/<name>` placement even when `worktrees_base` is set (scoped, documented; do not generalize).

## Enforcement

Three mechanisms share one read-only verdict helper, `hooks/helpers/check-impl-location.sh` (`verdict=ok|canonical-protected` + nested-worktree advisory; always exits 0):

- **SessionStart heads-up** (`hooks/session-start.sh`): non-blocking note when on the canonical protected branch.
- **Implementation-entry refusal** (`/target`, `/do`, `/fix`): on `canonical-protected` they refuse before the first write, with the `TARGET_LOCATION_OK=main-acknowledged` escape.
- **Config-driven relocation:** the `WorktreeCreate` hook (`hooks/worktree-setup.sh`) relocates `claude --worktree` to `<worktrees_base>/<repo>/<name>` when the knob is set; unset leaves harness-native.

Do not wire BOTH the plugin `WorktreeCreate` hook AND a user-global one for the same repo (hooks merge across levels and race). For non-footnote projects, wire `scripts/setup/worktree-create-hook.sh` into `~/.claude/settings.json` `WorktreeCreate`.

## Override semantics

An explicit in-conversation user request for a different path outranks this rule; note that `.fno/` state links will not exist there. Do not solicit overrides.
