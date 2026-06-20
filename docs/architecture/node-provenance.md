# Node provenance: the parent-session edge, captured ambiently

When a backlog node (idea / follow-up / carveout) is created mid-pipeline, the reason it needed to exist lives in the originating conversation transcript. That transcript does not auto-carry into a fresh thread, and the node's title + details are a lossy paraphrase. A later session that picks the node up starts from a reconstruction, not the ground truth that justified it. The same gap exists for spawned workers: the mesh can trace a worker forward (its node, logs, own transcript) but not backward to the conversation that decided to spawn it.

This feature (backlog node `x-30f6`) records that backward edge at the moment of creation, so a node or worker can be resolved back to the session and transcript that produced it.

## The load-bearing principle: capture is ambient, never volunteered

Any provenance design whose correctness depends on the model remembering to pass an arg is already broken. The direct evidence in this repo is `.fno/carveouts.jsonl`: it stayed empty because the structured `fno carveout add` verb went unused, and carveouts only ever landed as prose. Capture that hinges on a verb being called at the right moment does not happen reliably.

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
| `spawned_by_session` | parent session that spawned the worker | worker spawn |
| `spawned_by_harness` | parent harness | worker spawn |
| `spawned_by_cwd` | parent cwd, for the transcript-path slug resolver | worker spawn |

The `agent_spawned` event (`docs/architecture/events-schema.yaml`) carries the same `spawned_by_*` triple, so the durable event log keeps the parent edge even if a registry row is later rewritten.

## Where capture happens

**Node birth** (`cli/src/fno/graph/cli.py`, `_session_provenance`, merged in `_build_backlog_node`). Reads the running session's env and `.fno/target-state.md`. Centralized in the shared builder, so `add`, `idea`, and `decompose` all self-describe. `source_node_id` / `source_plan_path` resolve only when manifest ownership is proven: the manifest's `claude_transcript_id` must equal `CLAUDE_CODE_SESSION_ID`, mirroring `fno.agents.whoami.find_held_node`, so a stale, reused, or foreign-worktree manifest never leaks a node the session does not hold. Node + plan resolution is claude-only (the only proven transcript-resolver lane); codex/gemini stamp session + harness and degrade the rest.

**Worker spawn** (`cli/src/fno/agents/dispatch.py`, `_capture_parent_edge`, wired into the claude create path). `fno agents spawn` runs as a subprocess of the spawning session, so the parent's `CLAUDE_CODE_SESSION_ID` / `CODEX_SESSION_ID` / `GEMINI_SESSION_ID` and `PWD` are inherited in `os.environ`. The triple is recorded on the new `AgentEntry` and emitted on exactly one `agent_spawned` event after the registry write. Harness precedence is claude > codex > gemini.

Both helpers trim env values and coerce empty/whitespace to `None`. Neither raises: a missing env or absent manifest degrades every field to null and the create path proceeds unchanged.

## Reading it back: the resolver

`cli/src/fno/provenance/resolver.py` turns a stored pointer into a transcript path:

```
resolve_transcript(harness, session_id, cwd) -> ResolvedTranscript
```

For `claude` it resolves `~/.claude/projects/<slug(cwd)>/<session_id>.jsonl`, where `slug(cwd)` replaces both `/` and `.` with `-` (e.g. `/Users/bb16/code/me/abilities` -> `-Users-bb16-code-me-abilities`). It tries an exact `<session_id>.jsonl` first, then globs `<session_id>*.jsonl` because the id may be an 8-hex prefix; multiple matches return the first deterministically with `ambiguous=True`. A foreign harness (`codex`, `gemini`, anything else) returns `resolved=False` with `reason="harness-not-supported"` rather than guessing. Missing inputs and unexpected OS errors also return `resolved=False`; the function never raises.

The separation is deliberate: capture the pointer universally and harness-agnostically now (cheap, future-proof against the next CLI swap), resolve it lazily and per-harness only for harnesses actually read back. The codex resolver is deferred until codex session capture is fixed upstream; gemini and antigravity have no transcript store to resolve.

## The read command

```
fno backlog provenance <node-id>          # human summary
fno backlog provenance <node-id> --json   # structured: node_id, title, edges[]
```

Read-only. For each edge a node carries (node-birth and/or spawn), it runs the resolver and reports the resolved transcript path or the reason it could not resolve. The node-birth edge resolves against `source_cwd` (the originating session's cwd, which claude transcript dirs are slugged by), falling back to the node's durable `cwd` only for legacy pre-`source_cwd` nodes; the spawn edge resolves against `spawned_by_cwd`. Using the durable project `cwd` would point at the wrong `~/.claude/projects/<slug>` whenever a node was filed from a worktree, which is the common mid-pipeline case.

## Scope and sequencing

This is the field + ambient stamp + claude resolver. Out of scope and tracked separately: the presence-aware `/think` spawn mechanism that consumes these pointers is `x-6a10` (blocked by this node); the codex and antigravity resolver lanes are deferred. Historical backfill of provenance for pre-existing nodes is best-effort and out of scope here: `source_session_id` was never captured for old nodes and is unrecoverable; forward-stamping is the cheap, high-leverage path and makes every future creation self-describing.
