# Megatron: Cross-Project Fleet Orchestration

> **Status:** v0 substrate landed.
> Filesystem-substrate gap closure replaces the inbox
> bridge with plan-file dispatch + filesystem completion files. Budget
> circuit breaker and research-wave handler remain follow-ups.
>
> **Step-5 group 3:** the Python commander poll loop is
> GONE. `fno megatron run` now execs the unified Rust loop
> (`fno-agents loop run --driver megatron`); each project is walked as
> a megawalk one altitude down, and completion evidence is the child
> walk's journaled termination event. The mission wave logic lives in
> `queue.py` behind the `fno megatron next` / `fno megatron complete`
> plumbing verbs. See
> [unified-loop.md](unified-loop.md) "The megatron driver" for the
> current commander architecture; the "Commander loop" section below
> is HISTORICAL (the data layer, manifest guard semantics, and
> completion-file ledger it describes are still load-bearing).

Megatron sits one altitude above megawalk: where megawalk iterates a
single project's backlog until done, megatron coordinates *several*
projects on a shared mission. Projects are the wave units. Dispatch
writes a plan file into each child project's `internal/fno/plans/`
and registers it via `fno backlog intake`; completion is signaled by a
JSON file the child project's target stop hook drops into the mission's
fleet completions directory.

This document describes the v0 architecture, the filesystem-substrate
gap closure (the load-bearing path today), and remaining follow-ups.

## Layering

```
+----------------------------------------+
|  /megatron skill (in-conversation)     |  authoring wizard
+--------------------+-------------------+
                     | drafts manifest
                     v
+----------------------------------------+
|  fno megatron CLI (run/status/cancel)  |  headless surface
+--------------------+-------------------+
                     | calls
                     v
+----------------------------------------+
|  fno-agents loop run --driver megatron |  commander (Rust loop)
+--------------------+-------------------+
                     | shells
                     v
+----------------------------------------+
|  fno megatron next / complete          |  mission queue verbs (queue.py)
+--------------------+-------------------+
                     | reads
                     v
+----------+--------+--------+-----------+
| manifest |  state |  brief | validator |  data layer
+----------+--------+--------+-----------+
                     |
                     v dispatches via
+----------------------------------------+
|  plan file + fno backlog intake        |  work creation (dispatch.py)
+----------------------------------------+
                     |
                     v walked by
+----------------------------------------+
|  fno-agents loop run --driver megawalk |  per-project child walk
|    --mission <id> --termination-key k  |  (mission-scoped selection)
+----------------------------------------+
```

`/megatron` authors the mission. `fno megatron run` execs the unified
Rust loop, whose MegatronQueue shells `fno megatron next` (manifest +
state + sha guard + dispatch-on-demand) and `fno megatron complete`
(journal-evidenced close records). Each project unit is walked as a
megawalk one altitude down, scoped to the mission's nodes via
`--mission`; the child walk's `--termination-key`'d journal event is
the completion evidence the commander awaits.

## File layout

| Path | Purpose |
|------|---------|
| `cli/src/fno/megatron/__init__.py` | Public surface (Manifest, MissionState, validate_manifest, loop, etc.) |
| `cli/src/fno/megatron/manifest.py` | YAML-frontmatter schema + parser |
| `cli/src/fno/megatron/state.py` | Mission state file with filelock + monotonicity |
| `cli/src/fno/megatron/validator.py` | Pure-function validation rules |
| `cli/src/fno/megatron/brief.py` | Wave brief assembly + injection |
| `cli/src/fno/megatron/queue.py` | Mission queue verbs (mission_next / mission_complete) |
| `crates/fno-agents/src/loop_megatron.rs` | MegatronQueue + MegatronDispatcher (the Rust commander) |
| `cli/src/fno/megatron/cli.py` | Typer subapp (run/status/cancel/list) |
| `skills/megatron/SKILL.md` | Authoring wizard |
| `~/.fno/fleet/{slug}/00-INDEX.md` | Per-mission manifest |
| `~/.fno/fleet/{slug}/state.md` | Per-mission state |
| `~/.fno/fleet/{slug}/.cancelled` | Cancel sentinel |

## Mission manifest

Manifests live at `~/.fno/fleet/{slug}/00-INDEX.md` and use YAML
frontmatter for the structured schema, markdown body for human prose.

Example:

```yaml
---
mission_type: fleet
mission_id:
slug: 2026-05-06-state-co
created: 2026-05-06T13:00:00Z
budget:
  cost_cap_usd_per_mission: 50.0
failure_policy: block        # only "block" supported in v0
autonomy_level: cautious
waves:
  - wave: 1
    mode: sequential
    projects:
      - name: example-pipeline
        body: "Add new region source bootstrap..."
  - wave: 2
    mode: parallel
    projects:
      - name: footnote
        body: "Document the megatron flow"
      - name: acme-frontend
        body: "Surface region rollout completes"
---

# Mission body prose...
```

Wave defaults: `mode: sequential`, `kind: heads-up`. Per-project
`kind` may override (e.g. `kind: question` for a research wave).

## Validator rules

`validate_manifest` is a pure function over a parsed `Manifest`. Each
rule appends to a result list rather than raising; aggregated errors
let the operator fix every problem at once.

| Code | Trigger | Severity |
|------|---------|----------|
| `empty_wave` | `projects: []` AND `tasks: []` | block |
| `wave_project_cap_exceeded` | More than `config.megatron.max_projects_per_wave` (default 8) | block |
| `body_oversize` | Project body > 10KB | block |
| `research_chain` | Research wave directly follows another research wave | block |
| `duplicate_project_in_wave` | Same canonical project name appears twice in one wave | block |

The cap value is configurable via the `max_projects_per_wave` kwarg on
`validate_manifest`, plumbed from `config.megatron.max_projects_per_wave`
in `~/.fno/settings.yaml`.

### Manifest validation: duplicate project names

Each wave's `projects:` list must contain no duplicate canonical names.
A manifest with `[fake-a, fake-a]` (or `[fake-a, a-short]` where
`a-short` resolves to canonical `fake-a` through `resolve_project_name`)
is rejected at `validate_manifest` time with
`code="duplicate_project_in_wave"`. Use the same project name in a
*later* wave to dispatch it again; the duplicate check is per-wave.

Rationale: a wave's `_wave_complete` predicate collapses participant
names through `set` semantics. Two declarations of the same project in
one wave means one completion file marks the wave complete with
insufficient evidence; the validator refuses dispatch rather than let
that loophole reach the dispatcher.

## Mission state

Each mission's state lives in YAML frontmatter at
`~/.fno/fleet/{slug}/state.md`. The commander reads and rewrites
it under a sibling filelock at `state.md.lock`. Status transitions are
monotonic:

```
pending  -> running | cancelled
running  -> paused | complete | cancelled
paused   -> running | cancelled
complete -> (terminal)
cancelled -> (terminal)
```

Backwards or sideways transitions raise `MissionStateRegression`.
Corrupt frontmatter triggers an atomic rename to `state.md.bak` and
raises `MissionStateCorrupt` so the operator can repair it offline.

`append_sent_msg_id` is order-preserving and idempotent, so concurrent
appends from a daemon and a manual CLI invocation can never tear the
list.

### Manifest immutability

Once a mission has dispatched its first wave, the manifest file is
treated as immutable. The commander records a raw-bytes sha256 of the
manifest in `state.md` (`manifest_sha256:`) on the first iteration that
finds the field unset, and re-verifies the hash at the start of every
subsequent iteration. A mismatch pauses the mission with
`paused_reason=manifest_mutated:stored_sha=<12-hex> fresh_sha=<12-hex>`.

The hash is over **raw bytes**, not canonicalized YAML. Reordering
frontmatter keys or adding trailing whitespace counts as mutation.

Allow-listed writes go through `update_state_field(path, name, value)`,
which only accepts `manifest_sha256` and `manifest_sha256_first_set_at`.
The single-field helper exists so status flips keep going through
`update_status` (which enforces the monotonic transition table); a
future field that needs the same lock semantics adds its name to the
allowlist rather than reusing `update_status`.

Recovery paths (both require an operator action - the commander never
auto-lifts a paused mission):

- **Revert the manifest** to its original content, then flip status
  back to `running` (e.g. via `update_status(state_path, "running")` or
  by hand-editing `state.md`). The next `run_iteration` sees a matching
  hash and dispatches normally.
- **Re-baseline** (the edit was intentional): hand-edit `state.md` to
  remove the `manifest_sha256:` line AND flip status back to `running`,
  then re-issue `fno megatron run`. The commander lazy-inits the field
  from the current manifest bytes and continues.

## Commander loop (HISTORICAL - replaced by the unified Rust loop)

> Step-5 group 3 deleted `loop.py`. The wave-advance and sha-guard
> semantics below survive inside `queue.py::mission_next`; the
> POLLING (steps 5-6) does not - the Rust loop awaits the child
> walk's termination event instead. Kept for provenance.

`run_iteration(manifest_path, state_path, *, dispatcher)` was the
single-step entry point. The longer-running `run()` composes it with
sleep + cancel-sentinel polling.

Per iteration:

1. Read manifest and state under filelock semantics. Atomic
   `load_manifest_and_sha` reads the bytes once so the sha and the
   parsed manifest derive from the same snapshot.
   - **Manifest immutability check:** if `state.manifest_sha256` is
     None, lazy-init both it and `manifest_sha256_first_set_at` via
     `stamp_manifest_sha` (single-lock two-field atomic write) before
     any dispatch. If non-None and mismatching, transition status to
     `paused` with a structured `paused_reason` and return - no
     dispatch occurs in this iteration. See "Manifest immutability"
     under [Mission state](#mission-state) for the operator-side
     recovery paths.
2. Determine current wave: lowest wave whose participants are not all
   in `received_completes` with provenance-validated `reply_to`.
3. **Brief assembly:** if this is wave N+1 (i.e. wave N has at least
   one valid complete), synthesize the brief from wave N's
   discoveries (sorted by `msg_id` for stable ordering).
4. **Dispatch:** for each un-sent project in the current wave (the
   manifest's project list minus the current `sent_msg_ids[wave_N]`),
   inject the brief into the body and call the dispatcher. Append the
   returned `msg_id` to state.
5. **Wave-complete check:** if every wave-N project has a
   provenance-valid complete, advance. Final wave -> stamp
   `status: complete`.
6. Sleep `poll_interval_s` (default 30) and repeat.

Provenance gating: a `kind: complete` whose `reply_to` is not in
`sent_msg_ids[wave_N]` does NOT advance the wave. The drain handler
still records it via convo-signals so humans can investigate; the
loop is the gate owner.

Idempotent restart: `sent_msg_ids[wave_N]` is authoritative. If a
prior commander dispatched 2 of 4 before crashing, the next
commander dispatches the remaining 2 in declaration order (the test
`test_idempotent_restart_resumes_dispatch` verifies this).

## Filesystem completion substrate

The v0 substrate above is the historical record; the current load-bearing
path is the filesystem completion substrate that closed the dispatch -
observe gap. The inbox-bridge wiring stays in place for cascades and
forensic refs, but the commander no longer depends on `append_received_complete`
(which had zero production callers - hence "gap closure").

### Pipeline shape

```
megatron commander                                       child project
       |                                                       |
       | dispatch_project(canonical-name, body, wave)          |
       |---write plan file --> {project}/internal/fno/plans/
       |                       2026-MM-DD-mission-<id>-wave-<N>-<project>.md
       |                                                       |
       |---fno backlog intake plan_file ------------------------>| (megawalk)
       |                                                       v
       |                                              target executes plan
       |                                                       |
       |                                              status: COMPLETE
       |                                                       |
       |                          stop hook emits completion JSON
       |   <----  ~/.fno/fleet/{slug}/completions/wave-{N}/{project}.json
       |
       | _wave_complete walks the fleet completions tree
       | counts canonical project names against the manifest
```

### Key surfaces

| Surface | Purpose |
|---------|---------|
| `cli/src/fno/projects/resolve.py` | Canonical-name resolver. Walks `~/.fno/settings.yaml`'s `work.workspaces.*.projects[]` for `{name, short_name -> name}` map. Exceptions: `ProjectNotFound`, `SettingsNotFound`, `DuplicateShortName`. |
| `cli/src/fno/megatron/dispatch.py` | `dispatch_project(...)` writes a plan file with mission frontmatter (`mission_id`, `mission_wave`, `mission_slug`, `mission_from_msg_id`) and runs `fno backlog intake` from the child project's cwd. Returns `DispatchResult(plan_path, backlog_node_id)`. Cleans up the orphan plan file on intake failure. |
| `cli/src/fno/megatron/loop.py::_wave_complete` | Walks `~/.fno/fleet/{slug}/completions/wave-{N}/*.json` per call. Canonicalizes manifest names through `resolve_project_name` before comparing against completion-file `project:` field. `DuplicateShortName` propagates (spec AC1-FR). |
| `cli/src/fno/megatron/state.py::read_state` | Stamps the filesystem-derived `slug` and `_fleet_root_override` onto `MissionState` so the `received_completes` @property can walk `~/.fno/fleet/{slug}/completions/wave-*/*.json` on every access. There is no stored field to keep in sync; `_state_to_dict` skips the property entirely. Tests inject completions via the `_received_completes_override` field. `append_received_complete` is `_append_received_complete_for_test`, writing a JSON fixture file. |
| `cli/src/fno/megawalk.py::extract_mission_env` | When megawalk spawns target for a mission-flagged backlog node, it sets `TARGET_MISSION_{ID,WAVE,SLUG,FROM_MSG_ID}` on the subprocess env. Schema errors raise `MegawalkSchemaError` before target runs. |
| `hooks/helpers/init-target-state.sh` | Reads `TARGET_MISSION_*` env vars and seeds five `mission_*` fields in `target-state.md` frontmatter (always present, null when absent). Adds the `mission_complete_emitted_at: null` sentinel for idempotency. |
| `hooks/target-stop-hook.sh::emit_mission_complete_if_needed` | On `status: COMPLETE`, reads mission_* fields + `pr_url` from state.md and writes `~/.fno/fleet/{slug}/completions/wave-{N}/{project}.json` atomically (tempfile + mv -f). Idempotent via `mission_complete_emitted_at`. PR URL absence emits `<help reason="mission-pr-url-missing">` and defers. |
| `cli/src/fno/megatron/artifact.py::write_mission_complete` | Walks the completion tree + `~/.fno/ledger.json` and writes `mission-complete-{mission_id}.md` with frontmatter (`mission_status`, `waves_completed`, `project_count`, `elapsed_seconds`, `total_cost_usd`) and per-wave/per-project markdown rows. Called from `update_status` on terminal flip; non-fatal. |
| `cli/src/fno/megatron/cli.py::cmd_retro` | `fno megatron retro <mission-id>` prints the artifact above to stdout. Exit 4 on incomplete missions, 2 on unknown. |

### Completion JSON shape

`~/.fno/fleet/{slug}/completions/wave-{N}/{project}.json`:

```json
{
  "schema_version": 1,
  "project": "example-pipeline",
  "wave": 1,
  "mission_id": "ab-XXXXXXXX",
  "pr_url": "https://github.com/.../pull/123",
  "pr_status": "open",
  "commit_sha": "abc1234",
  "completed_at": "2026-05-13T19:18:00Z",
  "reply_to_msg_id": null,
  "discoveries": "### Discoveries\n- Found X.\n- Decided Y over Z.\n"
}
```

Records written before the `schema_version` rollout have no version
field; consumers default to `schema_version: 0` when the key is absent.
New records always set `schema_version: 1`. Future
shape changes increment the integer; consumers reading a version they do
not understand log a `completion_schema_unknown` event but still parse
the record (additive evolution).

`project` is the canonical name (resolved at write time). Malformed JSON
is treated as not-yet-complete and emits a `completion_file_corrupt`
event to `.fno/events.jsonl` (the loop never crashes on a bad
file). `pr_status` is captured at emit time and is not updated as the
PR progresses through review/merge - consumers needing live status
should query the GitHub API via `pr_url`.

`discoveries` is a markdown chunk extracted from the session's
`.fno/HANDOFF.md` (the `### Discoveries` section, falling back to
`### Learnings`), capped at 8 KB with a `...(truncated to 8 KB; see
HANDOFF.md)` marker when oversized. The field is always present in
completion JSONs written after plan 2026-05-13-megatron-discoveries-field;
older JSONs omit it. The brief assembler
(`cli/src/fno/megatron/brief.py::assemble_wave_brief`) reads
`discoveries` first and falls back to the legacy `body` field for
in-flight missions whose completion JSONs predate this change. Empty
string here means the helper ran but found nothing - the stop hook
emits a `mission_handoff_unreadable` event to `hook-events.jsonl`
carrying the reason (`file_not_found`, `file_unreadable`, `file_empty`,
or `no_sections_found`) for forensic correlation.

### Locked decisions

These shaped the design and are NOT to be relitigated without a new
spec:

1. Substrate is filesystem completion files, NOT inbox bridge.
2. Dispatch is plan-file write + `fno backlog intake`, NOT a send verb (`fno mail send`).
3. Reply emitter is the target stop hook, gated on target-state.md mission fields.
4. Canonical project name = settings.yaml `name`; `short_name` is alias.
5. Single-machine assumption; cross-host is out of scope.
6. Harness-agnostic by contract; adapters are the integration path.
7. Daemons drop out of megatron's critical path.
8. `state.received_completes` is derivative: an `@property` that walks the filesystem on every access (not a stored field).
9. Manifests are immutable after first dispatch (enforced by raw-bytes
   sha256 stamped in state.md on first iteration).
10. Executor routing: `executor: do` (archer / TDD) plan-default.

### Known limitations and follow-ups

- **Cross-wave context regression.** The brief assembler reads
  `c.get("body", "")` from completion records, but the new completion
  JSON shape has no `body` field. Wave N+1 dispatch renders
  `(no discoveries reported)` for every prior project. Follow-up: write
  a `discoveries:` field at completion-emit time (extracted from
  HANDOFF.md) or strip the brief mechanism entirely.
- **Manifest immutability enforcement** via checksum is deferred.
- **`fno megatron reconcile`** verb for git-scan backfill when completion
  files drift from PR state. See the
  Recovery subsection below.

## Provenance fields

Three nullable fields ride alongside the existing `ref_pr` /
`ref_node` / `ref_gate` refs on every inbox message:

| Field | Purpose |
|-------|---------|
| `mission_id` | Active mission this message belongs to |
| `source_mission` | Originating mission for cascades |
| `cascade_of` | Originating msg-id when one mission begets another |

These three fields were carried by the old inbox-send cascade flags (`--ref-mission` / `--source-mission` / `--cascade-of`). With messaging now under `fno mail send`, which does not carry the megatron cascade flags, megatron dispatch no longer uses a send verb at all: it writes the plan file and runs `fno backlog intake` (locked decision 2). The mission provenance now travels on the manifest and the backlog node rather than on a sent message. `fno mail reply` (retained, recipient-resolved) keeps the same provenance fields for replies.

## Drain integration

The headless inbox watcher (`fno mail drain`) routes `kind: complete`
to `_drain_complete`, which:

- Always acks (UNREAD -> READ) so the recipient inbox doesn't wedge.
- Logs `inbox_complete_drained` to `convo-signals.jsonl` with
  `mission_id`, `msg_id`, `from`, `reply_to` for the commander to
  pick up on the next iteration.
- Resolves the mission state file (walks `~/.fno/fleet/*/state.md`)
  and appends the complete to a `## Received completes` section under
  filelock. Phase 2's structured writer will replace this placeholder.
- Surfaces failure modes as distinct events: `inbox_complete_orphan`
  (no live mission), `inbox_complete_malformed` (no `### Summary` /
  `### Discoveries`), `inbox_complete_state_write_failed` (OSError
  appending).

## Cancellation

`fno megatron cancel <mission-id>` does two things:

1. Touches `~/.fno/fleet/{slug}/.cancelled` (the cancel
   sentinel; mirrors `.target-cancelled`).
2. Flips state.md to `status: cancelled`.

The running commander's next iteration sees the sentinel, marks the
state cancelled if it isn't already, and exits cleanly. In-flight
projects continue autonomously (no stop-the-world signal); their
results land in the recipient's own ledger and are dropped on the
floor by the cancelled mission.

## Mission-completion artifact (forensic, not gating)

When a mission flips into a terminal status (`complete`, `cancelled`,
`failed`), `update_status` writes a forensic attestation at
`~/.fno/fleet/{slug}/mission-complete-{mission_id}.md` while the
state filelock is held. The artifact is **forensic, not gating**:
state.md is the source of truth, and an artifact write that fails for
any reason is logged + mirrored to stderr and swallowed so the state
flip is preserved.

The attestation aggregates wave-level identity for postmortem and
tooling consumers:

```yaml
---
type: mission-complete
mission_id: ab-mm0042
slug: fleet-2026-05-07-cool-mission
status: complete
created_at: 2026-05-07T12:00:00Z
completed_at: 2026-05-07T13:00:00Z
total_waves_planned: 2          # null if manifest absent/unparseable
total_waves_advanced: 2
projects: [backend, docs, frontend]   # null if manifest absent
waves:
  - wave: 1
    sent_msg_ids: [msg-w1a, msg-w1b]
    received_completes:
      - {from: backend,  msg_id: msg-cb1a, reply_to: msg-w1a, ts: null}
      - {from: frontend, msg_id: msg-cb1b, reply_to: msg-w1b, ts: null}
    advanced_at: null            # populated when received_completes carry ts
  - wave: 2
    sent_msg_ids: [msg-w2a]
    received_completes:
      - {from: docs, msg_id: msg-cb2a, reply_to: msg-w2a, ts: null}
    advanced_at: null
total_dispatched: 3
total_received: 3
paused_reason: null              # populated for failed missions
---
# Mission complete: <title>
...
```

Manifest-derived fields (`projects`, `total_waves_planned`) are elided
to `null` when the sibling manifest at `00-INDEX.md` is missing or
unparseable. State.md alone is enough to render a complete artifact;
the manifest only provides structural metadata.

The slug field comes from filesystem position: `read_state` stamps
`state.slug = path.parent.name`; the value is never written to state.md
frontmatter (filtered by `_state_to_dict`) so the on-disk schema stays
unchanged.

`mission_artifact_path(fleet_dir, mission_id)` is the deterministic path
constructor; `build_mission_artifact(state, manifest, completed_at=None)`
is the pure builder; `write_mission_artifact(state, fleet_dir,
manifest=None)` performs the atomic-rename write inside the
update_status filelock scope.

## Lifecycle events

Three events fire on the mission timeline (all on `.fno/events.jsonl`):

| Event | Trigger | Source | Payload |
|-------|---------|--------|---------|
| `mission_started` | First `pending`/`paused` -> `running` flip | `state.py::_emit_status_event` | `{mission_id}` |
| `wave_advanced` | Iteration first observes a wave's participants all complete | `loop.py::_emit_pending_wave_advanced_events` | `{mission_id, wave, child_session_ids}` |
| `mission_complete` | Any -> terminal flip (`complete`, `cancelled`, `failed`) | `state.py::_emit_status_event` | `{mission_id, status}` (status maps `complete -> done`) |

`wave_advanced` is idempotent against events.jsonl: the emit helper
scans the file for existing `(mission_id, wave)` entries and only emits
the gap, so a wave that drains between iterations is observed exactly
once even when the loop calls `_emit_pending_wave_advanced_events` at
both entry and the in-iteration completion check.

`child_session_ids` is currently emitted as an empty list. Wave-level
child-session tracking is a follow-up; forensic consumers correlate
child sessions via the mission artifact's per-wave `received_completes`
block until that wiring lands.

All three emissions are telemetry-only and swallow exceptions so a
broken events.jsonl can never block the state machine.

## Failure modes table

| Failure | Detection | Handling | Test |
|---------|-----------|----------|------|
| Manifest YAML invalid | `load_manifest` | `ManifestError` with line number | `test_ac5_fr_malformed_yaml_raises_with_line` |
| Empty wave | `validate_manifest` | `ValidationError(code="empty_wave")` | `test_empty_wave_rejected` |
| > 8 projects/wave | `validate_manifest` | `ValidationError(code="wave_project_cap_exceeded")` | `test_wave_project_cap_exceeded` |
| Research chain | `validate_manifest` | `ValidationError(code="research_chain")` | `test_research_chain_rejected` |
| Body > 10KB | `validate_manifest` | `ValidationError(code="body_oversize")` | `test_body_oversize_rejected` |
| Corrupt state.md | `read_state` | atomic rename to `state.md.bak` + `MissionStateCorrupt` | `test_corrupt_frontmatter_backed_up` |
| Two commanders | `write_state` | filelock timeout -> `CommanderAlreadyRunning` | covered by filelock concurrency test |
| Backwards status | `_check_transition` | `MissionStateRegression` | `test_monotonicity_rejects_running_to_pending` |
| Provenance mismatch | `_valid_completes_for_wave` | complete recorded but not gated | `test_provenance_mismatch_complete_rejected` |
| Mid-dispatch crash | `_projects_to_dispatch` | resume from `sent_msg_ids` length | `test_idempotent_restart_resumes_dispatch` |
| Orphan complete (no mission) | `_drain_complete` | `inbox_complete_orphan` signal | `test_complete_drain_orphan_when_no_live_mission` |
| Malformed complete body | `_drain_complete` | `inbox_complete_malformed` signal | `test_complete_drain_malformed_body_acks_with_warning` |
| Artifact write failure (disk full / permission / yaml.YAMLError) | `write_mission_artifact` | WARN to log + stderr, swallowed; state-flip preserved | `test_write_recovers_from_disk_full`, `test_state_flip_survives_artifact_write_failure` |
| Manifest unparseable at terminal-write time | `_resolve_manifest` | manifest-derived fields elided to `null`; state-derived fields still present | `test_build_manifest_none_elides_fields` |

## Follow-up backlog

The v0 substrate ships a working end-to-end flow at the foundational
layer. Three follow-up backlog nodes finish the design:

- **Mission-level cost cap** (`04-budget-circuit-breaker.md`) - sums
  participating projects' `~/.fno/ledger.json` entries tagged
  with the mission_id, escalates via `kind: question` on cap.
- **Research wave handler** (`04b-research-wave.md`) - the
  research-then-propose flow that pauses for operator approval
  before dispatching the proposed waves.
- **End-to-end smoke** - delivered: `test_e2e_two_wave_mission_to_complete`
  drives a 2-wave manifest through `run_iteration` to `status: complete`
  and asserts both the mission artifact and the full lifecycle event
  stream (`mission_started`, `wave_advanced`, `mission_complete`).

These do not block the substrate from being usable. The CLI accepts
manifests today; the loop dispatches and tracks completes; the
authoring skill adopts missions to the backlog.

### Recovery: `fno megatron reconcile`

When a mission stalls because one project's stop hook never wrote its
completion JSON (process killed, schema mismatch, wrong canonical name),
`fno megatron reconcile <mission-id>` scans fleet completion files
against the manifest and queries GitHub for PRs on each missing
project's expected branch. Default is read-only:

```bash
$ fno megatron reconcile ab-XXXXXXXX
# Reconcile: ab-XXXXXXXX
Fleet dir: `~/.fno/fleet/2026-05-13-foo`

## Wave 2 / `example-pipeline`
- State: `missing-pr-merged`
- Branch pattern: `feature/2026-05-13-foo-mission-XXXXXXXX-wave-2-example-pipeline`
- PR #123: MERGED; https://github.com/.../pull/123; merged at 2026-05-13T20:00:00Z
```

Exit codes:

- `0` - no drift
- `2` - unknown mission, missing manifest, unreadable state, or invalid argument
- `3` - `gh` CLI not on PATH at scan time (scan-level auth failure)
- `4` - drift detected (read-only) or unresolved drift after `--backfill`. Per-record `gh` failures surface inside the report as `state: query-failed` and exit 4, distinct from the scan-level exit 3.

To backfill missing completions from confirmed merged PRs:

```bash
fno megatron reconcile ab-XXXXXXXX --backfill
```

Backfill safety contract:

- Never clobbers an existing completion file. If the file is on disk at
  write time, the record is skipped with an `already present` reason.
- Only backfills records whose drift state is `missing-pr-merged`. Open,
  closed-unmerged, no-PR, and ambiguous states are skipped.
- Refuses to write when the merged PR carries a null `merge_commit_sha`
  (a degenerate gh response).
- Backfilled JSONs carry `source: "reconcile-backfill"` so the forensic
  trail distinguishes them from stop-hook-written completions.
- Emits a best-effort `reconcile_backfill_wrote` event to
  `.fno/events.jsonl` for downstream telemetry.

The verb never auto-resumes the mission. The next `fno megatron run`
picks up the new files via the existing pause-lift path.

For ambiguous matches (multiple PRs on the same branch pattern), use
`--pr <N>` to disambiguate by 1-indexed candidate position. For
scripting, `--json` emits structured output.

## See also

- `docs/provider-rotation.md` - sibling cross-cutting feature with the
  same author-then-execute shape
- `skills/megawalk/SKILL.md` - the single-project layer below
- `cli/src/fno/inbox/store.py` - the message substrate
