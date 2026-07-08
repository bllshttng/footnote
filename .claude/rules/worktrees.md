# Worktree convention

The single place that says where git worktrees go for this repo and what to do after creating one. Loaded by every Claude / Codex / footnote / Hermes session via `AGENTS.md` (which `CLAUDE.md` and `GEMINI.md` import). Skill defaults (e.g. `superpowers:using-git-worktrees` and `fno:git-worktrees` placing worktrees in `.claude/worktrees/`) lose to this rule because user-level instructions outrank skill defaults.

## The rule

**The worktree root is config-driven via `config.paths.worktrees_base`. Set nothing and the defaults work - no config needed.**

- **Unset (OSS-neutral default):** harness-native `<repo>/.claude/worktrees/<name>`. That directory is gitignored, so `rg`/Grep already skip it. No relocation, zero config.
- **`config.paths.worktrees_base: <dir>`:** worktrees land at `<dir>/<repo>/<name>` (`<repo>` = `basename $(git rev-parse --show-toplevel)`).
- **`worktree.use_conductor_canonical: true` is DEPRECATED:** it still works (behaves as `worktrees_base = ~/conductor/workspaces`), but prefer `config.paths.worktrees_base`. The single knob is honored by the `WorktreeCreate` hook, `cli/src/fno/worktree.py` (the megawalk walker), and `cli/src/fno/worktree_paths.py` (the agents runtime; its own neutral default is `~/.fno/worktrees/{proj}-{name}`).

After `git worktree add`, run the canonical setup script (substitute your own `worktrees_base` for the example path, or use the harness-native default when unset):

```bash
# worktrees_base set (e.g. this repo's ~/conductor/workspaces):
git worktree add <worktrees_base>/<repo>/<name> <branch>
cd <worktrees_base>/<repo>/<name>

# unset (harness-native default) - run from the repo root; the path is repo-root-relative:
git worktree add .claude/worktrees/<name> <branch>
cd .claude/worktrees/<name>

bash scripts/setup/setup-worktree.sh
```

The setup script links the canonical `internal/`, the project-level `.fno/` state (settings, tasks, ledger, inbox, wake-signals, codemap), the `.claude/` subdirs (`agents`, `commands`, `skills`, `settings.local.json`, `scheduled_tasks*`), and the `.agents/` provider/agent config. It auto-resolves the canonical via `CANONICAL` env var, then `CONDUCTOR_ROOT_PATH`, then `git rev-parse --git-common-dir`, then `$HOME/code/me/abilities`. See `scripts/setup/setup-worktree.sh` for the full contract.

**Enter the worktree in-session (harness step).** A footnote `/target` cold-start prints the worktree path in its `fno target start` receipt, then calls the harness **EnterWorktree** tool with `path=<that worktree>` so the session actually runs from inside the worktree - a shell `cd` does not persist across tool calls. Location-agnostic: any path in `git worktree list` is enterable on first entry, so this works identically for a configured `worktrees_base` and the harness-native default. See `skills/target/SKILL.md` for the full cold-start ritual and its caveats.

To remove a worktree, use the archive script:

```bash
bash scripts/setup/archive-worktree.sh <name|path>
```

It enforces strict pre-removal checks (clean working tree, no unpushed commits, no live target session), prompts before SIGTERM'ing any process rooted in the worktree path, runs `git worktree remove` + `git worktree prune`, and preserves the branch. Flags: `--force` (skip checks), `--yes` (skip process-kill prompt), `--delete-branch` (drop the branch with `git branch -D` after removal). The plain alternative is `git worktree remove <path>`; never use `rm -rf` (leaves dangling refs in `.git/worktrees/`).

## Maintainer environment example: conductor workspaces

This is one environment's concrete choice, NOT a default an OSS user inherits - set nothing and you get the harness-native `.claude/worktrees/` above. The maintainer sets `config.paths.worktrees_base: ~/conductor/workspaces` in their **global** `~/.fno/config.toml`, so every footnote worktree lands at `~/conductor/workspaces/<repo>/<name>` (e.g. `~/conductor/workspaces/footnote/<name>`). Why keep it there:

- Every existing footnote worktree already lives under `~/conductor/workspaces/footnote/` (athens, davis, milan-v1, montpelier, nairobi-v1, ...). Consistency keeps `git worktree list` legible and avoids inventorying multiple roots. This is a per-environment choice set with `config.paths.worktrees_base: ~/conductor/workspaces`, not a hardcoded default - an OSS user who sets nothing gets harness-native `.claude/worktrees/`.
- Conductor itself drops worktrees there and calls `scripts/setup/setup-worktree.sh` via `conductor.json`. Other creation paths converging on the same location means agents, Conductor, and the CLI all produce the same end state.
- `setup-worktree.sh` symlinks `.fno/` state per-file from canonical, so target gates, the backlog graph, events.jsonl, the inbox, and the ledger stay coherent across worktrees. A worktree somewhere else either misses these links or has to reinvent them.
- `internal/` at the worktree root is symlinked absolutely to the canonical's `internal/` by `setup-worktree.sh`, so the Obsidian vault references resolve regardless of worktree depth, but only when the setup script has run.

## How each worktree-creation path is reconciled

| Path | Triggered by | How it lands at the canonical location |
|---|---|---|
| Conductor | Conductor UI | `conductor.json` `scripts.setup` runs `setup-worktree.sh`; worktree path is set by Conductor itself |
| Raw `git worktree add` | Agent or terminal | Agent reads `AGENTS.md`, sees this rule, places at the configured base (`<worktrees_base>/<repo>/<name>`, e.g. `~/conductor/workspaces/footnote/...`) or harness-native `.claude/worktrees/<name>` when unset, then runs the setup script |
| `claude --worktree <name>` (footnote-ecosystem project) | Claude Code CLI + footnote plugin | The plugin's `WorktreeCreate` hook (`hooks/worktree-setup.sh`) reads `config.paths.worktrees_base` from `.fno/config.toml` (via `fno config get`). Set -> relocate to `<base>/<repo>/<name>`; the deprecated `worktree.use_conductor_canonical: true` -> `~/conductor/workspaces/<repo>/<name>`; unset -> leave harness-native `<repo>/.claude/worktrees/<name>` in place. Then runs its existing setup (env copy, dep install, verification). Repo name from `basename $(git rev-parse --show-toplevel)`. |
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

For footnote-ecosystem projects, prefer the plugin hook over this recipe (it also handles dep install and verification automatically). The two hooks must not both fire for the same project; the plugin hook always runs in a footnote project (it relocates when `config.paths.worktrees_base` / the deprecated `use_conductor_canonical` is set, else leaves the worktree harness-native), so do not also wire a user-global hook for the same repo.

## Location by config

- **`worktrees_base` unset:** `<repo>/.claude/worktrees/<name>` is the harness-native default (gitignored, so search-clean). This is now allowed - the old blanket "inside-checkout is forbidden" rule is retired.
- **`worktrees_base` set (e.g. this repo's `~/conductor/workspaces`):** worktrees land at `<base>/<repo>/<name>`; the `WorktreeCreate` hook relocates `claude --worktree` there, so don't hand-place a worktree in `.claude/worktrees/` in that environment.

Still forbidden regardless of config (sprawl / un-inventoriable / no setup script):

- `~/.warp/worktrees/<repo>/<name>` (Warp's own worktree directory). Setup script does not run there automatically.
- `<repo>/worktrees/<name>` or any non-`.claude` path inside the canonical checkout. Confuses `find`, breaks `.gitignore` reasoning.
- `../<name>` or any sibling-of-canonical path. Hard to inventory.

**`/speculate`** keeps its own `.claude/worktrees/<name>` placement (via `skills/speculate/scripts/worktree-setup.sh`, which deliberately omits the relocation block) even when `worktrees_base` is set - a scoped, documented exception. Do not generalize it.

## Enforcement

The rule is actively enforced (not just documented) by three mechanisms that share one read-only verdict helper, `hooks/helpers/check-impl-location.sh` (it emits `verdict=ok|canonical-protected` plus a `nested_count` / `nested_path` advisory; always exits 0, degrades to `ok` outside a git repo):

- **SessionStart heads-up (universal, advisory).** `hooks/session-start.sh` consults the verdict on every session, including `claude --bg`, and surfaces a non-blocking note when the session is on the canonical checkout's protected branch and/or a nested worktree exists under `.claude/worktrees/`. It never blocks a session.
- **Implementation-entry refusal (`/target`, `/do`, `/fix`).** All three consult the same verdict before the first write. On `canonical-protected` they refuse with the branch name and the `TARGET_LOCATION_OK=main-acknowledged` escape (`/do` and `/fix` via a Step 0 preflight; `init-target-state.sh` keeps the hard refusal for `/target`). Attended `/target` additionally OFFERs to create a conductor worktree and continue there. One shared verdict means no per-skill drift.
- **Config-driven relocation.** The `WorktreeCreate` hook (`hooks/worktree-setup.sh`) relocates `claude --worktree` to `<config.paths.worktrees_base>/<repo>/<name>` when that knob is set (or `~/conductor/workspaces/<repo>/<name>` for the deprecated `worktree.use_conductor_canonical: true`); when unset it leaves the worktree harness-native in `.claude/worktrees/` (the `/speculate` exception keeps its own placement regardless).

## What gets linked vs left local

The granular contract is in `scripts/setup/setup-worktree.sh`. In summary:

| Path | Treatment | Why |
|---|---|---|
| `internal/` | Symlink to canonical's `internal/` (absolute) | Obsidian vault link; depth-independent because absolute |
| `.fno/config.toml`, `tasks.json`, `tasks.md`, `ledger.json`, `ledger.md` | Symlink to canonical | Shared project state; target gates and backlog must be coherent across worktrees |
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
