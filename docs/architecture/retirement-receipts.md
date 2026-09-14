# Retirement receipts and the verification gate

What a retirement must prove, and how `fno agents reap --verify` checks it.

## The sequence

A retirement is one sequence shared by the scheduled sweep and the merge trigger, in `crates/fno-agents/src/gc_sweep.rs` (`stage_session_retirement`, `commit_retirements`).

The durable record comes first. The receipt is built from the registry row and the harness capability table. It is written to disk BEFORE any effect fires, and each effect appends its typed record and rewrites it. A crash after an effect leaves a receipt naming what happened, instead of a removal nothing recorded. A receipt that cannot be built or persisted refuses the retirement before the harness is touched, and the row is kept.

The effects, in order: the confirmed stop of the held process, the native active-surface removal, the mux squad-member retirement, and the resumability evidence measured off the receipt itself. The planning lane adds a gate of its own. A planner row retires only after its own blueprint/think `sessions[]` entry carries `ended_at`. That field is the positive marker `fno backlog session close` writes. It stops a quiet replanning worker from inheriting a completion an earlier assignment wrote. Under the commit, the graph is re-read. Any session that has gained an open do row is held before the registry write.

## The receipt and its required ops

Receipts live in `<agents home>/reap-receipts/`, one per retired session, keyed by harness and session id. Each carries a build stamp (`writer_build`) baked from the crates subtree rev, so a receipt written by another build skips instead of failing the audit.

`reap --verify` audits a window of the store against the CURRENT build. A pass means the promised outcome, not a nonempty list: every verified receipt must carry four effect ops, each at `confirmed-removed`, `confirmed-already-absent`, or `not-applicable`:

| Op | What it proves |
|---|---|
| `native-stop` | the held process was stopped, or confirmed already gone: a claude background thread gets `claude stop`, and every other row gets the removal's own process end (pane stop, mux pane kill, or worker socket stop) |
| `active-surface` | the harness's own listing no longer carries the session |
| `mux-member` | the session's squad membership is retired from the shared mux store through `fno mux retire-session`, or the store measures no live membership for it |
| `resume-evidence` | the receipt names resume tokens and a transcript that exists on disk |

`resume-evidence` is what separates a recovery record from an obituary. A `failed` outcome does not hold the row, because the session is already stopped. It marks the receipt unverifiable, so the gate refuses rather than certifies. A `failed` or `kept` `mux-member` outcome DOES hold the row: a live squad member still answers for the session until a mux server confirms its removal.

`--expect-sessions <a>,<b>` adds a cohort. Every named session must appear among the verified retirements, so a pass can cover a named set instead of whatever the window happens to hold. The report carries `expected` and `missing`.

The gate also derives its cohort, always on. It reads the retained events log (the active journal and its one rotated generation) and, for every in-window `agent_row_reaped` event that does not carry `receipt_staged`, checks that a receipt file exists for that harness and session id. Existence is the test, so a stale-build receipt is accounted for, and a retirement that dropped a row without persisting its receipt fails the gate even when nobody passed `--expect-sessions`. The report carries `reaped_events`, so a zero names itself.

## The rerunnable probe

`scripts/probes/retirement-gate-refuses-incomplete.sh` seeds an isolated home with the synthetic incomplete receipt (one op, current-build stamp), runs the verifier, and requires the refusal. Exit 0 with the refusal reason means the gate holds. Exit 1 means the gate certifies an incomplete retirement again. Run it against a freshly built binary before trusting the gate from source.
