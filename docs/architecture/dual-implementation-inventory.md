# Dual-implementation inventory and the port protocol

The operator ruling: if something touches Python when it can also be written in Rust only, make it Rust only. Something implemented twice is a port candidate.

This page answers three questions. Which cross-tree guards mark a real second implementation. What the procedure is for retiring one. Why most of them are not port candidates at all.

## The discriminator

A parity guard exists because two things could drift. It does not follow that two IMPLEMENTATIONS exist.

Three kinds, and only the first is a port candidate.

**dual-logic.** Two hand-written implementations of the same behavior, in two languages. A port deletes one leg and the guard converts to a characterization test. This is the only kind the operator ruling is about.

**shared-vocabulary.** One concept declared, spelled, or stamped at many sites. The guard checks that the sites agree. Porting one site does not retire the guard, because the concept still has to be spelled the same everywhere it appears, and several sites are not code at all: a workflow YAML, a ruleset data file, a setup-doc filename. A guard whose surfaces include a non-code artifact can never be retired by a port.

**generated-artifact.** One owner, a generated copy, and a freshness tripwire. This is CORRECT and must not be "fixed". The exemplar is `harness_capabilities.toml`: the Rust tree owns the canonical file, `build.rs` generates the Python copy on every build, and `scripts/ci/check-harness-capabilities-fresh.sh` catches a stale copy. One owner, one generated artifact, one guard. A generated data file is not a second implementation.

The failure this page exists to prevent is reading a filename as evidence. A file called `*_parity.rs` looks like a live second implementation and often is not. The inventory below was filed wrong twice for exactly that reason, and the guard in the last section makes the distinction machine-readable.

## The parity tests

Measured by reading each file's header and then testing whether its oracle still exists on disk.

| Parity file | Oracle | On disk | Verdict |
|---|---|---|---|
| `crates/fno-agents/tests/claude_ask_parity.rs` | `fno.agents.harnesses.claude` | present | dual-logic |
| `crates/fno-agents/tests/codex_ask_parity.rs` | `fno.agents.harnesses.codex` | present | dual-logic |
| `crates/fno-agents/tests/kill_criteria_parity.rs` | `scripts/lib/kill-criteria.sh` | absent | characterization, port complete |
| `crates/fno-agents/tests/verify_evidence_parity.rs` | `scripts/lib/verify-event-evidence.sh` | absent | characterization, port complete |

Two of the four are finished ports. Their bash oracles were deleted and each case now asserts against a golden captured before the deletion. They are at step 4 of the protocol below, not awaiting one.

`kill_criteria` in particular is not a dual implementation, and the reason is worth stating because the mistake is easy to repeat. The Python files under `cli/src/fno/plan/` validate the kill-criteria DECLARATION against a schema. The Rust `kill-check` verb EVALUATES the predicates at a wave boundary. Two functions that share a vocabulary. The parity test never pointed at that Python; it points at a bash oracle that is gone.

## The real dual set

Three, in the order they should be retired.

**The claim classifier.** Dual and UNGUARDED, which is why it goes first. Nothing pins it, so it can drift silently while the other two cannot.

**The ask adapters.** `fno.agents.harnesses.claude` and `fno.agents.harnesses.codex`, pinned by the two differential harnesses above. `claude_ask_parity.rs` runs the genuine Python `_build_envelope` and `parse_short_id` and asserts byte-identity. `codex_ask_parity.rs` drives one fake `codex` binary through both legs and asserts reply text, exit code and event fields. Both skip rather than fail when `python3` is unavailable. This is one port with one production caller: `cli/src/fno/agents/cli.py`, the body of `fno agents ask`.

Nothing else in the tree is a confirmed dual implementation today.

## The eight CI parity scripts

All eight are shared-vocabulary. None is a port candidate, and the reasons differ enough to be worth stating one at a time.

| Script | Compares | Verdict |
|---|---|---|
| `check-coverage-context-parity.sh` | two commit-status context strings and one label across the Python publisher, the operator ruleset data, the Rust publisher, and the refresher workflow | shared-vocabulary |
| `check-harness-roster-parity.py` | `KNOWN_HARNESSES` against three evidence surfaces: `docs/SETUP-<name>.md` filenames, the `for_name()` match arms in `provider.rs`, and the `_register()` calls in the Python adapter registry | shared-vocabulary |
| `check-provider-vocabulary-parity.sh` | provider vocabulary across four Rust files and two Python files | shared-vocabulary |
| `check-registry-schema-parity.sh` | `REGISTRY_SCHEMA_VERSION` in `state.rs` against `SCHEMA_VERSION` in `registry.py` | shared-vocabulary |
| `check-review-app-parity.sh` | review-App logins across `BOT_PROFILES` in Rust and two separate Python declarations, plus the presence of a `usage_markers` field per profile | shared-vocabulary |
| `check-reviewer-descriptor-parity.sh` | `_RESOLVABLE_REVIEWERS` against `REVIEWER_INVOCATIONS`: invocation string, self-cert flag, per-harness verb overrides | shared-vocabulary |
| `check-session-identity-parity.sh` | the identity-writing surfaces in two Rust files and the Python registry | shared-vocabulary |
| `check-spawn-lineage-parity.sh` | that every registry mint site stamps the `spawned_by_*` parent edge, across eight Rust files and four Python files | shared-vocabulary |

Three of these deserve a note, because a reader in a hurry could mistake them for port candidates.

`check-registry-schema-parity.sh` compares a version constant, not logic. Two implementations read and write one on-disk store and must agree on the write-version. The guard is about the STORE's contract. It survives any port that leaves two readers.

`check-spawn-lineage-parity.sh` is an invariant gate, not a parity gate despite its name. It asserts that twelve separate mint sites each stamp a field. Collapsing the four Python sites into Rust would leave eight sites and the same gate, because the thing that drifts is a site forgetting the stamp, not two languages disagreeing.

`check-coverage-context-parity.sh` and `check-harness-roster-parity.py` both compare code against artifacts that are not code: a GitHub workflow, operator ruleset data, and setup-doc filenames. No port can retire either one.

`check-reviewer-descriptor-parity.sh` is the closest of the eight to a port candidate. It compares one declaration set held twice in two languages, and a port of the config layer would delete one side. It is still not dual LOGIC, and the config layer is not on the sequence below.

## The port protocol

Use this and do not invent another. The repo already proved it on two migrations, and the property that makes it good is that the safety net survives the deletion instead of dying with it.

1. **Differential parity while both legs exist.** Run both implementations over identical fixtures and assert byte-equality.
2. **Capture goldens from the proven-correct old leg, before touching it.** Under `FNO_CAPTURE_GOLDEN=1` the helper runs the old leg, writes the goldens, and asserts new equals old before freezing. Goldens live at `crates/fno-agents/tests/golden/<subject>/<case>.{exit,out,err}`, keyed by a slug of the case label.
3. **Delete the old leg and move its callers.** Both, in the same change.
4. **Convert the parity test to a characterization test** asserting against the goldens captured in step 2.

Never delete a leg before proving it is unreachable. The ask adapters are live in production. A port is a migration with callers to move, never a cleanup. `verify_evidence` was free only because its counterpart was already gone.

A test that skips when the other language is unavailable stays a skip, never a failure. `claude_ask_parity.rs` shows the shape.

## The provenance declaration

Every `*_parity.rs` declares its stage and its oracle in its header, and `scripts/ci/check-parity-test-provenance.sh` asserts the declaration against the filesystem in both directions.

```rust
//! parity-stage: differential
//! parity-oracle: fno.agents.harnesses.claude
```

`parity-stage` is exactly one of `differential` or `characterization`. `parity-oracle` is a repo-relative path or a Python dotted module, and nothing else.

The check is two-sided on purpose. A `characterization` file must name an oracle that does NOT exist, because the leg was deleted and the golden stands in for it. A `differential` file must name one that DOES exist, because a live second implementation is the only reason to run both. Either way the assertion has a positive marker rather than an absence, and either way it fails loudly the moment a port finishes or a new dual implementation appears.

The header carries no node id and no PR number. `scripts/ci/check-no-internal-refs.sh` fails on them. The oracle path is the identity.

## Sequence

The claim classifier first, because it is the only dual implementation with no parity guard pinning it. A guarded leg cannot drift silently; an unguarded one can.

The ask adapters second. They are already at step 1 with both harnesses passing, so the work is steps 2 through 4 plus moving one caller.

Nothing third. Do not open a `kill_criteria` port under this sequence: the table above removed it from the dual set, and filing one would re-import the error this page corrects.

One deferred item sits in the family without joining the sequence, and the reason generalizes. The tier-remap refusal exists in the Python spawn launcher and not in the Rust spawn binary. That is a guard MISSING from a second implementation, not a second implementation to retire, and writing it a second time in the second language would commit the exact pattern this page exists to remove. It closes for free when the spawn entry point collapses to one implementation. A missing guard on a duplicated entry point is an argument for finishing the port, never for duplicating the guard.
