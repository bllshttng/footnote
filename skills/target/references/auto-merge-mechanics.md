# Auto-Merge Mechanics

**Load when:** `auto_merge_approved: true` in target-state.md. Covers Phase 6a (pre-ship rebase), Phase 8a (post-review merge), and the resolution chain.

See also [auto-merge.md](auto-merge.md) for the cross-skill auto-merge protocol.

## Phase 6a: Pre-ship rebase (auto_merge_approved only)

If `auto_merge_approved: true` in target-state.md, run rebase before `/pr create`:

```bash
fno do pr rebase --base=origin/main
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

Docs are advisory (control-plane step 6, ab-f8e5f214): there is no docs pre-gate on auto-merge. `fno do target init` no longer writes a docs-completion field, so nothing reads one. Docs run before ship so they ride in the same PR, but a missing docs pass never blocks the merge.

```bash
PR_NUMBER=$(sed -n 's/^pr_number:[[:space:]]*//p' .fno/target-state.md | xargs)
RESULT=$(fno do pr merge "$PR_NUMBER")
OUTCOME=$(echo "$RESULT" | jq -r '.outcome')
```

State update rules per outcome:

| outcome | State update |
|---------|-------------|
| `merged` | Append `$PR_NUMBER` to `merged_prs` |
| `held` | No state change (checks not green, retry when green) |
| `failed` | Append `{pr: $PR_NUMBER, reason: ...}` to `merge_failed` (NOT a target failure - PR exists) |
| `skipped` | No state change (auto-merge disabled, or finalize already armed GitHub's queue) |

No `queued` outcome exists (x-9d11): the merge verb executes with in-process checks enforcement. GitHub's native auto-merge queue is armed by `fno-agents finalize` alone.

A `failed` outcome does NOT block the promise. The PR was created successfully; merge failure is post-hoc.

## Auto-merge resolution chain (CRITICAL)

When deciding whether to auto-merge, resolve in this order (first match wins):

1. **`TARGET_NO_MERGE=1`** (set by `fno do target init --no-merge`, or exported) - auto_merge_approved = false
2. **`no-merge` as a whole token in the invocation** - auto_merge_approved = false
3. **`TARGET_AUTO_MERGE=1`** - auto_merge_approved = true
4. **Local `.fno/config.toml`** - `config.auto_merge.enabled`
5. **Global `~/.fno/config.toml`** - `config.auto_merge.enabled`
6. **Default** - false

`init-target-state.sh` implements this chain and is the only place it lives. The resolved value is recorded in `target-state.md` as `auto_merge_approved`. `fno do pr merge` and `fno-agents finalize` resolve the same posture from it. A refusal (`false`) outranks every grant. A grant also needs the live `auto_merge.enabled` switch. Or it needs its own `auto_merge_source: env-target-auto-merge` stamp. That stamp is the operator's `TARGET_AUTO_MERGE=1` grant, folded at init. The git-protection hook guards raw `gh pr merge` on the same posture. An operator disarm mid-flight therefore still withholds everywhere. `TARGET_AUTO_MERGE` is read at init only. Runs carrying a mesh identity (`FNO_AGENT_SELF`) or an unattended marker scrub it. An interactive session the operator launched carries neither marker and sits inside the operator's trust boundary. That is the documented way the grant reaches a run. The stamp keeps every grant auditable. Exporting it on a merge command line grants nothing.

**Arming happens at the gate, not at PR creation.** `worker/ship.py` used to arm GitHub's native auto-merge the moment the PR existed, gated only on this field. That pre-authorized a merge before anything had been verified: once `--auto` is set GitHub owns the timing and fires as soon as its own branch protections pass, so a reviewer posting a blocking finding after CI greens loses the race. `fno-agents finalize` arms it instead, on the `DonePRGreen` terminal only, so the merge authorizes a state loop-check just verified (PR up, CI green, no unaddressed blocking finding). A reviewer that has not posted yet gets the whole CI duration before the merge is armed at all.

Note this reads the manifest field, and the manifest carries the run's raw `input` as a quoted scalar written *above* it. A multi-line argument spills newlines into the file, so finalize's parser treats lines inside that scalar as untrusted for this key specifically - the same "prose must never manufacture merge authority" rule the chain above enforces, applied at the read as well as the fold.

FORBIDDEN: auto-merging based on any inference other than this chain.

**The chain is deliberately asymmetric, and every refusal outranks every grant.** Rung 2 honors a `no-merge` token in the invocation because autonomous dispatch bakes exactly that token into its command (`harness_map._AUTONOMOUS_COMMAND`), where it arrives as prose no flag parser sees. There is no matching rung for `auto-merge`: honoring a *grant* discovered in free text would let arbitrary prose manufacture merge authority, whereas honoring a refusal fails safe. A positional `auto-merge` therefore grants nothing on its own and needs `TARGET_AUTO_MERGE=1` or `config.auto_merge.enabled`.

Rung 2 sits ABOVE the `TARGET_AUTO_MERGE` grant deliberately. Nothing in the codebase sets that variable, so the only way it is ever set is inheritance from an ancestor shell or a spawning parent - the same trap `fno do target init` scrubs for `TARGET_BEASTMODE`. An inherited grant must not defeat a refusal typed into this run's invocation, or an autonomously dispatched `/target no-merge <id>` merges anyway.

The token match is whole-token (space-padded), so `no-merger` or a path like `plans/no-merge-notes.md` does not revoke a configured grant. A standalone `no-merge` inside free text still matches and suppresses auto-merge, which is the safe direction.

Log the resolved value + source at session start, e.g.:
`target: auto_merge_approved=true (source: .fno/config.toml)`

## Phase 9 ship-docs invocation note

**Critical for Phase 9:** MUST invoke `fno:ship-docs` via the Skill tool — do NOT write docs ad-hoc. The skill reads `config.toml` to discover roles (from `config.docs.roles`) and generates how-to guides for each affected role. Writing architecture docs alone is NOT sufficient — user-facing how-to guides are required for every role touched by the feature.
