# Dual-implementation inventory and the port protocol

The operator ruling. When either trigger fires, port a Python behavior to Rust.

1. DUPLICATION. One behavior is hand-written twice, in two languages. This is the original rule, and the dual-logic kind below.
2. PREVENTION. The port removes a class of defect that a type system makes unrepresentable rather than merely detectable. The qualifying classes are in the second-trigger section below.

Neither trigger reaches a shared-vocabulary or a generated-artifact guard. Those stay excluded regardless of benefit, because porting them retires nothing and a generated-artifact tripwire is correct as built.

This page answers four questions. Which cross-tree guards mark a real second implementation. What the procedure is for retiring one. Why most of these guards are not port candidates at all. When a port is justified without duplication.

## The discriminator

A parity guard exists because two things can drift. Two implementations do not follow from that.

There are three kinds. Only the first is a port candidate.

**dual-logic.** Two hand-written implementations of one behavior, in two languages. A port deletes one leg. The guard then converts to a characterization test. This is the only kind the operator ruling is about.

**shared-vocabulary.** One concept declared, spelled, or stamped at many sites. The guard checks that the sites agree. Porting one site does not retire the guard. The concept still has to be spelled the same way everywhere it appears. Several sites are not code at all. A workflow YAML, a ruleset data file, and a setup-doc filename are all such sites. When a guard's surfaces include a non-code artifact, no port can ever retire it.

**generated-artifact.** One owner, a generated copy, and a freshness tripwire. This is correct and must not be "fixed". The exemplar is `harness_capabilities.toml`. The Rust tree owns the canonical file. `build.rs` generates the Python copy on every build. `scripts/ci/check-harness-capabilities-fresh.sh` catches a stale copy. One owner, one generated artifact, one guard. A generated data file is not a second implementation.

This page exists to prevent one failure: reading a filename as evidence. A file called `*_parity.rs` looks like a live second implementation. Often it is not. The inventory below was filed wrong twice for that reason. The guard in the last section makes the distinction machine-readable.

## The second trigger: prevention

Duplication is not the only reason a port pays. The second trigger is prevention: the port removes a class of defect that a type system makes unrepresentable rather than merely detectable. Detection-only fixes leave the wrong answer still writable. A type that cannot express the wrong answer ends the class. Every case below is one measured incident of the same failure class: an instrument returning a value indistinguishable from a real answer.

The provenance case. Two docstrings stated opposite meanings for one registry lookup: one said a hit proves the id is NOT ours, the other that it IS ours. Both were confident and both were load-bearing. They reconciled only on the provenance of the input, which nothing encoded. Three workers were refused their own node before anyone read both docstrings.

The exhaustiveness case. A capability table recorded agy and opencode as unsupported with no justification, while pi's row carried a measured reason. One word was doing two jobs, measured-absent and never-checked, and no reader can tell them apart.

The rename-safety case. The substrate vocabulary moved from `bg` to `thread`, and a literal comparison against `bg` survived the rename. Every autonomous dispatch then reported it went headless while it spawned a thread, and emitted a false fallback event with it.

Each case earns the port through the type system. Provenance moves into a type instead of a docstring. A closed vocabulary forces every entry to carry its reason. An enum turns a rename into a compile error at every surviving site instead of a false comparison at one.

The scope guard. Prevention is a second trigger for porting, never a third kind of guard. The discriminator above still decides what a dual implementation is, and its exclusions survive this trigger unchanged. A prevention port follows the same four-step protocol as a duplication port. The sequence section orders duplication ports only. A prevention port is scheduled on its own measured case, never by that sequence.

## The parity tests

Measured by reading each file's header, then testing whether its oracle still exists on disk.

| Parity file | Oracle | On disk | Verdict |
|---|---|---|---|
| `crates/fno-agents/tests/claude_ask_parity.rs` | `fno.agents.harnesses.claude` | present | dual-logic |
| `crates/fno-agents/tests/codex_ask_parity.rs` | `fno.agents.harnesses.codex` | present | dual-logic |
| `crates/fno-agents/tests/kill_criteria_parity.rs` | `scripts/lib/kill-criteria.sh` | absent | characterization, port complete |
| `crates/fno-agents/tests/verify_evidence_parity.rs` | `scripts/lib/verify-event-evidence.sh` | absent | characterization, port complete |

Two of the four are finished ports. Their bash oracles were deleted. Each case now asserts against a golden captured before that deletion. They sit at step 4 of the protocol below. They do not await one.

`kill_criteria` is not a dual implementation. State the reason, because the mistake is easy to repeat. The Python files under `cli/src/fno/plan/` validate the kill-criteria DECLARATION against a schema. The Rust `kill-check` verb EVALUATES the predicates at a wave boundary. Two functions share a vocabulary. The parity test never pointed at that Python. It points at a bash oracle that is gone.

## The real dual set

There are three, in retirement order.

**The claim classifier.** Dual and UNGUARDED. It goes first for that reason. Nothing pins it, so it can drift silently. The other two cannot.

**The ask adapters.** `fno.agents.harnesses.claude` and `fno.agents.harnesses.codex`, pinned by the two differential harnesses above. `claude_ask_parity.rs` runs the genuine Python `_build_envelope` and `parse_short_id` and asserts byte-identity. `codex_ask_parity.rs` drives one fake `codex` binary through both legs. It asserts reply text, exit code, and event fields. This is one port with one production caller. That caller is `cli/src/fno/agents/cli.py`, the body of `fno agents ask`.

Nothing else in the tree is a confirmed dual implementation today.

## The eight CI parity scripts

All eight are shared-vocabulary. None is a port candidate. The reasons differ enough to state one at a time.

| Script | Compares | Verdict |
|---|---|---|
| `check-coverage-context-parity.sh` | two commit-status context strings and one label, across the Python publisher, the operator ruleset data, the Rust publisher, and the refresher workflow | shared-vocabulary |
| `check-harness-roster-parity.py` | `KNOWN_HARNESSES` against three evidence surfaces: `docs/SETUP-<name>.md` filenames, the `for_name()` match arms in `provider.rs`, and the `_register()` calls in the Python adapter registry | shared-vocabulary |
| `check-provider-vocabulary-parity.sh` | provider vocabulary across four Rust files and two Python files | shared-vocabulary |
| `check-registry-schema-parity.sh` | `REGISTRY_SCHEMA_VERSION` in `state.rs` against `SCHEMA_VERSION` in `registry.py` | shared-vocabulary |
| `check-review-app-parity.sh` | review-App logins across `BOT_PROFILES` in Rust and two Python declarations, plus a `usage_markers` field per profile | shared-vocabulary |
| `check-reviewer-descriptor-parity.sh` | `_RESOLVABLE_REVIEWERS` against `REVIEWER_INVOCATIONS`: invocation string, self-cert flag, per-harness verb overrides | shared-vocabulary |
| `check-session-identity-parity.sh` | the identity-writing surfaces in two Rust files and the Python registry | shared-vocabulary |
| `check-spawn-lineage-parity.sh` | that every registry mint site stamps the `spawned_by_*` parent edge, across eight Rust files and four Python files | shared-vocabulary |

Three of these need a note. A reader in a hurry can mistake them for port candidates.

`check-registry-schema-parity.sh` compares a version constant, not logic. Two implementations read and write one on-disk store. They must agree on the write-version. The guard is about the STORE's contract. It survives any port that leaves two readers.

`check-spawn-lineage-parity.sh` is an invariant gate, not a parity gate, despite its name. It asserts that twelve separate mint sites each stamp a field. Collapse the four Python sites into Rust and eight sites remain, under the same gate. The thing that drifts is a site forgetting the stamp, not two languages disagreeing.

`check-coverage-context-parity.sh` and `check-harness-roster-parity.py` both compare code against artifacts that are not code. A GitHub workflow, operator ruleset data, and setup-doc filenames are among them. No port retires either one.

`check-reviewer-descriptor-parity.sh` is the closest of the eight to a port candidate. It compares one declaration set held twice, in two languages. A port of the config layer deletes one side. It is still not dual LOGIC, and the config layer is not on the sequence below.

## The port protocol

Use this. Do not invent another. The repo proved it on two migrations. The property that makes it good is simple. The safety net survives the deletion instead of dying with it.

1. **Differential parity while both legs exist.** Run both implementations over identical fixtures and assert byte-equality.
2. **Capture goldens from the old leg first.** Do this before touching it, while it is still proven correct. Under `FNO_CAPTURE_GOLDEN=1` the helper runs the old leg, writes the goldens, and asserts new equals old before freezing. Goldens live at `crates/fno-agents/tests/golden/<subject>/<case>.{exit,out,err}`, keyed by a slug of the case label.
3. **Delete the old leg and move its callers.** Both, in one change.
4. **Convert the parity test to a characterization test.** It asserts against the goldens from step 2.

Never delete a leg before proving it unreachable. The ask adapters are live in production. A port is a migration with callers to move, never a cleanup. `verify_evidence` was free only because its counterpart was already gone.

When the other language is unavailable, a test skips. It never fails. `claude_ask_parity.rs` shows the shape.

## The provenance declaration

Every `*_parity.rs` declares its stage and its oracle in its header. `scripts/ci/check-parity-test-provenance.sh` asserts that declaration against the filesystem, in both directions.

```rust
//! parity-stage: differential
//! parity-oracle: fno.agents.harnesses.claude
```

`parity-stage` is exactly one of `differential` or `characterization`. `parity-oracle` is a repo-relative path or a Python dotted module, and nothing else.

The check is two-sided on purpose. A `characterization` file must name an oracle that does NOT exist. The leg was deleted, and the golden stands in for it. A `differential` file must name one that DOES exist. A live second implementation is the only reason to run both legs. Either way the assertion has a positive marker rather than an absence. Either way it fails loudly the moment a port finishes or a new dual implementation appears.

The header carries no node id and no PR number. `scripts/ci/check-no-internal-refs.sh` fails on them. The oracle path is the identity.

## Sequence

The claim classifier goes first. It is the only dual implementation with no parity guard pinning it. A guarded leg cannot drift silently. An unguarded one can.

The ask adapters go second. They already sit at step 1, with both harnesses passing. The work is steps 2 through 4, plus moving one caller.

There is no third. Do not open a `kill_criteria` port under this sequence. The table above removed it from the dual set. Filing one re-imports the error this page corrects.

One deferred item sits in the family without joining the sequence, and its reason generalizes. The tier-remap refusal exists in the Python spawn launcher and not in the Rust spawn binary. That is a guard MISSING from a second implementation. It is not a second implementation to retire. Writing that guard again, in the second language, commits the exact pattern this page removes. When the spawn entry point collapses to one implementation, the gap closes for free. A missing guard on a duplicated entry point argues for finishing the port, never for duplicating the guard.
