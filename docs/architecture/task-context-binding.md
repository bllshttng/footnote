# Task-context binding

A task-context binding is the record that joins one executing attempt to the
context it was required to read. It exists so that nothing else has to be
trusted: not the dispatcher's memory, not a compaction boundary, and not a
worker's claim that it read the plan.

## What a binding contains

One binding per attempt, stored at `.fno/artifacts/handoff/task-context-<node>.json` (one slot per node; each binding is immutable through its `binding_digest`, and resume receipts embed their own copy):

| Field | Meaning |
|---|---|
| `version` | 1 |
| `node`, `attempt` | The node and the run id of the session that prepared it |
| `harness`, `session` | The preparing session's full harness/session identity |
| `worktree` | The worktree the sources live in |
| `plan_path`, `plan_digest` | The plan reference and its sha256 |
| `bundle_reference`, `bundle_digest` | The selected ContextBundle, named not re-selected |
| `required_constraints` | Constraints declared required, carried verbatim |
| `required_sources` | Sources that must revalidate: path, revision, sha256 digest, byte size |
| `source_bytes` / `payload_bytes` | The sources' bytes vs the prepared payload's bytes; always two separate measures |
| `stage` | The honest observation stage (see below) |
| `binding_digest` | sha256 over the canonical serialization |

## The evidence stages

`stage` answers "how far did observation actually get", and each value carries
a hard ceiling on what may be claimed. Nothing upstream may claim more than
the stage does.

| Stage | Proves | Does NOT prove |
|---|---||
| `prepared` | A binding was minted and digested by the native verifier | Nothing was sent, carried, or read |
| `submitted` | A payload carrying the pointer was handed to a harness for delivery | Delivery happened, or the recipient read anything |
| `observed` | x-175a accepted-turn evidence exists for the recipient | The required sources were read; the pointer being injected is not a read |
| `unavailable` | Observation capability is unknown or unsupported | Anything; it is an honesty label, and it can never advance to `observed` |

## The doors

- **Init gate** (`target_context_gate.gate_declared_task_context`, called from `fno do target init` before the init script): a DECLARED binding (env `FNO_TASK_CONTEXT_FILE`) revalidates before the node claim is acquired. Refusals are named (`context_stale_source`, `context_missing_source`, `context_wrong_node`, `context_binding_unreadable`, `context_native_verifier_unavailable`) and write no state, so an existing owner is preserved.
- **Handoff** (`skills/target/scripts/handoff.sh`, receipt-write step): the binding revalidates and embeds into the immutable resume receipt while the parent still holds authority; a refusal parks the handoff (`reason="context_revalidation: ..."`) before any delegation commits. The child's claim/manifest proof (the existing second proof) still gates the delegation itself.
- **Receipt validate** (`fno do resume receipt validate --attempt <own>`): a receipt-carried binding revalidates against live sources, with wrong-attempt/foreign-session checks at this door.
- **Compaction** (`hooks/target-postcompact-reinject.sh`): re-emits goal, node, the plan reference for BOTH shapes (the `-d` gate used to drop single-FILE plans), and the binding pointer with its declared constraints, once, without replacing the session. A pointer in context is not a read.
- **Spawn payloads** (`fno.agents.spawn_payload.prepare_spawn_payload`): both launch substrates (dispatch non-pane, mux_spawn pane) go through ONE entry; the block (identity, digest, stage, at most 10 declared constraints; never source contents) rides once. The spawner arms it with `FNO_TASK_CONTEXT_FILE`.

## Revalidation semantics

Content decides staleness: revalidation re-reads each required source under
the worktree and compares sha256, so a code HEAD that moved without touching
a required source never stales the binding (a cosmetic HEAD change is not a
task revision). An authorized task revision re-prepares: the new binding gets
a new digest and replaces the file slot; receipts keep the old digest as
history. Changed constraints never auto-override; a required source that
changed must revalidate through a new binding.

Identity expectations are per-door: a door checks the identities it knows.
The init gate knows node + root; the receipt door passes attempt + session;
the journey fixtures pass all three. An absent expectation key is unchecked
at that door, never a silent pass elsewhere.

## Inspect a binding

```bash
cat .fno/artifacts/handoff/task-context-<node>.json
fno do resume receipt show --node <node>        # task_context_bound: true/false
fno do resume receipt validate --node <node> --attempt <own attempt>
```

## Tests

```bash
cargo test --manifest-path crates/fno-agents/Cargo.toml task_context
cargo test --manifest-path crates/fno-agents/Cargo.toml --test task_context_journey
bash tests/hooks/test_target_context_binding.sh
uv run --project cli pytest -q cli/tests/unit/test_task_context_binding.py
```

The journey test closes with `TASK_CONTEXT_CONTINUITY_PASS attempt=... constraint=...` naming the attempt and the retained constraint.
