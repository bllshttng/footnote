# Session liveness

footnote answers two different questions with two different truth families.

Family 1 is `fno agents truth`, backed by `cli/src/fno/agents/session_truth.py`.
It reads the resolved transcript tail and content-aware activity age to answer whether a session is alive or working.

Family 2 is `truth_status` plus the target orienter's `manifest-live` check.
It reads claim liveness and loop-check recency to answer whether a session owns work.

Neither family substitutes for the other: a session can be alive without a current claim, and a claim can remain visible while its worker is not producing transcript activity.

## Worktree-local activity advisory

The SessionStart peer note uses a worktree-local observation cache, not a third truth family.
The existing PostToolUse heartbeat touches `<absolute-git-dir>/fno/live/<session_id>` at most every 30 seconds before any target-manifest or claim-holder gate, so a hand-started session contributes activity even when it owns no target claim.
Git gives each linked worktree its own administrative directory, which keeps this cache local even on the registered Claude WorktreeCreate path that can share the checkout's whole `.fno` directory.
The shared `hooks/helpers/worktree-live-peers.sh` reader emits a note only when another session's stamp is less than 120 seconds old; self-only and stale handoff stamps are silent.
Claude reaches the reader through `hooks/worktree-peers-session-start.sh`, while Codex reaches the same helper through its existing `hooks/session-start.sh` wrapper because the two harness manifests do not share a SessionStart carrier.

This cache answers only whether another session recently touched this worktree.

It never establishes death, orphaning, work ownership, or completion, and it is not registered on `PreToolUse`, so it cannot refuse `Edit`, `Write`, or `Bash`. Missing, malformed, or unreadable stamp state fails open to silence. Stale files are harmless because the reader judges only mtime and needs no retention sweep. The advisory includes the stable `fno-overlap-observed` marker. Both SessionStart carriers route that one observation through `fno workspace worktree overlap-record`. The machine-global event journal, not the transcript, is the durable, countable record. Read recurrence with `fno workspace worktree overlaps [--since DAYS] [--json]`. The carrier is advisory-only and always exits zero. A missing or old CLI, a rejected payload, lock contention, or a degraded fold surfaces `[fno-overlap-unrecorded]` or `[fno-overlap-count-unavailable]` rather than refusing a tool call. The shared predicate stays read-only and is never registered on `PreToolUse`.

Verify the writer and reader contracts with `bash tests/hooks/test_claim_heartbeat.sh` and `bash tests/hooks/test_worktree_live_peers.sh`.

## What is proved and what is assumed

Every liveness word the fleet renders is one of two things. A measurement, which some code actually took. Or an inference, which some code drew from evidence that does not entail it. The words look identical in a row, so this table says which is which.

| Record | What a reader takes it to assert | What the code probed | Proved? |
|---|---|---|---|
| claim `live` | the holder process is running | pid exists on this host, and its create time predates the claim | yes |
| claim `stale` | the holder is dead | the TTL lapsed and the pid is not live | yes |
| claim `suspect` | something is wrong with the holder | one bit: `is_live` returned false | no, five causes share the word |
| `liveness_origin` `survivor` or `resumed` | the pid started with, or after, the session | `created_at` and `pid_start_time` inside a 600 second band | yes |
| `liveness_origin: null` | no origin could be established | one of five named causes, in `liveness_origin_basis` | yes, since the basis names it |
| roster `live` from a spawn receipt | the worker can do work | a process started and a seed was accepted | no, it cannot show the model runs |
| roster `orphaned` | the worker died | a family-1 `done` or `stalled` verdict | yes for `stalled`, an inference for `done` |

`claim suspect` is the one row where a reader cannot recover the cause. `cli/src/fno/claims/staleness.py::is_live` returns false for five distinct situations: the claim is on another machine, the OS does not report the pid, the pid was reused, inspection was refused by permissions, and the holder was never a long-lived process. `claim_status` returns the raw inputs but no basis, so every caller reads one word for all five. A PreToolUse hook exits immediately, so every hook-registered hold sits in this state permanently and looks the same as a crashed worker.

### The claim classifier is duplicated and only partly guarded

`cli/src/fno/claims/staleness.py::classify` and `crates/fno-agents/src/claims.rs::classify` are two implementations of one rule, including the hybrid and suspect arms. Each says in a comment that it mirrors the other. No test drives both, so the comment is the only thing holding them together. They agree today, read line by line.

`liveness_origin` is the same shape and now has the guard the classifier lacks. Both producers read one corpus, `schemas/agents-row-contradiction.json`, asserted from Python in `cli/tests/agents/test_row_contradiction.py` and from Rust in `row_contradiction_fixture_matches_python_projection`.

That corpus is worth reading before trusting any parity claim here. It existed and drove both languages while the two producers still disagreed, because none of its cases carried the one row shape that separated them. A corpus blind to a divergence reads exactly like a corpus proving there is none, so add the case that fails before adding the rule that passes.

### `pid_start_time` is epoch microseconds on every platform

Both producers of this token now return epoch microseconds. The Linux path returned raw clock ticks since boot until it was changed, which is a small integer, and both row parsers refuse anything at or below the epoch-micros floor. So the field read null on every Linux row while a reader took null to mean no start time was recorded.

The unit was defensible while the value was only compared for equality against a capture for the same pid. `liveness_origin` broke that by comparing it to `created_at` as a wall clock. A later consumer cannot see an invariant that lives in a comment, which is why both platforms now agree rather than the comment being reworded.

## Read-side dispositions

| Surface | Disposition |
|---|---|
| `fno agents truth` | Keep as the canonical `alive?` verdict. |
| `truth_status` and `manifest-live` | Keep as the canonical `owns work?` verdict. |
| `discover_live_sessions` | Keep for enumeration; every caller routes only after family 1 classifies the row. |
| `peek` | Keep; it already shares the transcript reader with family 1. |
| claim PID and TTL classification | Keep inside family 2 only. |
| `control.sock` 250 ms probe | Keep only as a fast delivery pre-filter; a miss is not death. |
| recovery `state.json` | Keep for phase and error metadata; it is not a liveness oracle. |

No production path may declare a session dead or orphaned from socket miss, `state.json`, registry status, process-sidecar, daemon-row, or discovery-mtime evidence alone.
An inconclusive family-1 read makes no new death or orphan verdict and fails live routing quietly; only a family-1 `done` or `stalled` verdict establishes death.

### Terminal suppression needs family-2 artifact authority

Family 1 maps any terminal assistant `<promise>` to `done`, which is right for the `alive?` question and wrong as a completion verdict: a worker can promise and then die before shipping anything.
So the recovery watchdog's `SKIP_TERMINAL` (`recovery.classify`) does not suppress on that verdict alone.
It also reads the mission's external artifacts in the graph via `recovery.mission_complete`: for a `/target` mission, node `status: done` or a PR ref; for a *birth* `/think` design pass, a linked non-empty `plan_path`.
Only a birth pass is certified that way, because a lifecycle or conversational pass runs on a node that already carries the link and a retro is dispatched only once the node is `done`, so those artifacts predate the worker and describe the node rather than the invocation; they read unverifiable until an ownership lease can date them.
Only positive evidence of an *unfinished* mission relaxes the skip, and the candidate then falls through to the normal staleness gate, so a fresh promise mid-finalize is never acted on while fresh.
Claim state is deliberately not the authority here: claims are PID-anchored, so a finished worker and an abandoned one both read `suspect`/`stale`, and design-pass workers hold no node claim at all.
Which node a worker is on resolves from its manifest first, since the runtime wrote it and a worker name is only a convention; the exception is a `think-` named worker, which writes no manifest but runs with `--cwd` on the node's canonical root, where an unrelated `/target` session's manifest can sit.
Every probe failure (unreadable graph, unresolvable node, node-less thread) returns `None` and keeps the family-1 verdict, so the gate can only ever relax a `done`, never manufacture one.

## Mail boundary

The codex app-server daemon must predate a codex session for live mail injection to reach it.
An embedded codex session without that daemon is pane-only reachable, and durable mail addressed to it has no drain owner.
