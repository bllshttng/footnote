# Retirement receipts and the verification gate

What a retirement must prove, and how `fno agents reap --verify` checks it.

## The sequence

A retirement is one sequence shared by the scheduled sweep and the merge trigger, in `crates/fno-agents/src/gc_sweep.rs` (`stage_session_retirement`, `commit_retirements`).

The durable record comes first. The receipt is built from the registry row and the harness capability table. It is written to disk BEFORE any effect fires, and each effect appends its typed record and rewrites it. A crash after an effect leaves a receipt naming what happened, instead of a removal nothing recorded. A receipt that cannot be built or persisted refuses the retirement before the harness is touched, and the row is kept.

The effects, in order: the confirmed stop of the held process, the native active-surface removal, and the resumability evidence measured off the receipt itself. The planning lane adds a gate of its own. A planner row retires only after its own blueprint/think `sessions[]` entry carries `ended_at`. That field is the positive marker `fno backlog session close` writes. It stops a quiet replanning worker from inheriting a completion an earlier assignment wrote. Under the commit, the graph is re-read. Any session that has gained an open do row is held before the registry write.

## The receipt and its required ops

Receipts live in `<agents home>/reap-receipts/`, one per retired session, keyed by harness and session id. Each carries a build stamp (`writer_build`) baked from the crates subtree rev, so a receipt written by another build skips instead of failing the audit.

`reap --verify` audits a window of the store against the CURRENT build. A pass means the promised outcome, not a nonempty list: every verified receipt must carry three effect ops, each at `confirmed-removed`, `confirmed-already-absent`, or `not-applicable`:

| Op | What it proves |
|---|---|
| `native-stop` | the held process was stopped, or confirmed already gone |
| `active-surface` | the harness's own listing no longer carries the session |
| `resume-evidence` | the receipt names resume tokens and a transcript that exists on disk |

`resume-evidence` is what separates a recovery record from an obituary. A `failed` outcome does not hold the row, because the session is already stopped. It marks the receipt unverifiable, so the gate refuses rather than certifies.

`--expect-sessions <a>,<b>` adds a cohort. Every named session must appear among the verified retirements, so a pass can cover a named set instead of whatever the window happens to hold. The report carries `expected` and `missing`.

Mux effects are NOT required. No fno-agents call site emits a mux effect record yet. The mux server transport exists on the daemon side, and the wiring is owned by the transport epic. The refusal text names this as context so the absence reads as known, not as a regression.

## The rerunnable probe

`scripts/probes/retirement-gate-refuses-incomplete.sh` seeds an isolated home with the synthetic incomplete receipt (one op, current-build stamp), runs the verifier, and requires the refusal. Exit 0 with the refusal reason means the gate holds. Exit 1 means the gate certifies an incomplete retirement again. Run it against a freshly built binary before trusting the gate from source.
