# Auto-Merge Mechanics

**Load when:** `auto_merge_approved: true` in target-state.md. Covers Phase 6a (pre-ship rebase), Phase 8a (post-review merge), and the resolution chain.

See also [auto-merge.md](auto-merge.md) for the cross-skill auto-merge protocol.

## Phase 6a: Pre-ship rebase (auto_merge_approved only)

If `auto_merge_approved: true` in target-state.md, run rebase before `/pr create`:

```bash
fno pr rebase --base=origin/main
```

See [ship-phase.md](ship-phase.md) for the full exit-42 dispatch loop. State update rules per exit:

| Exit / status | Action |
|--------------|--------|
| 0 / `clean` | Proceed to Phase 6 (/pr create) |
| 0 / `resolved` | Append entry to `conflicts_resolved`; proceed to Phase 6 |
| 1 / `failed` | Abort — do NOT create PR. Touch `.fno/.target-cancelled` with the failure reason logged; the stop hook will write `status: BLOCKED` on next spawn. |
| 1 / `refused` | Abort — report refused files to user. Touch `.fno/.target-cancelled` so the hook hands off cleanly. |
| 2 / `dirty` | Internal bug (uncommitted changes). Abort loudly. |
| 42 / `needs_resolver` | Dispatch `conflict-resolver` agent via Task tool; call `--continue`; loop |

## Phase 8a: Post-review auto-merge

After `external_review_passed: true` (or skipped), if `auto_merge_approved: true`:

Docs are advisory (control-plane step 6, ab-f8e5f214): there is no docs pre-gate on auto-merge. `fno target init` no longer writes a docs-completion field, so nothing reads one. Docs run before ship so they ride in the same PR, but a missing docs pass never blocks the merge.

```bash
PR_NUMBER=$(sed -n 's/^pr_number:[[:space:]]*//p' .fno/target-state.md | xargs)
RESULT=$(fno pr merge --invoker=target "$PR_NUMBER")
OUTCOME=$(echo "$RESULT" | jq -r '.outcome')
```

State update rules per outcome:

| outcome | State update |
|---------|-------------|
| `merged` | Append `$PR_NUMBER` to `merged_prs` |
| `queued` | Append `$PR_NUMBER` to `merge_auto_queued` |
| `failed` | Append `{pr: $PR_NUMBER, reason: ...}` to `merge_failed` (NOT a target failure - PR exists) |
| `skipped` | No state change (auto-merge disabled for this invoker) |

A `failed` outcome does NOT block the promise. The PR was created successfully; merge failure is post-hoc.

## Auto-merge resolution chain (CRITICAL)

When deciding whether to auto-merge, resolve in this order (first match wins):

1. **CLI positional `no-merge`** - auto_merge_approved = false
2. **CLI positional `auto-merge`** - auto_merge_approved = true
3. **Local `.fno/settings.yaml`** - `config.auto_merge.enabled`
4. **Global `~/.fno/settings.yaml`** - `config.auto_merge.enabled`
5. **Default** - false

If both CLI modifiers are set (user typed both by mistake): `no-merge` wins (safer). The resolved value is recorded in `target-state.md` as `auto_merge_approved` and is the sole signal the global git-protection hook checks.

FORBIDDEN: auto-merging based on any inference other than this chain.

To apply CLI modifiers, set env vars before calling `init-target-state.sh`:
- `no-merge` positional - set `TARGET_NO_MERGE=1`
- `auto-merge` positional - set `TARGET_AUTO_MERGE=1`

If both are set, `TARGET_NO_MERGE=1` wins. The init script writes `auto_merge_approved` + source to `target-state.md`.

Log the resolved value + source at session start, e.g.:
`target: auto_merge_approved=true (source: .fno/settings.yaml)`

## Phase 9 ship-docs invocation note

**Critical for Phase 9:** MUST invoke `fno:ship-docs` via the Skill tool — do NOT write docs ad-hoc. The skill reads `settings.yaml` to discover roles (from `config.docs.roles`) and generates how-to guides for each affected role. Writing architecture docs alone is NOT sufficient — user-facing how-to guides are required for every role touched by the feature.
