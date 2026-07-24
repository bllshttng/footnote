# Session liveness

footnote answers two different questions with two different truth families.

Family 1 is `fno agents truth`, backed by `cli/src/fno/agents/session_truth.py`.
It reads the resolved transcript tail and content-aware activity age to answer whether a session is alive or working.

Family 2 is `truth_status` plus the target orienter's `manifest-live` check.
It reads claim liveness and loop-check recency to answer whether a session owns work.

Neither family substitutes for the other: a session can be alive without a current claim, and a claim can remain visible while its worker is not producing transcript activity.

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
It also reads the mission's external artifacts in the graph via `recovery.mission_complete`: for a `/target` mission, node `status: done` or a PR ref; for a `/think` design pass, a linked non-empty `plan_path`.
Only positive evidence of an *unfinished* mission relaxes the skip, and the candidate then falls through to the normal staleness gate, so a fresh promise mid-finalize is never nudged.
Claim state is deliberately not the authority here: claims are PID-anchored, so a finished worker and an abandoned one both read `suspect`/`stale`, and design-pass workers hold no node claim at all.
Which node a worker is on resolves from its manifest first, since the runtime wrote it and a worker name is only a convention; the exception is a `think-` named worker, which writes no manifest but runs with `--cwd` on the node's canonical root, where an unrelated `/target` session's manifest can sit.
Every probe failure (unreadable graph, unresolvable node, node-less thread) returns `None` and keeps the family-1 verdict, so the gate can only ever relax a `done`, never manufacture one.

## Mail boundary

The codex app-server daemon must predate a codex session for live mail injection to reach it.
An embedded codex session without that daemon is pane-only reachable, and durable mail addressed to it has no drain owner.
