# Cross-Language Schema + CI Parity (Phase 6 W7)

The `fno-agents` Rust supervisor and the Python `footnote` package both write the same on-disk files (`~/.fno/agents/events.jsonl`, per-agent `state.json`). They were built independently, so their wire shapes drifted and nothing caught it. W7 formalizes the cross-language contract and makes it enforceable on every commit.

W7 was **document-and-guard**, not **unify**: it pinned the then-current reality in versioned schemas, added a drift/collision check, and gated the Rust test suite in CI. The unify step it deferred has since landed - `schemas/events-v3.json` is now a single envelope, per its own `$comment` - so read the two-envelope material below as history.

## The two envelopes

> **Historical.** The split described in this section was retired: `events-v3.json` now defines a single envelope (`required: [ts, type, source, data]`) that both writers use, per the file's own `$comment`. Read this table as the shape the parity machinery was originally built against, not as current behavior.

`events.jsonl` had two writers that emitted structurally different envelopes:

| | Python (`footnote`) | Rust (`fno-agents` supervisor) |
|---|---|---|
| event-name field | `type` | `kind` |
| payload | nested under `data: {}` | flattened at top level |
| `source` values | `target`, `megawalk`, `hook`, ... (fixed enum) | `daemon`, `worker:<id>` (pattern) |
| size cap | 64KB (legacy YAML) | 500 bytes (`MAX_EVENT_PAYLOAD_BYTES`) |

Both were live at the time, and the contract accepted both rather than breaking either.

## Canonical schemas (in-repo)

The JSON Schemas live **in the repo** at `schemas/`, NOT in the `~/your-vault` Obsidian vault. The original design placed them in the vault, but GitHub CI checks out the repo and cannot read the vault, so the parity check would be non-functional there. In-repo is the only location where both CI and the parity script can read them.

- `events-v3.json` - the `events.jsonl` envelope. **The two-branch `oneOf` described below is retired.** Both emitters now write one envelope, `required: [ts, type, source, data]`, and the file's own `$comment` says so; the Rust `{ts, kind, <flat>}` branch and its `kind` enum are gone. `schema.yaml` is now the single cross-language name registry. The history is kept here because the sections downstream still speak of branches:
  - **Branch A** (Python, now the only shape): required `ts, type, source, data`; `source` from the fixed enum.
  - **Branch B** (Rust, retired): required `ts, kind, source`; payload flat. Each branch carried a `not: {required: [...]}` guard making the two disjoint, so an event with both `type` and `kind`, or neither, matched zero branches.
- `status-v1.json` - per-agent `state.json`, derived from the Rust `AgentState` struct (`crates/fno-agents/src/state.rs`). Required: `schema_version, short_id, status`; `status` is the 10-value `AgentStatus` enum; `pty` mirrors the flat `PtyStateWire` projection.

`cli/src/fno/events/schema.yaml` (the older per-type Python contract, consumed by `scripts/lib/events-validate.sh` and the Python validator) is reconciled **additively**: the Rust event kinds and the `daemon` source are documented there so live Rust events stop reading as undocumented. No existing entry changed.

## `--emit-schema` introspection

Each language can print the schema it believes it conforms to, so the parity check can diff actual-vs-canonical:

- Rust: `fno-agents --emit-schema` prints its envelope + `status-v1` + the `KNOWN_EVENT_KINDS` list.
- Python: `python -m fno.events --emit-schema` prints the envelope + the event-type names read from `schema.yaml`.

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

1. **Emit it.** Rust: `emitter.emit("my_new_kind", &payload)`. Add `"my_new_kind"` to `KNOWN_EVENT_KINDS` in `crates/fno-agents/src/lib.rs` (and to `emit_schema_json()` if it embeds the list). You do not have to remember this: `every_production_emit_kind_is_registered` scans every production `.emit(` call site and reds on a kind that was never registered. Python: emit with a unique `type` not already used by any Rust `kind`.
2. **Document it.** Add an additive `event_types` entry in `cli/src/fno/events/schema.yaml` with `sources`, a one-line description, and a minimal `data` shape.
3. **Keep payloads under 500 bytes.** Larger payloads use the evidence-pointer pattern (put the path in the event, the content in a separate file).
4. **Run the check.** `bash scripts/check-event-schema-parity.sh` must print `parity OK`. If you renamed a field or changed the envelope shape, bump the schema major version (`events-v3` -> `events-v4`) and release both languages together.

The drift this section used to warn about is now closed, and it is worth knowing which test closes which part of it:

- **Rust const vs Rust call sites:** `every_production_emit_kind_is_registered` (`crates/fno-agents/src/lib.rs`) scans every production `.emit(` and fails on an unregistered kind. It truncates each file at the first `#[cfg(test)]` so test fixtures do not register themselves.
- **Rust const vs `schema.yaml`:** step 6 of `scripts/check-event-schema-parity.sh` asserts `KNOWN_EVENT_KINDS` is a subset of the documented `event_types`. Subset, not equality, because `schema.yaml` also carries Python types and loop-runtime `type` events that never get a const entry. It needs the built binary, so it runs on the `rust-ci.yml` leg.
- **The Python-side view of the const:** `cli/tests/events/test_rust_events_documented.py` parses the const out of `lib.rs` rather than mirroring it. It previously kept a hand-written copy, which drifted 16 entries behind and stayed green for six weeks, because every assertion there is additive-only: a kind missing from the copy was a kind nothing asserted about. A `>= 40`-entry floor makes a broken parse loud instead of vacuous.

A hand-maintained list with a comment asking the next person to remember is the thing all three of these replaced.

## Not everything cross-language needs parity: plan readiness

`document-and-guard` is this doc's answer for a contract with **two implementations**. Plan readiness looked like a member of that class and is not, which is worth recording so the next design does not re-derive the wrong plan.

A node's plan sits on a rung (`idea` -> `design` -> `ready` -> `in_progress` -> `in_review`, plus the `done`/`superseded` terminals).
One Python function classifies it: `plan_rung` in `cli/src/fno/graph/ladder.py`.
Autonomous selection reads that classification through `selection_guards`, while `is_dispatchable` and `is_cold_dispatchable` own the plan-bearing and plan-less dispatch policies.
Bash asks the classifier through `fno do plan rung`.
**Rust never parses it.**

That last sentence is the finding. It reads as though `crates/fno-agents/src/loopcheck.rs` parses plan-frontmatter status, and it does parse a `status:` key - but out of `.fno/target-state.md`, whose vocabulary is `COMPLETE | BLOCKED | ABORTED`, a different axis entirely. Reading the rest of the Rust surface:

| Rust site | What it actually does |
|---|---|
| `loopcheck.rs`, `loop_target.rs` | parse `.fno/target-state.md` (session manifest vocabulary) |
| `finalize.rs` | shells out to `fno do plan validate` / `fno do plan stamp` |
| `loop_megawalk.rs` | takes `plan_path` from `fno backlog next` JSON, whose status Python already derived |
| `kill_criteria.rs` | opens the plan document, extracts the `kill_criteria:` block only |
| `backlog_view.rs` | renders the derived graph status; consumes, never re-derives |

So there is no second implementation. A fixture-corpus parity harness would have frozen a contract with one participant, and its green would have meant nothing.

What can actually regress is someone **adding** a Rust plan-status reader, so that is what `scripts/ci/check-plan-rung-authority.sh` guards: it freezes the set of Rust sources that open a plan document (`kill_criteria.rs` alone), asserts that file never grows a frontmatter `status` extraction, asserts the rung table lives in exactly one Python module, requires the real dispatch policies while rejecting the removed decorative `is_selectable` policy, and fails on any shell script that classifies a plan status itself.

**The rule this generalizes to:** before writing a parity check, enumerate the implementations. Parity is for two or more that must agree. One implementation plus N delegating callers needs a *uniqueness* guard instead - it is a cheaper artifact and it fails for the right reason.

> The check reads its file set from `git ls-files`, never a directory walk. ripgrep layers `.gitignore`, `~/.ignore` and `.rgignore` over any traversal, and a global `target/` rule for build output also swallows `skills/target/` - the single most important directory the check covers. That version passed, silently searching nothing.
