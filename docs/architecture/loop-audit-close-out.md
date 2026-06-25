# Loop-Audit Close-Out: Three Subsystem Contracts

> Architecture doc for three bugs (BUG-LOOP-001 / BUG-MR-007 / BUG-MT-003). Each section documents the contract the fix establishes, not the bug it fixed - the bug is in the commit history.

## Background

The 2026-05-15 audit of target / megawalk / megatron surfaced 30 bugs, 14 gaps, and 11 optimizations. Most landed in dedicated PRs. The three covered here share the property of being small, structurally independent, and bundled into one PR for review economy. They touch three different subsystems (target init, megawalk walker, megatron consumer chain) but share no files.

## 1. Init: phase_init skip_flags reflect resolved size profile

`hooks/helpers/init-target-state.sh` writes the initial `target-state.md` and emits a `phase_init` event into `.fno/events.jsonl`. The state file's `skip_flags_initial:` snapshot is the source of truth that the stop hook diffs against to detect mid-session skip-flag drift; the event is the parallel record consumed by post-hoc auditors.

**Contract:** the `skip_flags` payload of the `phase_init` event MUST equal `skip_flags_initial:` in the state file, byte-for-byte, for every `TARGET_SIZE` profile (`S` / `M` / `L`) and for every per-flag env override (`TARGET_NO_*`).

Both surfaces are now built from the same shell variables (`$no_external`, `$no_docs`, `$no_ship`, `$no_verify`, `$no_goals`, `$no_browser`, `$no_clean`, `$no_how_to`, `$no_memory`, `$has_ui`) which the size-profile case block + per-flag env-override layering set in lockstep. The event payload uses `jq --argjson` per flag rather than a literal jq object.

**AC1-EDGE: jq absent.** The existing `command -v jq` guard wraps the entire emit block. When jq is not on PATH, the state file is still written but the event is silently skipped. `verify_provenance` cannot validate any artifact written in this session, but the legacy-exemption phase-04 cutoff covers this case for sessions that predate the cutoff.

**Why the surfaces drift.** The previous implementation built the jq object with literal boolean values frozen at code-write time (`{no_external: false, no_clean: true, ...}`), which always matched the `M` profile. Any future detector that uses `events.jsonl` as source-of-truth instead of state file would mis-fire on every non-M session. Today's `check_skip_flag_drift` reads from state file only, so the drift was invisible - but the contract above closes the door on that class of detector regression.

## 2. Walker: mtime-gated graph_entries reload

`run_walker` in `cli/src/fno/megawalk.py` previously called `load_graph(graph_path)` once at startup and operated on the cached list for the rest of the walker's life. External `fno backlog intake / defer / triage / supersede` mutations during a long walker session were invisible until restart.

### Reload trigger

At the top of every walker tick (after the resume / pause-sentinel / budget checks), the walker stats BOTH `graph.json` AND its sha256 sidecar (`graph.json.sha256`). If EITHER `st_mtime_ns` differs from the cached baseline for that file, `load_graph` is called. The new entry list replaces the cached one and both baselines advance. Id-based comparisons mean the existing `completed_ids` / `blocked_ids` overlay sets continue to match correctly against the fresh dicts.

Tracking both files is necessary because `locked_mutate_graph` writes `graph.json` BEFORE its sidecar in two atomic-rename steps. A reader that lands in that gap sees a sha mismatch (`GraphCorruptionError`), and the later sidecar write does NOT bump `graph.json`'s mtime. Reloading only on graph mtime would leave the walker permanently stuck on stale entries until the next mutation. The sidecar catching up later counts as a fresh signal that triggers retry without needing a second graph mutation.

### Failure handling

`load_graph` reads bytes from disk before attempting JSON parse. Three exception classes can surface from a mid-write race or filesystem hiccup:

| Exception | Cause |
|-----------|-------|
| `GraphCorruptionError` | sha256 sidecar mismatch (writer crashed mid-write before sidecar update) |
| `json.JSONDecodeError` | partial / truncated JSON read (writer in `os.replace` window) |
| `OSError` (any subclass) | EIO, NFS hiccup, EACCES on inode swap, or `FileNotFoundError` if the file was unlinked between `os.stat` and `read_bytes` |

The reload `try` catches all three. On any failure the walker:
1. Emits a `graph_reload_failed` event with `error_kind`, `mtime_ns`, `sidecar_mtime_ns`, and a 200-char `detail`.
2. Keeps the cached `graph_entries` for this tick.
3. **Advances both `_graph_mtime_ns` and `_sidecar_mtime_ns` to the failed values.**

Step 3 is the no-retry-storm contract: a slow concurrent writer (an `fno backlog intake` taking 3-4s on a 1s poll loop) would otherwise re-trigger `load_graph` and emit `graph_reload_failed` every tick until the writer finished. Advancing both baselines means the next reload attempt waits for a fresh mtime mutation on EITHER file rather than hammering the same broken pair. When the writer reaches step 2 and the sidecar mtime advances, the walker picks it up next tick and retries; this handles the `GraphCorruptionError` recovery path without requiring a second mutation to graph.json itself.

The seed-time `os.stat` is also wrapped in `except OSError` (per [memory note on FileNotFoundError ⊂ OSError](../../README.md)) so a permission flap or NFS hiccup at startup falls through to `_graph_mtime_ns = None` rather than crashing the walker.

### Late-appearing graph.json

If `graph.json` does not exist when the walker starts, both seed mtimes stay `None`. The reload trigger is gated only on `graph_path is not None` (not on the seed mtime), so a later stat that returns a real mtime satisfies the `current_mtime_ns != _graph_mtime_ns` check (`int != None` is True) and triggers reload. The walker recovers when the file appears.

### Events

Two new event types are registered in `cli/src/fno/events/schema.yaml`:

| Event | Required fields | Optional fields | Emitted when |
|-------|-----------------|-----------------|--------------|
| `graph_reloaded` | `old_count`, `new_count`, `mtime_ns` | `sidecar_mtime_ns` | mid-loop reload succeeded |
| `graph_reload_failed` | `error_kind`, `mtime_ns` | `sidecar_mtime_ns`, `detail` | mid-loop reload raised one of the caught exceptions |

These rides on `_emit_event` in `megawalk.py` which writes directly to `.fno/megawalk-events.jsonl` and bypasses the schema-validating `append_event` sink. The schema entries are documentation-only for these particular events; the registration is conventional and aids future migration to the validating sink.

### Same-second-resolution race

On a coarse-resolution filesystem (HFS+, some NFS mounts) two writes within the same second may yield the same `st_mtime_ns`. The second write becomes invisible until the next mutation bumps mtime. This is a documented design tradeoff (Locked Decision #3 in the plan): one-tick staleness is acceptable; the alternative (sidecar-checksum poll on every tick) trades correctness for steady-state load.

## 3. Megatron: producer/consumer field-fallback contract

The producer (`hooks/target-stop-hook.sh:459`) writes per-wave completion JSONs with the schema:

```json
{
  "schema_version": 1,
  "project": "p1",
  "wave": 1,
  "mission_id": "m1",
  "pr_url": "https://github.com/...",
  "pr_status": "open",
  "commit_sha": "abcdef123456...",
  "completed_at": "2026-05-15T...",
  "reply_to_msg_id": "msg-...",
  "discoveries": "### Discoveries\n\n..."
}
```

The consumers (`cli/src/fno/megatron/{artifact,brief}.py`) read four fields that are NOT in the producer schema: `from`, `msg_id`, `reply_to`, `ts`. The previous implementation read them directly via `c.get(...)`, which always returned `None` against producer JSONs. Tests passed because the test fixture wrote a half-consumer / half-producer shape. Every shipped mission's forensic artifact had nulls.

**Locked Decision #2:** consumer-side tolerance via fallback chains, NOT producer-side rename. Smaller surface, backward-compatible with legacy on-disk records, no migration story.

### Fallback chains

Both `artifact._build_waves` and `brief.assemble_wave_brief` apply the same chains:

| Consumer field | Fallback chain |
|----------------|----------------|
| `from` | `c.get("from")` ⟶ `c.get("project")` |
| `msg_id` | `c.get("msg_id")` ⟶ `str(c.get("commit_sha") or "")[:12]` ⟶ `c.get("project")` |
| `reply_to` | `c.get("reply_to")` ⟶ `c.get("reply_to_msg_id")` |
| `ts` | `c.get("ts")` ⟶ `c.get("completed_at")` |

Each chain terminates in a producer-mandatory field (`project`, `commit_sha`, `reply_to_msg_id`, `completed_at`) so a record from a fully-conforming producer always has every consumer field non-None.

### Defensive str() on commit_sha

`str(c.get("commit_sha") or "")[:12]` rather than `(c.get("commit_sha") or "")[:12]`. The `or ""` short-circuits only on falsy values; a corrupted producer that wrote a non-string truthy value (int, list, dict) would flow through and `123[:12]` raises `TypeError`. The `str()` coercion keeps the chain crash-free for any input shape and is identity on real string commit_shas.

### Test fixture alignment

`_append_received_complete_for_test` accepts backward-compatible optional `commit_sha` and `discoveries` kwargs so future schema drift can be caught by unit tests writing producer-shape fixtures. Existing call sites are unchanged because both kwargs default to `None` and are written into the payload either way.

### Brief implements only `from` and `msg_id`

`assemble_wave_brief` constructs a section header `## From {from_proj} ({msg_id}):`. `reply_to` and `ts` are not part of the brief format, so the brief implements only the first two fallback chains. The artifact implements all four because its `received_completes[i]` dict shape is structured forensic data, not a header string.

## Cross-cutting

All three fixes share an anti-pattern catalogued in past memory:

- `feedback_register_new_event_types_in_schema_yaml`: schema-validating event sinks silently drop unregistered types. The graph_reloaded / graph_reload_failed registration follows the recipe even though the megawalk emit path bypasses the validating sink.
- `feedback_filenotfounderror_is_oserror`: the megawalk `os.stat` and `load_graph` catches both broadened to `OSError` rather than enumerating subclasses.
- `feedback_int_comparison_outside_try_block` (family): the megatron `commit_sha` defensive coercion mirrors the same lesson - assume callers can pass non-canonical types and either coerce inside try/except or convert before the operation.

## Files

| Subsystem | Path |
|-----------|------|
| Init | `hooks/helpers/init-target-state.sh` (lines 634-648) |
| Init test | `cli/tests/hooks/test_init_target_state_skip_flags.py` |
| Walker | `cli/src/fno/megawalk.py` (`run_walker` reload block, lines 1525-1531 + 1665-1695) |
| Walker test | `cli/tests/megawalk/test_graph_reload.py` |
| Walker schema | `cli/src/fno/events/schema.yaml` (`graph_reloaded`, `graph_reload_failed`) |
| Megatron consumers | `cli/src/fno/megatron/artifact.py` (`_build_waves`, lines 185-197) |
| | `cli/src/fno/megatron/brief.py` (`assemble_wave_brief`, lines 69-74) |
| Megatron fixture | `cli/src/fno/megatron/state.py` (`_append_received_complete_for_test`) |
| Megatron test | `cli/tests/megatron/test_completion_contract.py` |
