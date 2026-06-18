# Worktree convention

The single place that says where git worktrees go for this repo and what to do after creating one. Loaded by every Claude / Codex / footnote / Hermes session via `AGENTS.md` (which `CLAUDE.md` and `GEMINI.md` import). Skill defaults (e.g. `superpowers:using-git-worktrees` and `fno:git-worktrees` placing worktrees in `.claude/worktrees/`) lose to this rule because user-level instructions outrank skill defaults.

## The rule

**Create worktrees at `~/conductor/workspaces/abilities/<name>` and nowhere else.**

After `git worktree add`, run the canonical setup script:

```bash
git worktree add ~/conductor/workspaces/abilities/<name> <branch>
cd ~/conductor/workspaces/abilities/<name>
bash scripts/setup/setup-worktree.sh
```

The setup script links the canonical `internal/`, the project-level `.fno/` state (settings, tasks, ledger, inbox, wake-signals, codemap), the `.claude/` subdirs (`agents`, `commands`, `skills`, `settings.local.json`, `scheduled_tasks*`), and the `.agents/` provider/agent config. It auto-resolves the canonical via `CANONICAL` env var, then `CONDUCTOR_ROOT_PATH`, then `git rev-parse --git-common-dir`, then `$HOME/code/me/abilities`. See `scripts/setup/setup-worktree.sh` for the full contract.

To remove a worktree, use the archive script:

```bash
bash scripts/setup/archive-worktree.sh <name|path>
```

It enforces strict pre-removal checks (clean working tree, no unpushed commits, no live target session), prompts before SIGTERM'ing any process rooted in the worktree path, runs `git worktree remove` + `git worktree prune`, and preserves the branch. Flags: `--force` (skip checks), `--yes` (skip process-kill prompt), `--delete-branch` (drop the branch with `git branch -D` after removal). The plain alternative is `git worktree remove <path>`; never use `rm -rf` (leaves dangling refs in `.git/worktrees/`).

## Why the conductor location is canonical

- Every existing footnote worktree already lives under `~/conductor/workspaces/abilities/` (athens, davis, milan-v1, montpelier, nairobi-v1, ...). Consistency keeps `git worktree list` legible and avoids inventorying multiple roots.
- Conductor itself drops worktrees there and calls `scripts/setup/setup-worktree.sh` via `conductor.json`. Other creation paths converging on the same location means agents, Conductor, and the CLI all produce the same end state.
- `setup-worktree.sh` symlinks `.fno/` state per-file from canonical, so target gates, the backlog graph, events.jsonl, the inbox, and the ledger stay coherent across worktrees. A worktree somewhere else either misses these links or has to reinvent them.
- `internal/` at the worktree root is symlinked absolutely to the canonical's `internal/` by `setup-worktree.sh`, so the Obsidian vault references resolve regardless of worktree depth, but only when the setup script has run.

## How each worktree-creation path is reconciled

| Path | Triggered by | How it lands at the canonical location |
|---|---|---|
| Conductor | Conductor UI | `conductor.json` `scripts.setup` runs `setup-worktree.sh`; worktree path is set by Conductor itself |
| Raw `git worktree add` | Agent or terminal | Agent reads `AGENTS.md`, sees this rule, places at `~/conductor/workspaces/abilities/...`, then runs the setup script |
| `claude --worktree <name>` (footnote-ecosystem project) | Claude Code CLI + footnote plugin | The plugin's `WorktreeCreate` hook (`hooks/worktree-setup.sh`) reads `worktree.use_conductor_canonical: true` from `.fno/settings.yaml` and redirects to `~/conductor/workspaces/<repo>/<name>` before running its existing setup (env copy, dep install, verification). Repo name from `basename $(git rev-parse --show-toplevel)`. |
| `claude --worktree <name>` (non-footnote project) | Claude Code CLI | Wire `scripts/setup/worktree-create-hook.sh` into your **user-global** `~/.claude/settings.json` (recipe below). Falls back to Claude's default `.claude/worktrees/<name>` if not wired. |
| Warp tab from `git worktree add` | Warp UI | Place the worktree at the canonical location first; point Warp at it after (see Warp TOML snippet in `AGENTS.md` history). |

All paths converge on the same end state when configured. Do NOT wire BOTH a project-level `WorktreeCreate` hook in `.claude/settings.json` AND the plugin hook AND a user-global hook for the same repo - Claude Code merges matching hooks across levels and runs them in parallel, which races on path creation. Pick one of plugin (recommended for footnote-ecosystem) or user-global.

## Opting in for non-footnote projects (user-global recipe)

Wire the script into `~/.claude/settings.json` so `claude --worktree <name>` redirects in every Claude session, including projects that don't load the footnote plugin:

```json
{
  "hooks": {
    "WorktreeCreate": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "bash ~/code/abilities/scripts/setup/worktree-create-hook.sh"
          }
        ]
      }
    ]
  }
}
```

The script reads the repo name from `git rev-parse --path-format=absolute --git-common-dir`, so the same wiring works for every repo. After redirecting it runs `scripts/setup/setup-worktree.sh` if present; absent that, the worktree is bare. If the host repo has its own bootstrap (env copy, dep install), point its own setup script from `WorktreeCreate` instead - the footnote script is one option, not the only one.

For footnote-ecosystem projects, prefer the plugin hook over this recipe (it also handles dep install and verification automatically). The two hooks must not both fire for the same project; the plugin hook gates on `worktree.use_conductor_canonical: true` in `.fno/settings.yaml`, so a project that doesn't set the flag falls through to whatever user-global hook you have.

## Forbidden locations

Do NOT create worktrees at any of:

- `.claude/worktrees/<name>` (Claude Code's default for `--worktree`, the `superpowers:using-git-worktrees` skill default, and the `fno:git-worktrees` skill default). The `WorktreeCreate` hook now redirects `claude --worktree` here, so you should not see Claude Code land in `.claude/worktrees/` for general worktree creation.
- `~/.warp/worktrees/abilities/<name>` (Warp's own worktree directory). Sprawl; setup script does not run there automatically.
- `<repo>/worktrees/<name>` or any path inside the canonical checkout. Confuses `find`, breaks `.gitignore` reasoning.
- `../<name>` or any sibling-of-canonical path. Hard to inventory.

**Exceptions (sanctioned inside-checkout placement):** two flows intentionally write worktrees at `.claude/worktrees/<name>` and are exempt from the inside-checkout redirect:

- the cross-project pipeline (`/target cross-project`) writes per-project worktrees at `.claude/worktrees/{feature}` in each participating repo (scoped, short-lived, torn down per feature; see `AGENTS.md` "Cross-Project Worktrees"). It creates them via `git worktree add` directly, not the `WorktreeCreate` hook, so the redirect never fires for it.
- `/speculate` materializes its parallel variations at `.claude/worktrees/<name>` via its own copy of the setup script (`skills/speculate/scripts/worktree-setup.sh`), which deliberately does NOT carry the inside-checkout guard.

Do not generalize either; both are scoped, documented exceptions.

If a skill or tool defaults to one of these paths for a general-purpose worktree, override with `~/conductor/workspaces/abilities/<name>` at invocation time.

## Enforcement

The rule is actively enforced (not just documented) by three mechanisms that share one read-only verdict helper, `hooks/helpers/check-impl-location.sh` (it emits `verdict=ok|canonical-protected` plus a `nested_count` / `nested_path` advisory; always exits 0, degrades to `ok` outside a git repo):

- **SessionStart heads-up (universal, advisory).** `hooks/session-start.sh` consults the verdict on every session, including `claude --bg`, and surfaces a non-blocking note when the session is on the canonical checkout's protected branch and/or a nested worktree exists under `.claude/worktrees/`. It never blocks a session.
- **Implementation-entry refusal (`/target`, `/do`, `/fix`).** All three consult the same verdict before the first write. On `canonical-protected` they refuse with the branch name and the `TARGET_LOCATION_OK=main-acknowledged` escape (`/do` and `/fix` via a Step 0 preflight; `init-target-state.sh` keeps the hard refusal for `/target`). Attended `/target` additionally OFFERs to create a conductor worktree and continue there. One shared verdict means no per-skill drift.
- **Inside-checkout prevention.** The `WorktreeCreate` hook (`hooks/worktree-setup.sh`) redirects any worktree path resolving inside `.claude/worktrees/` to `~/conductor/workspaces/<repo>/<name>`, regardless of the `worktree.use_conductor_canonical` flag; the flag only chooses the redirect target, never whether inside-checkout is allowed (the `/speculate` exception above keeps its own placement).

## What gets linked vs left local

The granular contract is in `scripts/setup/setup-worktree.sh`. In summary:

| Path | Treatment | Why |
|---|---|---|
| `internal/` | Symlink to canonical's `internal/` (absolute) | Obsidian vault link; depth-independent because absolute |
| `.fno/settings.yaml`, `tasks.json`, `tasks.md`, `ledger.json`, `ledger.md` | Symlink to canonical | Shared project state; target gates and backlog must be coherent across worktrees |
| `.fno/codemap.md` | Symlink (regenerable artifact; last-writer-wins) | Latest map visible everywhere |
| `.fno/wake-signals/` | Symlink to canonical | Wake signals dropped by the inbox drain; read per-project, not per-worktree |
| `internal/agents/abilities/inbox.md` | Reached via the `internal/` symlink (no separate link) | Cross-project inbox lives in the Obsidian vault, not under `.fno/` |
| `.claude/agents/`, `commands/`, `skills/` | Symlink to canonical | Locally-installed agents/commands/skills, shared across worktrees |
| `.claude/settings.local.json` | Symlink to canonical | Permission allowlist and autoMemoryDirectory pin |
| `.claude/scheduled_tasks.json`, `.lock` | Symlink to canonical | `/schedule` skill state; lock prevents concurrent-worktree races |
| `.claude/.skill-scoping-state.json`, `audit-progress.txt`, `plans/`, `*.local.md` | Symlink to canonical | All other gitignored `.claude/` state follows the canonical so skill scoping, audit checkpoints, and local notes stay in sync |
| `.agents/`, `.codex/`, `.codex-plugin/`, `.gemini/` | Symlink to canonical (skip-if-missing) | Per-CLI project config roots; gitignored at top level so they propagate the same way across worktrees |
| All other tracked files | git checkout produces them | Never copy or symlink tracked content |

If `setup-worktree.sh` finds a real (non-symlink) file or non-empty directory at a target, it warns and skips; it never overwrites real state.

## Override semantics

If the user explicitly asks for a different worktree path in a single conversation, follow the user. The user's in-the-moment instruction outranks this rule. Note that the `.fno/` state links will not exist there and target-state coherence is lost; tools that touch `.fno/` will operate on a fresh local state file rather than the canonical.

Do not solicit such overrides. Default to the canonical location and only deviate when explicitly told.

## When to update this file

Update only when:
- The canonical worktree root changes (rare; deliberate org decision).
- `scripts/setup/setup-worktree.sh` or `scripts/setup/worktree-create-hook.sh` gets renamed or relocated.
- A new skill or tool starts creating worktrees in a non-canonical location and the forbidden list needs to grow.
- The cross-project pipeline's worktree placement changes.
