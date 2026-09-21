# Node provenance: the parent-session edge, captured ambiently

When a backlog node (idea / follow-up / carveout) is created mid-pipeline, the reason it needed to exist lives in the originating conversation transcript. That transcript does not auto-carry into a fresh thread, and the node's title + details are a lossy paraphrase. A later session that picks the node up starts from a reconstruction, not the ground truth that justified it. The same gap exists for spawned workers: the mesh can trace a worker forward (its node, logs, own transcript) but not backward to the conversation that decided to spawn it.

This feature records that backward edge at the moment of creation, so a node or worker can be resolved back to the session and transcript that produced it.

## The load-bearing principle: capture is ambient, never volunteered

Any provenance design whose correctness depends on the model remembering to pass an arg is already broken. The direct evidence in this repo is `.fno/carveouts.jsonl`. It first failed in the obvious direction, staying empty while the structured `fno backlog carveout add` verb went unused. Adoption fixed that and exposed the same defect underneath. Measured 2026-08-12, 13 of 39 rows carry no `session_id`. A third of the ledger cannot be traced back to the session that filed it. Capture that hinges on a verb being called at the right moment is unreliable. So is a field that hinges on a caller passing it.

So provenance is stamped from the environment at the moment of node birth and worker spawn, inside the originating session. That session is the only place the full context exists; everything later is reconstruction. No caller passes anything.

## The schema

Every graph node carries (all nullable, defaulted on read in `cli/src/fno/graph/store.py`, declared on the `Entry` model in `types.py`):

| Field | Meaning | Stamped at |
|---|---|---|
| `source_session_id` | session that created the node | node birth |
| `source_harness` | harness of that session: `claude` \| `codex` \| `gemini` | node birth |
| `source_cwd` | originating session cwd (transcript-resolver key; distinct from the node's durable `cwd`) | node birth |
| `source_node_id` | the origin node that session was working on | node birth (manifest) |
| `source_plan_path` | plan the origin session was executing, if any | node birth (manifest) |
| `spawned_by_session` | parent session that spawned the worker | worker spawn (registry row and, when the spawn names a node, the node) |
| `spawned_by_harness` | parent harness | worker spawn |
| `spawned_by_cwd` | parent cwd, for the transcript-path slug resolver | worker spawn |
| `lineage_reason` | why no parent session could be proved: the identity disposition the capture read, or the daemon-mint miss. An origin=spawn row carries a session or a reason, never neither | worker spawn (schema v33) |
| `node` | the backlog node the worker is FOR, stamped at the spawn seam - never the spawner's ambient value | worker spawn (registry v21) |
| `node_reason` | why the row works no node when the spawn NAMED one: the seed's verb argument read as a node id but resolved to no readable row. Absent when the node resolved; absent when none was named | worker spawn (registry v36) |

The `agent_spawned` event (`cli/src/fno/events/schema.yaml`) carries the same `spawned_by_*` triple, so the durable event log keeps the parent edge even if a registry row is later rewritten.

## Where capture happens

**Node birth** (`cli/src/fno/graph/cli.py`, `_session_provenance`, merged in `_build_backlog_node`). Reads the running session's env and `.fno/target-state.md`. Centralized in the shared builder, so `add`, `idea`, and `decompose` all self-describe. `source_node_id` / `source_plan_path` resolve only when manifest ownership is proven: the manifest's `claude_transcript_id` must equal `CLAUDE_CODE_SESSION_ID`, mirroring `fno.agents.whoami.find_held_node`, so a stale, reused, or foreign-worktree manifest never leaks a node the session does not hold. Node + plan resolution is claude-only (the only proven transcript-resolver lane); codex/gemini stamp session + harness and degrade the rest.

**Worker spawn** (`cli/src/fno/agents/dispatch.py`, `_capture_parent_edge`, wired into the claude create path). `fno agents spawn` runs as a subprocess of the spawning session, so the parent's `CLAUDE_CODE_SESSION_ID` / `CODEX_SESSION_ID` / `GEMINI_SESSION_ID` and `PWD` are inherited in `os.environ`. The triple is recorded on the new `AgentEntry` and emitted on exactly one `agent_spawned` event after the registry write. `_capture_parent_edge` refuses markers from two harness families rather than ranking them. A machine dispatcher (`ac`, `rd`, `ab`) sets `FNO_SPAWN_TRIGGER=dispatch:<source>` and strips the identity markers from the spawn env. The row then records the dispatcher in `spawn_trigger` and no parent session. The ask event reads `caller_kind` `dispatcher`.

Both helpers trim env values and coerce empty/whitespace to `None`. Neither raises: a missing env or absent manifest degrades every field to null and the create path proceeds unchanged.

**Node launch edge** (`cli/src/fno/agents/cli.py`, `_stamp_launch_edge`, called at both spawn sites beside `_stamp_spawned_session_row`). A registry row is reaped and a node is durable, so a spawn that names a node writes the same triple onto the node. Until this stamp existed the three graph fields were declared and read, but written by nothing. 0 of 2356 nodes carried one, flat zero in every `created_at` cohort.

Do not fill this field from `source_session_id`. They answer different questions. Of 506 nodes carrying both a `source_session_id` and a worked session, the filer is not among the workers in 428.

The stamp refuses rather than half-writes. No node, no write. No proven parent session, no write, because a triple with a null session on a durable node asserts a launch nobody can trace. An existing edge is never overwritten: launch is the FIRST launch, so a second worker on the node does not rewrite who started it.

**Reading a null parent.** `spawned_by_session` is null for more than one reason, and the harness half plus `spawn_trigger` plus `lineage_reason` say which:

| session | harness | spawn_trigger | reason | means |
|---|---|---|---|---|
| set | set | - | - | full lineage |
| null | set | - | - | a harness process spawned it; its session id could not be proved |
| null | null | - | - | no harness ancestor: a human shell or a daemon |
| null | set | `dispatch:<source>` | - | a dispatcher chose this spawn; the session that ran it did not ask |
| null | - | - | `daemon mint: spawn request carried no parent edge` | a daemon minted the row and the spawn request carried no parent edge |
| null | - | - | `identity disposition=..., markers=...` | the ambient capture resolved no parent; the disposition names what it read |

A daemon mint reads the parent edge from the spawn REQUEST, never from the daemon's own environment. The daemon lazy-start scrubs the harness session markers. An ambient read there stamped None by construction: every row it minted lost who spawned it. The Rust spawn client stamps its own ambient `spawned_by_*` triple onto the request. The mint (`Lineage::from_request`) reads the request the way the `node` field is already trusted. A request with no parent edge stamps `lineage_reason` instead of a silent null. `fno agents registry-json` now projects `spawned_by_harness` and `lineage_reason` beside `spawned_by_session`, so this table applies through that reader too.

When the spawning process carries NO identity marker at all, `_capture_parent_edge` takes the harness from the process-tree walk and leaves the session id null. The walk is the prover, and a harness ancestor cannot be a stranger the way an inherited marker can. The fallback is gated on an empty marker set, not on a missing harness. A marker that IS present and resolved to nothing is a contradiction, and a contradiction attributes nothing. The `agent_spawned` event carries the `lineage_reason` in every case.


## Reading it back: the resolver

`cli/src/fno/provenance/resolver.py` turns a stored pointer into a transcript path:

```
resolve_transcript(harness, session_id, cwd) -> ResolvedTranscript
```

For `claude` it resolves `~/.claude/projects/<slug(cwd)>/<session_id>.jsonl`, where `slug(cwd)` replaces both `/` and `.` with `-` (e.g. `/Users/bb16/code/me/fno` -> `-Users-bb16-code-me-fno`). It tries an exact `<session_id>.jsonl` first, then globs `<session_id>*.jsonl` because the id may be an 8-hex prefix; multiple matches return the first deterministically with `ambiguous=True`. A foreign harness (`codex`, `gemini`, anything else) returns `resolved=False` with `reason="harness-not-supported"` rather than guessing. Missing inputs and unexpected OS errors also return `resolved=False`; the function never raises.

The separation is deliberate: capture the pointer universally and harness-agnostically now (cheap, future-proof against the next CLI swap), resolve it lazily and per-harness only for harnesses actually read back. The codex resolver is deferred until codex session capture is fixed upstream; gemini and antigravity have no transcript store to resolve.

## The read command

```
fno backlog provenance <node-id>          # human summary
fno backlog provenance <node-id> --json   # structured: node_id, title, edges[]
```

Read-only. For each edge a node carries (node-birth and/or spawn), it runs the resolver and reports the resolved transcript path or the reason it could not resolve. The node-birth edge resolves against `source_cwd` (the originating session's cwd, which claude transcript dirs are slugged by), falling back to the node's durable `cwd` only for legacy pre-`source_cwd` nodes; the spawn edge resolves against `spawned_by_cwd`. Using the durable project `cwd` would point at the wrong `~/.claude/projects/<slug>` whenever a node was filed from a worktree, which is the common mid-pipeline case.

## The spawn door: required origin, separate owner (schema v33)

The one spawn door lives in `crates/fno-agents/src/spawn_contract.rs` and `spawn_transaction.rs`. It replaced ambient capture as the birth contract. Every new worker birth carries a validated structured record, and the ambient triple becomes a generated compatibility projection of it.

| Field | Meaning |
|---|---|
| `spawn_id` | coordinator-allocated attempt id (`sp-<hex>`) correlating journal accepted record, birth row, and receipt |
| `spawn_provenance.origin` | `session` (proven parent: harness, full session id, caller cwd, plus an optional invocation reference for a script the session ran) or `non_session` (typed source: daemon arm + cause, LaunchAgent label, shell with TTY and process identity, or test script + run id) |
| `spawn_provenance.owner` | session, project-qualified mission, project-qualified crown scope, operator/TTY, or test run. Separate from origin by design. Daemon work names its mission or crown without inventing a parent. A kingless scope is still a valid owner |
| `spawned_by_*` | generated compatibility projection of a session origin, never independently writable on a new birth |

Rules the door enforces before any launch:

- a daemon or launch-agent origin requires a mission or crown owner. The daemon starter is never substituted.
- a cause code must speak the naming-codes vocabulary. The retired `sob` refuses.
- a known-shape session id attributed to the wrong harness refuses (the forgery shape).
- a session-launched test keeps its live session parent. The script is recorded as the invocation, never as a parent replacement.
- rows predating the door read as `legacy_missing` provenance. That is a visible defect, never silently blessed, and never repaired from a name prefix, crown holder, or adopter.

Producers carry context through the `FNO_SPAWN_ORIGIN` / `FNO_SPAWN_OWNER` env carrier (validated, malformed refuses) or the `agent.spawn` request's `origin`/`owner` fields. Explicit dispatch context outranks ambient capture. A session spawning by hand keeps its real session parent. Adoption is a voucher, not a birth: the adopter lands in `adopted_by_session`, and a re-adopt preserves every birth field. The crown hook reads ownership. A door-stamped row is owned autonomous work, never an unlinked orphan.

Read-only runtime verification: `scripts/diagnostics/verify-spawn-contract.py --plan <plan.md>` checks the correlated real-path proof (spawn ids, journal correlation, sha pinning, freshness) and refuses anything self-asserted.

## Scope and sequencing

This is the field + ambient stamp + claude resolver. Out of scope and tracked separately: the presence-aware `/think` spawn mechanism that consumes these pointers is a separate effort (blocked by this node); the codex and antigravity resolver lanes are deferred. Historical backfill of provenance for pre-existing nodes is best-effort and out of scope here: `source_session_id` was never captured for old nodes and is unrecoverable; forward-stamping is the cheap, high-leverage path and makes every future creation self-describing.
