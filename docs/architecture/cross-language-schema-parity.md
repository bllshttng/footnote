# Cross-Language Schema + CI Parity (Phase 6 W7)

The `fno-agents` Rust supervisor and the Python `footnote` package both write the same on-disk files (`~/.fno/agents/events.jsonl`, per-agent `state.json`). They were built independently, so their wire shapes drifted and nothing caught it. W7 formalizes the cross-language contract and makes it enforceable on every commit.

W7 is **document-and-guard**, not **unify**: it pins the current reality in versioned schemas, adds a drift/collision check, and gates the Rust test suite in CI. Collapsing the two envelopes into one byte-identical shape is deferred follow-up work (a breaking change for live Wave 1-4 consumers).

## The two envelopes

`events.jsonl` has two writers that emit structurally different envelopes:

| | Python (`footnote`) | Rust (`fno-agents` supervisor) |
|---|---|---|
| event-name field | `type` | `kind` |
| payload | nested under `data: {}` | flattened at top level |
| `source` values | `target`, `megawalk`, `hook`, ... (fixed enum) | `daemon`, `worker:<id>` (pattern) |
| size cap | 64KB (legacy YAML) | 500 bytes (`MAX_EVENT_PAYLOAD_BYTES`) |

These are both live. The contract accepts both rather than breaking either.

## Canonical schemas (in-repo)

The JSON Schemas live **in the repo** at `docs/architecture/schemas/`, NOT in the `~/your-vault` Obsidian vault. The original design placed them in the vault, but GitHub CI checks out the repo and cannot read the vault, so the parity check would be non-functional there. In-repo is the only location where both CI and the parity script can read them.

- `events-v3.json` - the `events.jsonl` envelope as a `oneOf` of two mutually-exclusive branches:
  - **Branch A** (Python): requires `ts, type, source, data`; `source` from the fixed enum; carries `not: {required: [kind]}`.
  - **Branch B** (Rust): requires `ts, kind, source`; `source` matches `^(daemon|worker:.+)$`; payload flat (`additionalProperties: true`); carries `not: {required: [type]}`.
  The two `not` guards make the branches disjoint: an event with both `type` and `kind`, or neither, matches zero branches and is rejected. A `$comment` records that the union is a documented bridge, not an accident.
- `status-v1.json` - per-agent `state.json`, derived from the Rust `AgentState` struct (`crates/fno-agents/src/state.rs`). Required: `schema_version, short_id, status`; `status` is the 10-value `AgentStatus` enum; `pty` mirrors the flat `PtyStateWire` projection.

`docs/architecture/events-schema.yaml` (the older per-type Python contract, consumed by `scripts/lib/events-validate.sh` and the Python validator) is reconciled **additively**: the Rust event kinds and the `daemon` source are documented there so live Rust events stop reading as undocumented. No existing entry changed.

## `--emit-schema` introspection

Each language can print the schema it believes it conforms to, so the parity check can diff actual-vs-canonical:

- Rust: `fno-agents --emit-schema` prints Branch B + `status-v1` + the `KNOWN_EVENT_KINDS` list.
- Python: `python -m fno.events --emit-schema` prints Branch A + the event-type names read from `events-schema.yaml`.

Both are read-only, side-effect-free, and idempotent.

## Drift + collision check

`scripts/check-event-schema-parity.sh`:

1. Validates `events-v3.json` + `status-v1.json` parse as JSON Schema.
2. Runs each language's `--emit-schema` (30s timeout each) and diffs the output against the matching on-disk branch. A non-zero exit, non-JSON output, or a timeout is a **failure**, never a silent pass.
3. Asserts the event-name namespaces (`type` names ∪ `kind` names) are globally unique - no name may mean two payloads.
4. Degrades gracefully: when the `fno-agents` binary is absent (e.g. a Python-only or pre-commit context) it prints a WARN and exits 0 after still validating the Python side. The Rust CI job, which builds the binary, is the real gate for the Rust half.

`scripts/tests/check-event-schema-parity-selftest.sh` feeds the script synthetic drift, collision, and malformed-schema fixtures and asserts it exits non-zero - so a regression in the check's own logic is itself caught. It runs in `cli-ci.yml`.

## CI

- `.github/workflows/rust-ci.yml` (new): on `crates/**` / schema / parity-script changes, installs the stable toolchain and runs `cargo test --all-targets` for `crates/fno-agents` - this is what finally makes `tests/flock_interop.rs` (the Python-`fcntl`-vs-Rust-`fs2` interop kill criterion) gate every commit. It then runs the parity check.
- `cli-ci.yml`: runs the parity check and the self-test on Python changes, so a Python-side schema break is caught even without a `crates/**` change.

## How to add a new cross-language event

1. **Emit it.** Rust: `emitter.emit("my_new_kind", &payload)`. Add `"my_new_kind"` to `KNOWN_EVENT_KINDS` in `crates/fno-agents/src/lib.rs` (and to `emit_schema_json()` if it embeds the list). The list is hand-maintained; keep it in sync with every `.emit(...)` call site. Python: emit with a unique `type` not already used by any Rust `kind`.
2. **Document it.** Add an additive `event_types` entry in `docs/architecture/events-schema.yaml` with `sources`, a one-line description, and a minimal `data` shape.
3. **Keep payloads under 500 bytes.** Larger payloads use the evidence-pointer pattern (put the path in the event, the content in a separate file).
4. **Run the check.** `bash scripts/check-event-schema-parity.sh` must print `parity OK`. If you renamed a field or changed the envelope shape, bump the schema major version (`events-v3` -> `events-v4`) and release both languages together.

> Known limitation: `KNOWN_EVENT_KINDS` is a hand-maintained list, so it can drift from the actual `.emit()` call sites. A source-scanning completeness test (or a `schemars`-derived schema) would make this drift caught automatically; tracked as follow-up.
