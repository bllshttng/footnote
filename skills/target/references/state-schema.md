# State Schema Reference (Immutable Manifest)

> **Updated 2026-06-05 (ab-d0337fbc):** `target-state.md` is now a write-once session manifest. Fields like `status`, `current_phase`, `iteration`, `blocked_reason`, `provenance_nonce`, and all `completion_gates` booleans no longer exist. See docs/architecture/control-plane-loop.md for the full post-wedge architecture.

## Write-once rule

`fno target init` writes the manifest once at session start. After init, the file is immutable. The only legal post-init mutation is first-fill of an empty `plan_path` via:

```bash
fno state set --field plan_path --value "<path>"
```

Any other field write exits with code 5 and logs a `state_write_refused` event referencing ab-d0337fbc.

**Detection:** a manifest is "immutable" when its frontmatter has no `status:` key (new init style). Old-style manifests (pre-wedge, with `status: IN_PROGRESS`) remain mutable so existing workflows on old state files are unaffected.

## Field list (written by `fno target init`)

### Core inputs

```yaml
session_id: 20260420T091434Z-56177-a1b2c3
  # Precedence: explicit TARGET_SESSION_ID, then generated
  # YYYYMMDDTHHMMSSZ-<provider-infix>PPID-<6 hex chars>.
  # CODEX_THREAD_ID owns the claim but is not reused here: one Codex
  # conversation can run several targets, and loop/finalize dedupe by session_id.
  # Stable for the lifetime of the session (across resume and external-loop restarts).
  # Used by fno-agents loop-check as the primary session discriminator.

created_at: "2026-06-05T03:00:00Z"   # ISO 8601 UTC, set at fresh init
initial_head: "eb7505a7..."          # HEAD at init; `null` in a repo with no commits.
                                      # finalize diffs initial_head..HEAD for commits
                                      # AUTHORED at/after created_at before stamping
                                      # do-provenance, so a rebase-only successor
                                      # (committer date rewritten, author date kept)
                                      # is not mistaken for an implementer.

input: "Add AI chat feature"          # original user argument (idea or plan path)
plan_path: null                       # resolved plan path; may be first-filled post-init
                                      # via `fno state set --field plan_path`
target_size: M                        # S | M | L
dispatch_model: ""                    # model pin from `fno target start/init --model`;
                                      # empty = unpinned. Carried to dispatched workers.
dispatch_provider: ""                 # provider pin from `--provider`; empty = infer the
                                      # invoking harness at dispatch time. Init-time only.
```

### Skip flags (set from CLI flags or config; never mutated after init)

```yaml
no_external: false       # skip external review phase
no_docs: false           # skip docs phase
no_ship: false           # skip ship/PR phase (advisory mode)
no_browser: false        # skip browser testing (advisory run-and-log; never gates)
no_clean: false          # skip simplify/clean phase
no_how_to: false         # skip how-to doc generation
no_memory: false         # skip memory pass
no_deferrals_capture: false  # skip deferrals capture
```

### Session context

```yaml
has_ui: false            # true iff plan or input involves UI surfaces
attended: true           # true in interactive sessions; false for unattended/megawalk
advisory: false          # true when no_ship: true OR config.advisory: true
cross_project: false     # true for cross-project pipeline runs
scratchpad_path: ""      # path to worktree scratchpad directory (if set)
```

### Authority grant (omitted unless granted)

```yaml
authority: full          # `/target beastmode` / `fno target init --beastmode`; absent otherwise
```

Absence is the default posture, so an ungranted session is byte-for-byte unchanged.
Read the grant from the `attended` line of `fno target status`, never from the raw file: a dead manifest never grants authority.
Contract: [SKILL.md §Authority](../SKILL.md#authority-the-beastmode-grant).

### Budget caps (omitted when unconfigured)

```yaml
budget_wall_clock_cap_minutes: 120   # hard wall-clock cap; omitted if unconfigured
budget_cost_cap_usd: 25.00           # hard cost cap; omitted if unconfigured
```

### Provider

```yaml
provider: claude                     # active provider at init time
provider_mode: interactive           # interactive | autonomous | etc.
provider_upgrade_reason: ""          # why provider was upgraded (if applicable)
```

### Session ownership (written by init; used by shim for foreign-session guard)

```yaml
owner_pid: 12345                     # PPID of the init subprocess; transient, best-effort.
                                     # Alive while init runs (the orienter reports
                                     # `live (owner_pid alive)` there), dead soon after,
                                     # so it can PROVE life but never disprove it - and
                                     # never anchors an authority grant.
owner_started_at: "2026-06-05T03:00:00Z"
owner_cwd: "~/conductor/workspaces/abilities/my-feature"
                                     # absolute path to the worktree at init time
claude_session_id: "abc123def"       # Claude session UUID; TARGET_TRANSCRIPT_ID/
                                     # CLAUDE_CODE_SESSION_ID semantics are unchanged
codex_thread_id: "019f48e4-..."      # CODEX_THREAD_ID, or null outside Codex
```

### Auto-merge (set at init from config; never mutated)

```yaml
auto_merge_enabled: false            # mirrors config.auto_merge.enabled at init time
auto_merge_approved: false           # true iff enabled AND invoker allowed AND TARGET_NO_MERGE != 1
```

### Mission fields (set when dispatched from megatron)

```yaml
mission_id: ""
mission_wave: ""
mission_slug: ""
mission_from_msg_id: ""
```

### Graph node fields (appended post-main-write when a graph node is found)

```yaml
graph_node_id: ""                    # backlog node ID associated with this session
graph_node_claim_refused: false      # true if claim acquisition failed
target_claim_key: ""                 # claim key (node:<id>)
target_claim_holder: "target-session:<claim-owner-id>"
target_claim_ttl: ""
target_claim_blocked_reason: ""
```

The graph lock owner and authoritative claim holder derive from the same claim
owner id and must never diverge. The owner is `TARGET_SESSION_ID` when explicitly
assigned, otherwise nonblank `CODEX_THREAD_ID` for Codex, otherwise the manifest
`session_id`. This keeps Codex ownership legible across subprocesses while the
per-target `session_id` stays unique for loop/finalize event deduplication.

## What was removed (ab-d0337fbc)

The following fields existed in pre-wedge manifests and are no longer written:

| Removed field | Was used for |
|---|---|
| `status` | IN_PROGRESS / COMPLETE / BLOCKED state machine |
| `current_phase` | which pipeline phase is running |
| `iteration` / `max_iterations` | loop iteration tracking |
| `blocked_reason` | structured BLOCKED taxonomy |
| `provenance_nonce` | gate-event correlation |
| `completion_gates.*` | quality_check_passed, output_validated, ledger_updated, artifact_shipped, pr_number, external_review_passed, browser_testing_passed, clean_passed (the full pre-wedge gate boolean set) |
| `skip_flags_initial` | snapshot for skip-flag drift detection |
| `verification.*` | consecutive_failures, last_failure_phase |
| `checkpoint.*` | rollback_count, max_rollbacks, latest_name |
| `total_cost` / `total_tokens` / `duration_minutes` | session-cost fields (now only written by pre-promise) |

## Introspection

```bash
fno whoami    # prints phase=n/a (collapsed) when current_phase absent
fno status    # derives session status from latest termination event in events.jsonl
```

## Back-compat

Legacy manifests (those with a `status:` key) are recognized by `fno-agents loop-check` as pre-wedge and handled in allow-exit mode (a `loop_check_legacy_manifest` event is emitted). They are not migrated automatically.
