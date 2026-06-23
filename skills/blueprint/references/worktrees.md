# Git Worktrees

Create isolated workspaces sharing the same repository, allowing work on multiple branches simultaneously.

## When to Use

- **Feature isolation:** Start new feature without affecting current work
- **Parallel phases:** Run phases 02 and 02b simultaneously in separate worktrees
- **Cross-project:** Coordinate frontend + backend changes
- **Clean baseline:** Start implementation with verified clean state

## Directory Selection

### Priority Order

1. **Check `.claude/worktrees/`** (preferred - matches Claude Code native `claude -w`)
2. **Check `.worktrees/`** (legacy fallback)
3. **Check CLAUDE.md** for preference
4. **Ask user** if neither exists

### Project Conventions

Example multi-project layout:
```
webapp/                         # Frontend
├── .claude/worktrees/
│   ├── auth/
│   └── dashboard/

api/                            # Backend
├── .claude/worktrees/
│   ├── auth/                   # Matching worktree
│   └── dashboard/
```

## Creation Process

### 1. Verify Directory Ignored

**MUST verify before creating project-local worktree:**

```bash
git check-ignore -q .claude/worktrees 2>/dev/null || git check-ignore -q .claude 2>/dev/null
```

**If NOT ignored:** Add `.claude/worktrees` to .gitignore and commit first.

### 2. Create Worktree

Use the shared worktree manager - it handles `worktree_base` resolution
from settings.yaml (so projects with `~/conductor/workspaces/<project>`
configured don't end up under `.claude/worktrees/`), branch naming,
and idempotent re-creation. See
[skills/_shared/worktree.md](../../_shared/worktree.md) for the
decision matrix.

```bash
PROJECT=$(basename "$(git rev-parse --show-toplevel)")
RESULT=$(bash scripts/lib/worktree-manager.sh create "$PROJECT" sign-in-sheet)
WORKTREE_PATH=$(echo "$RESULT" | python3 -c \
    'import json,sys; print(json.load(sys.stdin)["path"])')
cd "$WORKTREE_PATH"
```

### 3. Install Dependencies

```bash
# Node.js (frontend)
pnpm install

# Python (backend)
uv sync
```

### 3b. Symlink .fno Directory

Persist state across worktrees by symlinking to main repo:

```bash
MAIN_REPO=$(git rev-parse --path-format=absolute --git-common-dir | sed 's/\/.git$//')

if [[ -d "$MAIN_REPO/.fno" ]]; then
  if [[ ! -e "$WORKTREE_PATH/.fno" ]]; then
    ln -s "$MAIN_REPO/.fno" "$WORKTREE_PATH/.fno"
    echo "Symlinked .fno/ from main repo"
  elif [[ -L "$WORKTREE_PATH/.fno" ]]; then
    echo ".fno/ already symlinked"
  else
    echo ".fno/ exists as directory, skipping symlink"
  fi
else
  echo "No .fno/ in main repo yet (will be created on first target)"
fi
```

**Why symlink:**
- Shared `target-state.md` for pipeline continuity across worktrees
- Shared `ledger.json` for complete feature history
- Shared `STATE.md` for wave progress tracking

### 4. Verify Clean Baseline

```bash
# Run tests to confirm clean state
pnpm test

# If tests fail: Report failures, ask whether to proceed
```

### 5. Report Ready

```
Worktree ready at .claude/worktrees/sign-in-sheet
Branch: feature/sign-in-sheet
Tests: 47 passing, 0 failures
State: .fno/ symlinked from main repo
Ready to implement.
```

## Multi-Repo Features

The `--cross-project` parallel-worktree pattern has been removed. A multi-repo
feature is decomposed into one backlog node per project (linked by `blocked_by`);
each node ships its own PR from its own repo, and spawn-into-project dispatches
the cross-repo handoff. No matched-worktree coordination across repos is needed.

## Claude Code Native Integration

Claude Code's `--worktree` flag (`claude -w feature-name`) creates worktrees at
`<repo>/.claude/worktrees/<name>`. This skill uses the same location for consistency.

### Related settings (in settings.json or .claude/settings.json):

| Setting | Purpose | Example |
|---------|---------|---------|
| `worktree.symlinkDirectories` | Symlink large dirs to save disk | `["node_modules", ".venv"]` |
| `worktree.sparsePaths` | Sparse checkout for monorepos | `["packages/my-app"]` |

### Subagent isolation

Subagents can use `isolation: "worktree"` in the Agent tool to automatically
get a fresh worktree.

## Parallel Phase Execution

For phases that `Can Parallel With` each other:

```bash
# Terminal 1: Phase 02
cd .worktrees/sign-in-sheet
# Work on 02-core-api.md

# Terminal 2: Phase 02b (separate worktree on same branch)
# Or use dispatching-parallel-agents skill
```

## Worktree Cleanup

After feature complete:

```bash
# List worktrees
git worktree list

# Remove completed worktree
cd /path/to/main/repo
git worktree remove .claude/worktrees/sign-in-sheet

# Delete branch if merged
git branch -d feature/sign-in-sheet
```

## Common Mistakes

| Mistake | Fix |
|---------|-----|
| Worktree not ignored | Add to .gitignore FIRST |
| Skipping baseline tests | Always run tests after setup |
| Proceeding with failing tests | Report failures, get permission |
| Hardcoding paths | Use project-relative paths |
| Forgetting dependencies | Run pnpm install / uv sync |
| Forgetting .fno symlink | Symlink is automatic in step 3b |

## Quick Reference

| Command | Purpose |
|---------|---------|
| `git worktree add PATH -b BRANCH` | Create worktree with new branch |
| `git worktree list` | List all worktrees |
| `git worktree remove PATH` | Remove worktree |
| `git check-ignore -q DIR` | Verify directory ignored |

## Red Flags

**Never:**
- Create worktree without verifying it's ignored
- Skip baseline test verification
- Proceed with failing tests without asking
- Assume directory location
- Edit main repo while in worktree (cd back first)

**Always:**
- Verify directory ignored for project-local
- Run dependency install
- Verify clean test baseline
- Use descriptive branch names
- Clean up worktrees after merge
