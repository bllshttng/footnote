---
name: speculate
description: "Run N parallel variations of the same feature for comparison. Use when: exploring design alternatives, comparing architectures, A/B testing implementations, 'give me 3 takes on this'."
argument-hint: "<count> \"<feature>\" [--skill <skill-name>] [--port-start <port>]"
---

# Speculate - Parallel Variation Implementations

Run N implementations of the same feature, each in its own worktree with a unique creative direction. Compare side-by-side when all are done.

## Usage

```bash
# 3 variations of a dashboard redesign
/speculate 3 "redesign the facility dashboard"

# With a specific skill for creative direction
/speculate 3 "dashboard UX" --skill ui-ux-pro-max

# Custom port range for dev servers
/speculate 2 "auth flow" --port-start 4000

# Check status of running speculation
/speculate --status

# Pick a winner after comparison
/speculate --pick 2

# Clean up all speculate worktrees
/speculate --cleanup
```

## How It Works

```
/speculate 3 "dashboard redesign"

Creates:
  .claude/worktrees/dashboard-v1/  (variation: data-dense table layout)
  .claude/worktrees/dashboard-v2/  (variation: card-based visual layout)
  .claude/worktrees/dashboard-v3/  (variation: minimal progressive disclosure)

Each runs as a background Agent with the same task + unique creative direction.
When all complete, starts:
  localhost:3001  (v1)
  localhost:3002  (v2)
  localhost:3003  (v3)

User opens all 3 in browser tabs, picks a winner.
Winner branch gets kept. Losers get cleaned up.
```

## Process

### 1. Parse Input

- **Count:** Number of variations (default 2, max 5)
- **Feature:** What to implement
- **Skill:** Optional creative direction skill (e.g., `ui-ux-pro-max`, `engineering:senior-frontend`)
- **Port start:** Base port for dev servers (default 3001)

### 2. Generate Variation Prompts

For each variation, generate a distinct creative direction. See [references/variation-prompts.md](references/variation-prompts.md) for the full prompt catalog.

If `--skill` is specified, load that skill's methodology and inject per-variation constraints.

If no `--skill`, infer constraint type from the feature description:
- UI features: visual style variations (data-dense, visual, minimal, playful, enterprise)
- API features: architecture variations (REST, tRPC, GraphQL)
- Data features: storage strategy variations

### 3. Create Worktrees

Speculate creates one worktree per variation in `--mode=ephemeral` -
the artifact we care about is the picked variation, the others get
discarded. Path resolution flows through
`scripts/lib/worktree-manager.sh` so `worktree_base` from settings.yaml
is honored. See [skills/_shared/worktree.md](../../_shared/worktree.md)
for the manual-vs-ephemeral decision matrix.

```bash
PROJECT=$(basename "$(git rev-parse --show-toplevel)")
WTM="${FNO_PLUGIN_ROOT:-${SKILL_DIR}/../..}/scripts/lib/worktree-manager.sh"

for i in $(seq 1 $COUNT); do
    SLUG_I="${SLUG}-v${i}"
    bash "$WTM" create "$PROJECT" "$SLUG_I" \
        --mode=ephemeral --branch="speculate/${SLUG_I}" >/dev/null
    bash "$WTM" setup "$(bash "$WTM" resolve "$PROJECT")/${SLUG_I}" &
done
wait
```

**Note:** Speculate uses `--mode=ephemeral` because the losing variations
get cleaned up at the end of the run. The decision matrix doc explains
when ephemeral is the right call vs manual production worktrees. The
shared module's `setup` verb caches install on lockfile hash, so the N
parallel installs only do real work the first time.

### 4. Spawn Parallel Agents

Launch all variations simultaneously using the Agent tool with `run_in_background: true`:

```
For each variation N:
  Agent(
    description="Speculate v{N}: {constraint}",
    model="sonnet",
    run_in_background=true,
    isolation="worktree",
    prompt="You are implementing variation {N} of {COUNT} for: {feature}.
      Your creative direction: {constraint}.
      {skill_context if --skill provided}

      Implement the feature following this direction.
      Commit your work when done."
  )
```

### 5. Track Completion

Write state to `.fno/speculate-state.json`:

```json
{
  "feature": "dashboard redesign",
  "slug": "dashboard",
  "count": 3,
  "port_start": 3001,
  "variations": {
    "v1": {"status": "running", "branch": "speculate/dashboard-v1", "worktree": ".claude/worktrees/dashboard-v1", "constraint": "data-dense"},
    "v2": {"status": "running", "branch": "speculate/dashboard-v2", "worktree": ".claude/worktrees/dashboard-v2", "constraint": "visual"},
    "v3": {"status": "running", "branch": "speculate/dashboard-v3", "worktree": ".claude/worktrees/dashboard-v3", "constraint": "minimal"}
  }
}
```

As each target variation completes, update status to "complete".

### 6. When All Complete - Compare

Run the comparison launcher:
```bash
bash "${SKILL_DIR}/scripts/speculate-compare.sh" .fno/speculate-state.json
```

This starts dev servers on sequential ports and opens browser tabs for side-by-side comparison.

### 7. Pick Winner

Present options to the user:

```
Which variation do you want to keep?
  1) v1 - data-dense table layout
  2) v2 - card-based visual
  3) v3 - minimal progressive disclosure
  a) Keep all branches (cherry-pick later)
  n) Discard all (exploration only)
```

- **Keep winner:** Merge winning branch to current branch, clean up losers
- **Keep all:** Leave all branches for manual selection
- **Keep none:** Clean up everything

## Commands

### `--status`
Show current speculation progress (reads speculate-state.json).

### `--pick N`
Pick variation N as the winner without re-running comparison.

### `--cleanup`
Remove all speculate worktrees and branches:
```bash
fno worktree cleanup --prefix speculate/
```

## See Also

- `fno worktree` - Worktree status, cleanup, archive
- `/target fork` - Full pipeline in worktree
- [references/variation-prompts.md](references/variation-prompts.md) - Creative direction catalog
