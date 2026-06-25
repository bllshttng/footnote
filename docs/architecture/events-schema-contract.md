# Events Schema Contract

How `events.jsonl` rows are produced, validated, and consumed across footnote.

## Why this exists

Before this contract, multiple producers wrote `events.jsonl` rows in
inconsistent shapes. Two legacy envelopes were live at the same time:

```json
{"timestamp":"...","source":"...","type":"...","data":{...}}   // events.sh::emit_event
{"ts":"...","type":"...","data":{...}}                         // events.sh::emit_event_raw
```

Consumers (the target and megawalk stop hooks, postmortems) had to grep
both. New event types could ship without any consumer updating, so a
"verifier present" check was meaningless. Worst, the load-bearing
`target` pre-promise sequence flipped `ledger_updated: true` via `sed -i`
without emitting the matching `phase_transition` event - so the first
promise of a fresh session always failed `verify_provenance`, requiring
an out-of-band `emit-gate-transition.sh` retry. That tax was the
canonical "first promise always fails" symptom.

The contract has six pieces, one job each.

## Six components

```
cli/src/fno/events/schema.yaml      single source of truth
        |
        +-> cli/src/fno/events/__init__.py    (Python validator + builders)
        |
        +-> scripts/lib/events-validate.sh          (bash validator with cached parse)
        |
        +-> scripts/lib/set-gate.sh                 (atomic flip+emit)
        |
        +-> scripts/migrate-events-shape.py         (one-shot legacy rewriter)
        |
        +-> producer call sites                     (target, megatron, abi-loop)
```

A CI parity test (`cli/tests/events/test_validator_parity.py`) runs both
validators against a hand-crafted corpus (`parity_corpus.jsonl`) on
every PR; either side drifting fails the test with a side-by-side
diagnostic.

## Canonical envelope

```yaml
{ts: <RFC3339-UTC>, type: <event-type>, source: <producer>, data: {...}}
```

Required fields and allowed source enum live in
`cli/src/fno/events/schema.yaml`. Per-type required-data fields
live under `event_types[].data.required`. A `phase_transition` with
`gate_bearing: true` MUST carry a `data.gate` value drawn from the
schema's `gates:` allowlist; `gate_bearing: false` is for audit-only
phase boundaries (no gate flip happened).

The 64KB cap on `data` payload is enforced by both validators.

### Source enum

| Source | Producers |
|--------|-----------|
| `target` | the target skill / pre-promise sequence |
| `megawalk` | the megawalk stop hook + roadmap loop |
| `megatron` | mission lifecycle events |
| `abi-loop` | HISTORICAL: the pre-wedge headless `fno loop` driver (verb removed in step-5 group 3); the source value survives in old journals |
| `hook` | every other in-tree hook (PostToolUse, PreToolUse, etc.) |
| `subagent` | reserved for direct subagent emissions |
| `migration` | the one-shot `scripts/migrate-events-shape.py` |
| `test` | test-only fixtures |

Adding a new source means editing the YAML manifest + adding a
`parity_corpus.jsonl` row. CI catches missed updates.

## Three event types you'll touch most

### `phase_transition`

Every gate flip emits one. Carries `gate_bearing: true` and a `gate`
name (must match the schema's `gates:` allowlist). The stop hook's
`verify_provenance` reads these via grep on `.type ==
"phase_transition" and .data.nonce == <session-nonce>` - same shape it
read pre-contract, so older readers keep working through the
rollout window.

### `child_promise`

Target emits this at `<promise>`-time so megawalk can verify the child
session actually completed before advancing the loop. The shape is
`{session_id, nonce}` plus the optional sidecar fields
`{plan_path, graph_node_id, pr_number, pr_url, completed_at}`. Both
`session_id` and `nonce` are required.

#### Producer

`hooks/target-stop-hook.sh` reads `provenance_nonce` from
`target-state.md` and threads it through the jq build that constructs
the event payload. When the nonce is missing or empty (state-file
corruption / pre-upgrade target session), the hook still emits the
event with `data.nonce: ""` and surfaces a loud-fail WARN to both the
hook log and stderr so the operator can investigate at the source
rather than chasing a confusing megawalk block downstream.

#### Consumer

`hooks/megawalk-stop-hook.sh` reads `provenance_nonce` from the prior
target session's `target-state.md` as `PREV_NONCE`, sources the helper,
and dispatches as follows:

| Helper available? | PREV_NONCE | Path |
|---|---|---|
| yes | non-empty | helper-call (`verify_child_promise`); rc=0 advance, rc=1 BLOCK with helper diagnostic, rc=2 BLOCK with substrate diagnostic |
| no  | any       | inline-grep fallback + stderr WARN (helper unsourceable) |
| yes | empty     | inline-grep fallback + stderr WARN (legacy target session predating the producer-nonce write) |

Both fallback branches preserve the substring-only behavior that
shipped before the producer-nonce write so legacy state files keep advancing; the WARN is
the operator's signal of substrate degradation.

#### Helpers

- **fno-agents (canonical)**: `fno-agents verify-evidence child-promise
  SESSION_ID NONCE [EVENTS_FILE]`, folded out of the deleted
  `scripts/lib/verify-event-evidence.sh` into the bundled binary in US1.
  Returns rc=0 (match), rc=1 (missing OR nonce mismatch),
  rc=2 (substrate failure). Diagnostics go to stderr.
- **Python (in-package parallel)**: `verify_child_promise(session_id, nonce, events_path)`
  in `cli/src/fno/events/verify_child_promise.py`. Returns
  `tuple[Literal[True], None]` on match, or `tuple[Literal[False],
  ChildPromiseError]` on failure where the error key is one of
  `child_promise_missing`, `child_promise_nonce_mismatch`, or
  `events_unreadable`. The Python error keys map one-to-one to the
  fno-agents verb's stderr substrings; a parameterized symmetry test at
  `cli/tests/integration/test_verify_child_promise.py::test_diagnostic_symmetry`
  pins the vocabulary so future refactors cannot drift the two apart.

### `mission_started` / `wave_advanced` / `mission_complete`

Megatron lifecycle events. Emitted automatically from
`fno.megatron.state.update_status()` when the mission status
transitions to `running`, `complete`, or `cancelled`. Per locked
decision 10, `mission_id` is the join key for postmortems.

## Producing events

### Shell scripts

Use `scripts/lib/set-gate.sh` for any gate flip:

```bash
bash "${SKILL_DIR}/scripts/lib/set-gate.sh" "$STATE_FILE" ledger_updated true register
```

It collapses the prior two-step pattern (`sed -i 's/false/true/'` +
`emit-gate-transition.sh ledger_updated register`) into a single call
inside a mutex with rollback on validation failure. Lock semantics:
mkdir-based mutex at `<file>.lock.d` (POSIX atomic, portable across
macOS and Linux without the `flock(1)` shell binary). State-file flip
uses temp-write + atomic-rename so concurrent readers never see
partial state.

Skill bodies must use `${SKILL_DIR}/...` paths (per the skill
self-containment rule) and `skill-bundles.yaml` must list both
`scripts/lib/set-gate.sh` and `scripts/lib/events-validate.sh` under
the consuming skill.

For non-gate events from shell, source `scripts/lib/events.sh` and
call `emit_event_raw TYPE PAYLOAD` (legacy shape) or build the canonical
envelope inline with `jq` and append.

### Python

Import the typed builders from `fno.events`:

```python
from fno import events as abilities_events

ev = abilities_events.phase_transition(
    gate="quality_check_passed",
    phase="review",
    nonce=state["provenance_nonce"],
    session_id=state["session_id"],
    source="abi-loop",
)
abilities_events.append_event(ev)
```

Builders use keyword-only arguments so unknown kwargs raise `TypeError`
at call time. `append_event` validates the event again before
acquiring the cross-language mkdir mutex on `events.jsonl.lock.d`. If
the schema YAML is missing or unparseable, `import fno.events`
raises `SchemaUnavailableError` (loud failure - callers cannot
silently proceed with malformed events).

### Telemetry-must-not-block

The `_emit_status_event` helper in `megatron/state.py` and
`_emit_gate_flip` in `loop.py` swallow exceptions with a defensive
`try/except`. The intent: a broken `events.jsonl` (filesystem error,
schema unavailable, something else) must NOT block the critical state
write the producer is wrapping. Audit-trail coverage is observability,
not a write dependency.

## Validating events

Both validators load the same YAML manifest and enforce the same
shape. Run them against the parity corpus on every PR:

```bash
pytest cli/tests/events/test_validator_parity.py -v
bash tests/events/test-bash-validator.sh
```

Either side drifting fails the parity test with a side-by-side
diagnostic naming which validator accepted vs rejected and the
rejection messages each produced.

## Migrating legacy files

`scripts/migrate-events-shape.py` is a one-shot stream rewriter that
walks every `events.jsonl` in the repo (root, cli/, artifacts/,
.claude/worktrees/*) and rewrites legacy `{timestamp, ...}` rows to
the canonical `{ts, ...}` envelope.

Properties:

- Idempotent: canonical-only files produce byte-for-byte equal output;
  no `.bak` is written when migrated count is zero.
- Stream processing: line-at-a-time, safe for million-row files.
- Corrupt-row tolerant: malformed JSON rows pass through verbatim and
  land in a sidecar `<file>.corrupt` log with line numbers; migration
  continues processing subsequent rows.
- Lock-shared with `set-gate.sh`: acquires `<file>.lock.d` via the
  same mkdir-based mutex so a live target session and a migration run
  cross-serialize.

Run once at ship time:

```bash
python3 scripts/migrate-events-shape.py
```

Pass `--dry-run` to see what would change without writing. Override
the lock timeout with `MIGRATE_LOCK_TIMEOUT_SECONDS=N` (defaults to 30).

## CI gates

Three checks that prevent regressions on the substrate:

| Check | Where | What |
|-------|-------|------|
| Parity test | `cli/tests/events/test_validator_parity.py` | Python and bash validators agree on every corpus row |
| `events-discipline.sh` | `scripts/lint/events-discipline.sh` | Catches bypass-echo, --soft outside hooks, unwrapped set-gate calls |
| `no-invalid-events.sh` | `scripts/lint/no-invalid-events.sh` | Fails CI when any `events.invalid.jsonl` is non-empty across repo + worktrees |

All three are wired into `cli-ci.yml` along with five bash test
harnesses (`test-bash-validator`, `test-set-gate`,
`test-target-ledger-set-gate`, `test-verify-child-promise`,
`test-events-discipline`).

## Adding a new event type

1. Add an entry under `event_types` in
   `cli/src/fno/events/schema.yaml`. Declare `data.required`
   and `data.properties`.
2. If the type is gate-bearing, add the gate name to `gates:`.
3. Add a typed builder in `cli/src/fno/events/__init__.py`
   following the existing `phase_transition` / `child_promise` shape.
   Builders use keyword-only args.
4. Add fixture rows to `cli/tests/events/parity_corpus.jsonl` covering
   happy path + at least one rejection case. The parity test runs
   both validators against every row and fails on any disagreement.
5. Update `__all__` in the events module if exporting new symbols.
6. Update this doc.

## Removing an event type

Add a `deprecated: <UTC ISO8601>` marker rather than deleting the
entry. The structural-validation test enforces marker shape; downstream
consumers get one release of warning. After consumers migrate off,
delete the entry in a follow-up PR.

## Phase rename history

The reasoning-phase names in the loop state machine were renamed in
2026-05-08:

- `produce` -> `blueprint`
- `review_fix` -> `review`

`events.jsonl` is append-only, so historical rows continue to carry the
old `phase: produce` and `phase: review_fix` strings indefinitely. New
emissions use the new names. Any reader that filters by phase string
must accept both the historical and current spellings (the schema
itself does not constrain the phase field). Today no reader in
`cli/src/`, `scripts/`, or `hooks/` actually filters on these phase
names, so no tolerance branch is required at the moment of the rename;
this note exists so a future reader knows not to narrow the filter back
down without restoring the old-name branch.

## Locked decisions (do not revisit)

These were locked during the design phase and codified by the spec
ship. Re-litigating any of them is out of scope:

1. Schema home: YAML at `cli/src/fno/events/schema.yaml`
   (language-neutral; both validators parse directly).
2. Two validators that both load the YAML; CI parity test catches
   drift.
3. `set_gate` migrates the load-bearing call site (target pre-promise
   `ledger_updated`) first; lint blocks new bare `sed` flips on gate
   fields.
4. Single canonical record shape `{ts, type, source, data}`. No
   `oneOf`, no compatibility branch.
5. Strict-default validation; hooks get explicit `--soft` mode (events
   route to `events.invalid.jsonl`; CI fails on non-empty).
6. Migration script shares the runtime lock with 30s timeout.
7. Gate-flip emissions ship in the substrate spec; audit-only
   `phase_entered`-style emissions are author discretion (per
   `gate_bearing: false` flag).
8. One event type with `gate_bearing: bool` flag, not two
   near-identical types.
9. Megatron mission events reference children via `data.session_id`;
   no separate parent-pointer field.
