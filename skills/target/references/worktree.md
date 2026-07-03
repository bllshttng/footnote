# Worktree Management (Decision Matrix)

Single canonical reference for target, megawalk, the
cross-project pipeline, and speculate. Read this *before* writing inline
`git worktree` snippets in a new pipeline.

## TL;DR

Two distinct ways to get an isolated copy of the repo. Pick by use case,
not by author preference.

| Use case | Right tool | Why |
|----------|-----------|-----|
| target / megawalk production runs | **Manual worktrees** via `scripts/lib/worktree-manager.sh` | Predictable path/branch for retrieval; persists past agent lifetime; expensive setup must not repeat |
| Cross-project per-project worktrees | **Manual worktrees** | Same as above, plus PR cross-linking needs known branch names |
| Code-review / discovery agents | **`Agent(isolation: "worktree")`** | Ephemeral sandbox; auto-cleanup when no changes; no artifact retrieval needed |
| Speculative N-variation runs | **`Agent(isolation: "worktree")`** per variation | Ephemeral; pick-one drop-N pattern; auto-cleanup of losers |
| Refactor / fix-attempt with rollback | **`Agent(isolation: "worktree")`** | Discard if it doesn't work |

## Failure modes to avoid

These are real bugs that have shipped and shaped the rules above.

- **`isolation: "worktree"` silently no-ops on Claude Code.** Two megawalk
  workers landed in the same repo and clobbered each other's commits on
  2026-04-22. Production pipelines (target and friends) MUST use manual
  worktrees so two workers cannot share a repo.
- **Tilde-form `worktree_base` paths get concatenated, not expanded.** A
  settings entry like `worktree_base: ~/conductor/workspaces/foo` was
  emitted verbatim by older inline shell, producing a bogus
  `/users/bb16/~/conductor/...` path. The shared module expands a single
  leading tilde via parameter substring before joining.
- **Hardcoded `.claude/worktrees/` ignored settings.yaml.** Each pipeline
  rolled its own path resolution and the per-project `worktree_base`
  declaration was honored by no one. Always go through the manager.
- **`pnpm install` reran from scratch on every worktree creation.** ~3 min
  of wasted wall time per worktree, even when the lockfile hadn't changed.
  The manager hashes `pnpm-lock.yaml` / `uv.lock` / `package-lock.json` and
  skips install on match.

## Manual worktree shape (production)

```bash
# Create. Manager picks worktree_base from settings.yaml; falls back to
# <repo>/.claude/worktrees. Branch defaults to feature/{slug}.
RESULT=$(bash scripts/lib/worktree-manager.sh create "$PROJECT" "$SLUG")
WORKTREE_PATH=$(echo "$RESULT" | python3 -c \
    'import json,sys; print(json.load(sys.stdin)["path"])')
EXISTED=$(echo "$RESULT" | python3 -c \
    'import json,sys; print(json.load(sys.stdin)["existing"])')

# Setup: lockfile-hash cached, env files declared in settings copied.
# Idempotent - safe to call on every run.
bash scripts/lib/worktree-manager.sh setup "$WORKTREE_PATH"

# Cleanup later (selective):
bash scripts/lib/worktree-manager.sh cleanup --mode=stale --older-than=7d
```

The `create` JSON includes `existing: true` for an idempotent hit, so a
re-run against the same slug attaches to the existing branch instead of
erroring on `git worktree add`.

## Ephemeral worktree shape (review, discovery, speculation)

```typescript
Agent({
  description: "Review the diff for security regressions",
  isolation: "worktree",
  prompt: "...",
})
```

The harness creates and destroys the worktree for you. No path retrieval,
no cleanup work; appropriate when the artifact you care about is the
agent's verdict, not files on disk.

## Path resolution chain (manual mode)

1. Project-local `<repo>/.fno/settings.yaml` `work.workspaces[].projects[].worktree_base`
2. Global `~/.fno/settings.yaml` (same key, multi-workspace shape)
3. Global `~/.fno/settings.yaml` `work.projects[].worktree_base` (legacy flat shape)
4. Back-compat default: `<repo>/.claude/worktrees`

A single leading `~/` in `worktree_base` is expanded to `$HOME` by the
manager. A path with no leading tilde is used as-is.

## Branch naming (manual mode)

| Mode | Default branch |
|------|---------------|
| `--mode=manual` (default) | `feature/{slug}` |
| `--mode=ephemeral` | `{slug}` (caller may override with `--branch`) |

Pipelines that need PR cross-linking (cross-project) rely on the
`feature/{slug}` convention being identical across repos. Don't override
unless you know what you're doing.

**Dispatched worktrees** (the megawalk walker, `WorktreeManager.create`) name
their branch `<config.branch.prefix>/<slug>-<node>` (default prefix `fno`, e.g.
`fno/plan-docs-in-plans-dir-status-consistency-x-ff83`) via
`worktree.branch_name()` — legible and round-trip resolvable back to the node
(the full node id, not a truncated hex). Only new branches use this; existing
`feature/*` branches keep working. (x-ff83 W3)

## Verbs reference

| Verb | Purpose |
|------|---------|
| `create <project> <slug> [--mode] [--branch]` | Create or attach to a worktree. JSON output includes `path`, `branch`, `existing`. |
| `setup <worktree-path> [--force]` | Install deps + copy env files. Cached on lockfile hash. |
| `cleanup [--mode=ephemeral\|stale\|all] [--older-than=Nd] [--dry-run] [--prefix=<prefix>]` | Wraps git-worktrees lifecycle script. Refuses to remove worktrees with `status: IN_PROGRESS` target-state. |
| `migrate [--auto] [--dry-run]` | One-shot scan of legacy locations for orphaned worktrees. |
| `resolve <project>` | Echo the resolved `worktree_base` for a project. |

## One-shot migration runbook

Older pipelines created worktrees in two places that the current setup
no longer treats as canonical:

- `<repo>/.claude/worktrees/` (per-repo, hardcoded by old inline shell)
- `~/conductor/workspaces/<project>/` (used inconsistently when settings
  declared it but most pipelines ignored the declaration)

After this module lands, run the migration verb once per machine to
classify and remove orphaned worktrees. It never runs automatically.

```bash
# 1. See what's there. No filesystem changes.
bash scripts/lib/worktree-manager.sh migrate --dry-run

# 2. After reviewing the dry-run output, do the cleanup.
bash scripts/lib/worktree-manager.sh migrate --auto
```

`migrate` reads each candidate worktree's `.fno/target-state.md`.
A worktree with `status: IN_PROGRESS` is classified `live` and is
NEVER removed - even with `--auto`. Everything else is `stale` and is
deleted via `git worktree remove --force` when `--auto` is set.
