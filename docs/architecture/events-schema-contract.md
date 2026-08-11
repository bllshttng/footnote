---
created: 2026-06-18T11:54
updated: 2026-08-11T12:00
status: approved
---

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
        +-> producer call sites                     (target, megatron, fno-loop)
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

Branch-A Python, Rust claims, and loop-journal writers serialize on the same mkdir mutex at `<events-file>.lock.d`.
When `events.jsonl` is a worktree symlink, every writer resolves the leaf before deriving that mutex so sibling worktrees lock the canonical target rather than separate worktree paths.
The project journal treats a lock timeout as fatal, while its global observability mirror remains best-effort so daemon progress does not depend on that mirror.
The three fixed-shape shell helpers remain unlocked and reject serialized rows above 4000 bytes before appending.
They register unique entries under `<events-file>.shell-writers.d`, recheck `<events-file>.gc.d`, and remove the entry after appending, so they remain unserialized with each other while compaction can prove no append targets the old inode.

`schemas/events-v3.json` is the JSON-Schema mirror of this envelope,
used by the cross-language parity gate; `cli/src/fno/events/schema.yaml`
is the source of truth both validators load.

### One envelope, both languages

For a window, the Rust `fno-agents` daemon wrote a second envelope shape
(`{ts, kind, <flat fields>}`) and `events-v3.json` carried a `oneOf`
bridge between it and the canonical `{ts, type, source, data}`.
Publishing the public event contract on top of that split-brain would
have frozen it in, so it was retired: the daemon's `EventEmitter` now
emits the single canonical envelope, and `events-v3.json` is one branch
again (restoring locked decision 4).

Because `events.jsonl` is append-only and `fno-agents` deploys as a
binary pair, the retired shape can still appear on the wire during a
rollout: a running daemon keeps emitting `kind` lines until `fno
restart`, and rotated `events.jsonl.1` history holds old lines. The two
Rust readers (`subscribe::classify`, `digest`) therefore keep a
read-side fallback: they read `type` with a `kind` fallback and the
payload from `data` with a flat fallback. Each fallback site carries its
removal criterion in a comment - drop it once the daemon fleet has
restarted on the post-cut binary and no rotated file carries a `kind`
line. Emitting the old shape is no longer valid; only reading it is
tolerated.

### Source enum

| Source | Producers |
|--------|-----------|
| `target` | the target skill / pre-promise sequence |
| `megawalk` | the megawalk stop hook + roadmap loop |
| `megatron` | mission lifecycle events |
| `fno-loop` | HISTORICAL: the pre-wedge headless `fno loop` driver (verb removed in step-5 group 3); the source value survives in old journals |
| `hook` | every other in-tree hook (PostToolUse, PreToolUse, etc.) |
| `subagent` | reserved for direct subagent emissions |
| `migration` | the one-shot `scripts/migrate-events-shape.py` |
| `test` | test-only fixtures |
| `backlog` | `fno backlog` graph mutations (done/refused/forced, capture, triage) |
| `daemon` | the Rust `fno-agents` supervisor daemon (agent/drive/reconcile lifecycle) |
| `active-backlog` | the daemon's active-backlog dispatch task |
| `observer` | the skill-eval observer harness |
| `skill_diff` | the skill-diff proposer loop |

Plus two **pattern** sources for per-agent Rust workers, matched by
`envelope.properties.source.patterns` rather than the enum:

| Pattern | Producers |
|---------|-----------|
| `worker:<id>` | a per-agent worker process |
| `stream-worker:<id>` | a claude stream-json worker |

Adding a fixed-string source means editing the YAML manifest's
`source.enum` + adding a `parity_corpus.jsonl` row. A new worker-style
source adds a regex to `source.patterns`. Both validators (Python
`validate()` and `events-validate.sh`) accept a source that matches the
enum OR any pattern; CI catches missed updates.

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

### `verification_receipt`

A verification receipt is the append-only, schema-owned record of what was actually executed against one candidate commit.
It binds a full 40-hex `candidate_sha`, the exact command argument vector, host/platform/runner environment, named scope, start and finish timestamps, evidence mode, result, producer identity, a positive journal-derived per-candidate generation, and expected versus executed step counts.
The command, scope, payload, and string sizes are bounded before the receipt can enter a journal.

Modes and results remain orthogonal and explicit:

| Field | Values | Meaning |
|---|---|---|
| `mode` | `full`, `subset`, `void`, `advisory` | The coverage class that was attempted. |
| `result` | `not_configured`, `unavailable`, `pending`, `failed`, `passed`, `stale` | The observed outcome without collapsing absence or uncertainty into success. |

A full passed receipt is structurally valid only when the scope is non-empty, `steps_expected` equals the number of named scope items, and every expected step executed.
A void receipt can never be passed.
Explicit false, absent, pending, unavailable, stale, and advisory observations are therefore not interchangeable, and unknown evidence fails closed.

Python validation in `fno.events`, Bash validation in `scripts/lib/events-validate.sh`, and Rust validation in `fno-agents verify-evidence receipt` enforce the same receipt vocabulary and invariants.
The Rust reducer takes the canonical journal first and any diagnostic mirror journals afterward.
The parity corpus includes fractional UTC timestamps and bounded-command failures so wire-compatible RFC3339 values and size limits do not drift between implementations.

`scripts/ci/preflight.sh` remains the authority for actual local execution and constructs, schema-validates, and appends each receipt inside the process that holds the shared preflight lock for the exact candidate SHA.
It commits the canonical event to the global journal first and mirrors the same event best-effort to the delivery-root journal, so unregistered worktrees and independent local clones share one durable generation floor without a split-brain second commit point.
The canonical journal is pinned beside the cross-project ledger, so a supported relative `state_dir` cannot fork it by checkout.
No CLI accepts caller-authored trusted receipt fields, and the generic event emitter refuses `verification_receipt`.
Its required deterministic scope is `smoke`, both Rust format checks, and both Rust test suites; the squads leak guard joins the full scope only when squads exist to measure, while a not-configured guard is recorded separately as advisory evidence.
The two cargo-audit scopes are also separate advisory evidence, and every receipt counts only steps that actually executed.
Setup failures emit a zero-executed void/unavailable receipt instead of manufacturing a verdict.
The preflight attestation is only a cache carrier and never substitutes for the event receipt.

Ship-gate consumers accept only the newest exact-SHA full/passed receipt from the trusted preflight producer with the canonical command, runner, host-bound producer identity, and complete deterministic scope.
Before every append, preflight derives the next generation as one above the highest exact-SHA generation across readable project, global, delivery-root, and salvage journals.
Until the canonical journal contains exact-SHA evidence, malformed or unreadable discovery blocks the append; afterward, an unavailable best-effort mirror cannot overrule the canonical floor, while every readable mirror remains part of deduplication and conflict detection.
Mirrors never originate satisfaction or supersede canonical evidence; a missing exact-SHA canonical receipt is unavailable, and a readable mirror ahead of the canonical generation is explicit drift that fails closed.
Reducers expose skipped carrier failures as `coverage.unavailable_mirrors` instead of conflating them with an absent journal or a complete canonical failure.
This makes lost local state safe across clones, while simultaneous cross-clone allocation creates an explicit same-generation conflict rather than an arbitrary winner.
Journals may be concatenated in any order, so consumers parse RFC3339 timestamps, deduplicate canonical event objects, and select the highest exact-candidate generation.
Timestamps retain at most six fractional digits and remain observational metadata; future-dated receipts and distinct receipts tied for the highest generation are unavailable rather than arbitrarily ordered.
Starting a new preflight holds a pinned cross-checkout lock for the exact candidate and appends a canonical pending receipt before execution, so concurrent clones cannot create unmatched transitions and a failed final append or interrupted run leaves every checkout on the same non-passing generation without a mutable revocation authority.
Ship-gate readers take the repository lock while reducing journals, and the append-only pending-to-final transition keeps the receipt journal as the sole verdict authority.
Malformed canonical rows, unreadable canonical journals, malformed readable mirrors, or errors while discovering delivery journals mark coverage incomplete and prevent satisfaction even when another journal contains a plausible pass.
An older valid receipt for a different SHA is reported as stale rather than absent.
Local ship paths apply one shared requirement policy: explicit skip, documentation-only changes, and repositories without a configured preflight runner remain distinct exemptions; every other candidate must present trusted receipt evidence.

## Retention and garbage collection

The schema classifies event types as `ephemeral`, `gate`, or `durable`, and an omitted or unknown classification fails closed to `durable`.
Ephemeral rows are high-volume operational evidence, gate rows participate in current-HEAD decisions, and durable rows preserve long-lived lineage.
Declared join pairs must use the same retention class, and schema loading fails if either side is missing or the classes differ.

The minimum ephemeral horizon is 672 hours because `human_touch` and claim context feed the scoreboard's default 28-day window.
`fno event gc` refuses a shorter horizon and deletes only expired rows explicitly marked `ephemeral`.
It preserves gate, durable, undeclared, unknown, and malformed rows, including a malformed row whose timestamp cannot be evaluated.
The collector rewrites through an fsynced same-directory temporary file while holding the shared writer mutex, a shell-visible GC marker, and an empty shell-writer rendezvous.
When a worktree journal is a symlink, collection resolves and rewrites its target without replacing the symlink.

```bash
fno event gc --dry-run
fno event gc
fno event gc --events /path/to/.fno/events.jsonl --ttl-hours 672
```

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
from fno import events as fno_events

ev = fno_events.phase_transition(
    gate="quality_check_passed",
    phase="review",
    nonce=state["provenance_nonce"],
    session_id=state["session_id"],
    source="fno-loop",
)
fno_events.append_event(ev)
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
fno test cli/tests/events/test_validator_parity.py
bash tests/events/test-bash-validator.sh
cargo test --manifest-path crates/fno-agents/Cargo.toml verify_evidence::tests --lib
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
- Symlink-safe: resolves shared worktree journals before deriving sidecars or replacing bytes, deduplicates their canonical targets, and preserves each worktree symlink.

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
