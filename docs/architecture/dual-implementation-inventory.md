# Dual-implementation inventory and the port protocol

The operator ruling. When any trigger fires, port a Python behavior to Rust.

1. DUPLICATION. One behavior is hand-written twice, in two languages. This is the original rule, and the dual-logic kind below.
2. PREVENTION. The port removes a class of defect that a type system makes unrepresentable rather than merely detectable. The qualifying classes are in the second-trigger section below.
3. PARITY GUARD. Writing a guard whose purpose is to force Python and Rust implementations to agree is itself the strongest trigger for a port. The guard is legitimate as scaffolding during the port, never as its end state.

The first two triggers do not reach a shared-vocabulary or a generated-artifact guard. Those stay excluded regardless of benefit, because porting them retires nothing and a generated-artifact tripwire is correct as built. The parity-guard trigger applies only when the guard pins two implementations of one decision, not when it checks shared vocabulary or generated data.

This page answers four questions. Which cross-tree guards mark a real second implementation. What the procedure is for retiring one. Why most of these guards are not port candidates at all. When a port is justified without duplication.

## The discriminator

A parity guard exists because two things can drift. Two implementations do not follow from that.

There are three kinds. Only the first is a port candidate.

**dual-logic.** Two hand-written implementations of one behavior, in two languages. A port deletes one leg. The guard then converts to a characterization test. This is the only kind the operator ruling is about.

**shared-vocabulary.** One concept declared, spelled, or stamped at many sites. The guard checks that the sites agree. Porting one site does not retire the guard. The concept still has to be spelled the same way everywhere it appears. Several sites are not code at all. A workflow YAML, a ruleset data file, and a setup-doc filename are all such sites. When a guard's surfaces include a non-code artifact, no port can ever retire it.

**generated-artifact.** One owner, a generated copy, and a freshness tripwire. This is correct and must not be "fixed". The exemplar is `harness_capabilities.toml`. The Rust tree owns the canonical file. `build.rs` generates the Python copy and the mux-crate copy on every build. `scripts/ci/check-harness-capabilities-fresh.sh` catches a stale copy. One owner, one generated artifact, one guard. A generated data file is not a second implementation.

This page exists to prevent one failure: reading a filename as evidence. A file called `*_parity.rs` looks like a live second implementation. Often it is not. The inventory below was filed wrong twice for that reason. The guard in the last section makes the distinction machine-readable.

## The second trigger: prevention

Duplication is not the only reason a port pays. The second trigger is prevention: the port removes a class of defect that a type system makes unrepresentable rather than merely detectable. Detection-only fixes leave the wrong answer still writable. A type that cannot express the wrong answer ends the class. Every case below is one measured incident of the same failure class: an instrument returning a value indistinguishable from a real answer.

The provenance case. Two docstrings stated opposite meanings for one registry lookup: one said a hit proves the id is NOT ours, the other that it IS ours. Both were confident and both were load-bearing. They reconciled only on the provenance of the input, which nothing encoded. Three workers were refused their own node before anyone read both docstrings.

The exhaustiveness case. A capability table recorded agy and opencode as unsupported with no justification, while pi's row carried a measured reason. One word was doing two jobs, measured-absent and never-checked, and no reader can tell them apart.

The rename-safety case. The substrate vocabulary moved from `bg` to `thread`, and a literal comparison against `bg` survived the rename. Every autonomous dispatch then reported it went headless while it spawned a thread, and emitted a false fallback event with it.

Each case earns the port through the type system. Provenance moves into a type instead of a docstring. A closed vocabulary forces every entry to carry its reason. An enum turns a rename into a compile error at every surviving site instead of a false comparison at one.

The scope guard. Prevention is a trigger for porting, not a third kind of implementation. The parity-guard trigger is stronger still: once a guard is needed to force two decision legs to agree, the port has already earned priority. The discriminator above still decides what a dual implementation is, and its exclusions survive these triggers unchanged. Every port follows the same four-step protocol. The sequence section orders confirmed duals by dependency, while a prevention-only case is scheduled on its own measured evidence.

## The parity tests

Measured by reading each file's header, then testing whether its oracle still exists on disk.

| Parity file | Oracle | On disk | Verdict |
|---|---|---|---|
| `crates/fno-agents/tests/claude_ask_parity.rs` | `fno.agents.harnesses.claude._build_envelope` | absent (symbol) | characterization, port complete |
| `crates/fno-agents/tests/codex_ask_parity.rs` | `fno.agents.harnesses.codex.resume` | absent (symbol) | characterization, port complete |
| `crates/fno-agents/tests/graph_store_parity.rs` | `fno.graph.store._acquire_flock` | absent (symbol) | characterization, port complete |
| `crates/fno-agents/tests/kill_criteria_parity.rs` | `scripts/lib/kill-criteria.sh` | absent | characterization, port complete |
| `crates/fno-agents/tests/verify_evidence_parity.rs` | `scripts/lib/verify-event-evidence.sh` | absent | characterization, port complete |
| `crates/fno-agents/tests/claim_classifier_parity.rs` | `fno.claims.staleness` | absent (module) | characterization, port complete |

All six are finished ports. Their oracle legs are gone. Each converted case now asserts against a golden captured before that deletion, and each file sits at step 4 of the protocol below. None awaits anything. The ask files and the graph store file carry the SYMBOL oracles, and their Python modules survive the port. The claim classifier's module oracle is absent because its Python module was deleted. The spawn surface keeps `parse_short_id`, the session registry keeps the reply readers, and `codex.create` keeps the one-shot lane. Only a symbol or module resolution says which leg is gone.

`kill_criteria` is not a dual implementation. State the reason, because the mistake is easy to repeat. The Python files under `cli/src/fno/plan/` validate the kill-criteria DECLARATION against a schema. The Rust `kill-check` verb EVALUATES the predicates at a wave boundary. Two functions share a vocabulary. The parity test never pointed at that Python. It points at a bash oracle that is gone.

## The real dual set

Four live duals below, plus one retirement recorded where it landed (the watchdog reap and retire lanes, 2026-09-04). The ask adapters, the claim classifier and the graph store are retired (2026-09-02), and the review-freshness mirror followed (2026-09-03). The native graph reader remains the confirmed dual closest to a port, with three port-owed duals named below.

**The review-freshness mirror.** `_pr_code_diff_identity` and `_reviewed_sha_still_describes_head` in `cli/src/fno/pr/_reviews.py` mirrored `review_freshness` in loopcheck.rs decision for decision. When the Rust instrument was absent, they were the fallback answer. Retired 2026-09-03. The fallback guessed where every other missing-instrument gate refuses. It also disagreed with the gate it mirrored: a locally recomputed floor read 3 where the gate's row said 1 on one PR. The one predicate now lives in `crates/fno-agents/src/review_freshness.rs`. With the binary absent, `fno do pr status` answers `unmeasurable` and names the remedy.

**The claim classifier.** Port complete (2026-09-02). The Python liveness module was deleted, every verdict consumer moved to the Rust decision door, and `claim_classifier_parity.rs` now characterizes the captured state+basis golden. Its parity guard is the third trigger's specimen: writing the guard proved that the duplicated decision had to be retired.

**The ask adapters.** `fno.agents.harnesses.claude` and `fno.agents.harnesses.codex`, pinned by the two differential harnesses until their port completed (2026-09-02). The Rust runtime already owned production `ask` end to end. The port deleted the Python ask functions and moved the last Python-only caller duty, `--to-project` anycast resolution, into `cmd_ask` ahead of the binary exec. The surviving Python surface is spawn substrate, not ask: `parse_short_id` for bg receipts, the registry reply readers, and `codex.create` for one-shot spawns.

**The graph store.** `cli/src/fno/graph/store.py`, pinned by the differential harness until its port completed (2026-09-02). The store core lives in `crates/fno-agents/src/graph_store.rs` behind a keeper process. The core owns the byte-compatible JSON I/O, defaults, status recompute, canonicalization, slugs, the bounded lock, and the publish with its backup and sidecar. The keeper model is one process per graph, with the socket beside the file. The Python file survives as the socket client with unchanged signatures. The verb surface above it never moved. The port also removed two defect classes. Typed presence makes an empty overwrite of `details` unrepresentable at the store boundary. The bounded lock answers inside its deadline and never blocks forever.

**The native graph reader.** `crates/fno/src/backlog_view.rs`, 2140 lines of Rust parsing `graph.json` with no subprocess. Its docstrings name the Python oracles it mirrors. Two mirrors still hold on both sides: `KANBAN_COLUMNS` (`cli/src/fno/graph/render.py:26`, mirrored at `backlog_view.rs:380`) and `_kanban_column` (`render.py:71`, mirrored near `backlog_view.rs:390`). The readiness mirror lost its Python counterpart. The store port moved the overlay into `graph_store.rs`, and the Python statuses module now answers through the keeper. This file is not a duplicate that waits for deletion. It is the read-only reader of the mux, and the store client borrows its `kanban_column` for peer lanes. A port here deletes a reader, not a second implementation of the store. Dual and UNGUARDED. The parity is a comment asking a human to remember, and the tests beside it run one side only. The earlier guard enumeration did not find this file. That is the limit the duplicate-discovery sweep exists to close. A mirror that declares itself in a docstring is invisible to a guard census keyed on parity-test files. It holds exactly one seam crossing, so a crossing count alone scores writing more of it as an improvement. The two-axis budget in [the seam doc](rust-python-seam.md) exists for exactly this shape.

**The watchdog reap and retire lanes.** Dual, disposition dual-logic, RETIRED (2026-09-04). The Python lanes (`reap_decision`, `retire_decision` and their apply halves in `cli/src/fno/agents/watchdog.py`) and the Rust sweep answered the same question, which row may be removed, with different keys: the Python lane read a node's `status` plus claim occupancy; the Rust sweep read exit stamps. Both were wrong keys. The retirement question now has one leg, the daemon sweep in `crates/fno-agents/src/gc.rs` + `gc_sweep.rs`, keyed by the reverse join through `node.sessions[]` plus transcript quiet. The characterization is the AC7 test family in `crates/fno-agents/src/daemon/tests/gc_receipts.rs`. The watchdog keeps wake, reroute, ghost and report.

**`classify_progress` vs the daemon's list projection.** Dual, disposition dual-logic, port owed, not in this PR. Python leg `classify_progress` in `cli/src/fno/agents/reachability.py:217`, Rust leg the progress fold in `crates/fno-agents/src/daemon.rs` (the list projection, `rendered_status_from_truth` around line 5954). Both classify a session's tail into the progress axis the roster renders, and both are correct today. A port deletes the Python leg when the roster's progress axis answers through the daemon.

**`pending_supersession_reason`.** Dual, disposition dual-logic, port owed, not in this PR. Python leg `cli/src/fno/graph/statuses.py:220`, Rust leg `crates/fno-agents/src/graph_store.rs:770`. Both answer whether a supersession lacks merged-PR proof, and both are correct today. The blocked_by edge settlement landed beside them and recorded this row. A port deletes the Python leg and converts the guard to characterization. The readiness predicate it sits beside is already Rust-only.

**`node_is_open`.** Dual, disposition dual-logic, port owed, not in this PR. Python leg `cli/src/fno/graph/_reconcile.py` (`node_is_open`, the selection fan-out's open count), Rust leg `crates/fno-agents/src/graph_store.rs` (`is_open_entry`, the settlement's row filter). Both key off the underlying fields, never the derived status, so both hold on rows that never saw a recompute, and both are correct today. A port deletes the Python leg once the fan-out answers through the store.

Nothing else in the tree is a confirmed dual implementation today.

## The eight CI parity scripts

All eight are shared-vocabulary. None is a port candidate. The reasons differ enough to state one at a time.

| Script | Compares | Verdict |
|---|---|---|
| `check-coverage-context-parity.sh` | two commit-status context strings and one label, across the Python publisher, the operator ruleset data, the Rust publisher, and the refresher workflow | shared-vocabulary |
| `check-harness-roster-parity.py` | `KNOWN_HARNESSES` against three evidence surfaces: `docs/SETUP-<name>.md` filenames, the `for_name()` match arms in `provider.rs`, and the `_register()` calls in the Python adapter registry | shared-vocabulary |
| `check-provider-vocabulary-parity.sh` | provider vocabulary across four Rust files and two Python files | shared-vocabulary |
| `check-review-app-parity.sh` | review-App logins across `BOT_PROFILES` in Rust and two Python declarations, plus a `usage_markers` field per profile | shared-vocabulary |
| `check-reviewer-descriptor-parity.sh` | `_RESOLVABLE_REVIEWERS` against `REVIEWER_INVOCATIONS`: invocation string, self-cert flag, per-harness verb overrides | shared-vocabulary |
| `check-session-identity-parity.sh` | the identity-writing surfaces in two Rust files and the Python registry | shared-vocabulary |
| `check-spawn-lineage-parity.sh` | that every registry mint site stamps the `spawned_by_*` parent edge, across eight Rust files and four Python files | shared-vocabulary |

Two of these need a note. A reader in a hurry can mistake them for port candidates.

`check-spawn-lineage-parity.sh` is an invariant gate, not a parity gate, despite its name. It asserts that twelve separate mint sites each stamp a field. Collapse the four Python sites into Rust and eight sites remain, under the same gate. The thing that drifts is a site forgetting the stamp, not two languages disagreeing.

`check-coverage-context-parity.sh` and `check-harness-roster-parity.py` both compare code against artifacts that are not code. A GitHub workflow, operator ruleset data, and setup-doc filenames are among them. No port retires either one.

`check-reviewer-descriptor-parity.sh` is the closest of the eight to a port candidate. It compares one declaration set held twice, in two languages. A port of the config layer deletes one side. It is still not dual LOGIC, and the config layer is not on the sequence below.

## The port protocol

Use this. Do not invent another. The repo proved it on two migrations. The property that makes it good is simple. The safety net survives the deletion instead of dying with it.

0. **Measure both budget axes.** Count the seam crossings and the dual set before step 1. A port that raises either axis is refused, however clean its diff. The reason and the gaming path are in [the seam doc](rust-python-seam.md). This page's hand list is the second axis until the duplicate-discovery sweep lands.
1. **Differential parity while both legs exist.** Run both implementations over identical fixtures and assert byte-equality.
2. **Capture goldens from the old leg first.** Do this before touching it, while it is still proven correct. Under `FNO_CAPTURE_GOLDEN=1` the helper runs the old leg, writes the goldens, and asserts new equals old before freezing. Goldens live at `crates/fno-agents/tests/golden/<subject>/<case>.{exit,out,err}`, keyed by a slug of the case label.
3. **Delete the old leg and move its callers.** Both, in one change.
4. **Convert the parity test to a characterization test.** It asserts against the goldens from step 2.

Never delete a leg before proving it unreachable. The ask adapters are live in production. A port is a migration with callers to move, never a cleanup. `verify_evidence` was free only because its counterpart was already gone.

When the other language is unavailable, a test skips. It never fails. The live-differential residue in `claude_ask_parity.rs` and `codex_ask_parity.rs` shows the shape. It covers the spawn substrate whose Python counterparts survive. The golden-driven cases need no other language at all.

## The provenance declaration

Every `*_parity.rs` declares its stage and its oracle in its header. `scripts/ci/check-parity-test-provenance.sh` asserts that declaration against the filesystem, in both directions.

```rust
//! parity-stage: differential
//! parity-oracle: fno.agents.harnesses.claude
```

`parity-stage` is exactly one of `differential` or `characterization`. `parity-oracle` names the LEG, in one of three forms.

- A repo-relative path, for a leg that occupied a whole file.
- A dotted module, for a leg that occupies a whole Python module.
- A dotted module with one final symbol, for a leg inside a surviving module. The symbol names a `def`, a `class`, or an assignment at that module's top level.

The symbol form is the ordinary Python case. The module file survives the port, the ask functions inside it do not, and only a symbol can say which leg is gone. The two finished bash ports made "the file is gone" and "the leg is gone" read as one sentence. They shared that property by accident, and no Python oracle shares it at all.

The check is two-sided on purpose. A `characterization` file must name an oracle that NO LONGER RESOLVES. The leg was deleted, and the golden stands in for it. A `differential` file must name one that DOES resolve. A live second implementation is the only reason to run both legs. Either way the assertion has a positive marker rather than an absence. Either way it fails loudly the moment a port finishes or a new dual implementation appears.

The header carries no node id and no PR number. `scripts/ci/check-no-internal-refs.sh` fails on them. The oracle is the identity. A path names a whole-file leg, a symbol names a leg inside a surviving module, and resolution is what both arms of the check test.

## Sequence

The claim classifier went first and is complete. It was the only dual implementation with no parity guard when this sequence was filed; its golden characterization now proves the Rust decision is the sole surviving leg.

Retirement order is subordinate to the crossing dependency order in [the seam doc](rust-python-seam.md). A leg with no guard still waits for the legs it feeds. Risk argues for a port. Dependency decides the safe order.

The ask adapters went second, and their retirement is complete. They sat at step 1. The work was steps 2 through 4, plus moving one caller and fixing the guard the first symbol-oracle port exposed.

The graph store went third (2026-09-02). It followed the same four steps. The goldens came from the Python leg. One change deleted the Python store core and moved its callers onto the keeper client. The parity file is now characterization. Two of its cases are Rust-only: the corrupt-kind taxonomy and the concurrent-writer case. The Python leg never held those behaviors, so no golden exists for them.

There is no third. Do not open a `kill_criteria` port under this sequence. The table above removed it from the dual set. Filing one re-imports the error this page corrects.

One deferred item sits in the family without joining the sequence, and its reason generalizes. The tier-remap refusal exists in the Python spawn launcher and not in the Rust spawn binary. That is a guard MISSING from a second implementation. It is not a second implementation to retire. Writing that guard again, in the second language, commits the exact pattern this page removes. When the spawn entry point collapses to one implementation, the gap closes for free. A missing guard on a duplicated entry point argues for finishing the port, never for duplicating the guard.
