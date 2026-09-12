# Task-context binding

A task-context binding joins one executing attempt to the context it was required to read. Nothing else needs to be trusted: not the dispatcher's memory, not a compaction boundary, and not a worker's claim that it read the plan.

## What a binding contains

One binding per attempt. The file slot is `.fno/artifacts/handoff/task-context-<node>.json`. Each binding is immutable through its `binding_digest`. Resume receipts embed their own copy.

| Field | Meaning |
|---|---|
| `version` | 1 |
| `node`, `attempt` | The node and the run id of the preparing session |
| `harness`, `session` | The preparing session's full harness/session identity |
| `worktree` | The worktree the sources live in |
| `plan_path`, `plan_digest` | The plan reference and its sha256 |
| `bundle_reference`, `bundle_digest` | The selected ContextBundle, named not re-selected |
| `required_constraints` | Constraints declared required, carried verbatim |
| `required_sources` | Sources that must revalidate: path, revision, sha256 digest, byte size |
| `source_bytes` / `payload_bytes` | Two separate measures: the sources' bytes and the prepared payload's bytes |
| `stage` | The honest observation stage (see below) |
| `binding_digest` | sha256 over the canonical serialization |

## The evidence stages

`stage` answers how far observation actually got. Each value carries a hard ceiling on what can be claimed. Nothing upstream can claim more.

| Stage | Proves | Does NOT prove |
|---|---|---|
| `prepared` | A binding was minted and digested by the native verifier | Nothing was sent, carried, or read |
| `submitted` | A payload with the pointer was handed to a harness | Delivery happened, or the recipient read anything |
| `observed` | x-175a accepted-turn evidence exists for the recipient | That the required sources were read. An injected pointer is not a read |
| `unavailable` | Observation capability is unknown or unsupported | Anything. It is an honesty label. It can never advance to `observed` |

## The doors

- **Init gate** (`target_context_gate.gate_declared_task_context`, from `fno do target init`): a DECLARED binding (env `FNO_TASK_CONTEXT_FILE`) revalidates before the node claim is acquired. Refusals are named: `context_stale_source`, `context_missing_source`, `context_wrong_node`, `context_binding_unreadable`, `context_native_verifier_unavailable`. A refusal writes no state. An existing owner stays intact.
- **Handoff** (`skills/target/scripts/handoff.sh`, receipt-write step): the binding revalidates while the parent still holds authority. It then embeds into the immutable resume receipt. A refusal parks the handoff (`reason="context_revalidation: ..."`) before any delegation commits. The child's claim/manifest proof still gates the delegation itself.
- **Receipt validate** (`fno do resume receipt validate --attempt <own>`): a receipt-carried binding revalidates against live sources. This door also checks wrong-attempt and foreign-session.
- **Compaction** (`hooks/target-postcompact-reinject.sh`): re-emits goal, node, the plan reference for both shapes, and the binding pointer with its declared constraints. The old `-d` gate used to drop single-FILE plans. The session is not replaced. A pointer in context is not a read.
- **Spawn payloads** (`fno.agents.spawn_payload.prepare_spawn_payload`): both launch substrates go through ONE entry. The block carries identity, digest, stage, and at most 10 declared constraints. Source contents never ride. The spawner arms it with `FNO_TASK_CONTEXT_FILE`.

## Revalidation semantics

Content decides staleness. Revalidation re-reads each required source under the worktree and compares sha256. A code HEAD that moved without touching a required source never stales the binding. A cosmetic HEAD change is not a task revision. An authorized task revision re-prepares: the new binding gets a new digest and replaces the file slot. Receipts keep the old digest as history. Changed constraints never auto-override. A required source that changed must revalidate through a new binding.

Identity expectations are per-door. A door checks the identities it knows. The init gate knows node + root. The receipt door passes attempt + session. The journey fixtures pass all three. An absent expectation key is unchecked at that door. It is never a silent pass elsewhere.

## Inspect a binding

```bash
cat .fno/artifacts/handoff/task-context-<node>.json
fno do resume receipt show --node <node>
fno do resume receipt validate --node <node> --attempt <own attempt>
```

## Tests

```bash
cargo test --manifest-path crates/fno-agents/Cargo.toml task_context
cargo test --manifest-path crates/fno-agents/Cargo.toml --test task_context_journey
bash tests/hooks/test_target_context_binding.sh
uv run --project cli pytest -q cli/tests/unit/test_task_context_binding.py
```

The journey test closes with `TASK_CONTEXT_CONTINUITY_PASS`. The marker names the attempt and the retained constraint.
