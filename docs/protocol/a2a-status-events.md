# a2a status-breakpoint events (protocol family v1)

The wire contract a third-party observer integrates against to track fno work as
it runs, depending only on the fno protocol (not on fno internals). Four
task-boundary events land in `events.jsonl`; two of them also push to a parent.
This is the LSP inversion: observers mold to this contract, so it is versioned,
self-contained, and additive-only.

- **Machine schema:** [`schemas/events-protocol-v1.json`](../../schemas/events-protocol-v1.json) (draft-07, no external refs).
- **Golden fixtures:** [`schemas/fixtures/events-protocol-v1/`](../../schemas/fixtures/events-protocol-v1/) (valid + invalid, testable without running fno).
- **Wire version:** `v: 1`. Additive-only within a version (no field removals or renames); a breaking change bumps `v`.

## The envelope

Routable fields live at the ENVELOPE level so an observer can filter on them
("notify me when MY worker blocks") without parsing per-type `data`. Only
type-specific payload stays in `data`. The family validates with
`additionalProperties: false`: an undeclared top-level field is rejected.

| Field | Presence | Meaning |
|---|---|---|
| `ts` | required | UTC RFC3339 timestamp. |
| `v` | required | Wire version (`1`). |
| `type` | required | `task_started` \| `task_done` \| `blocked` \| `run_summary`. |
| `source` | required | Producer class (`target`, `subagent`, `hook`, ...) or `worker:<id>`. |
| `run` | required | Target-run id. The dedup identity; distinct from the harness session id. |
| `data` | required | Per-type payload (see below). |
| `from` | session producers only | Sender canonical handle (see grammar). Omitted, never faked, for a non-session producer (cron, bare shell). |
| `model` | session producers only | Sender's own model string, or `unknown`. Omitted with `from`. |
| `host` | optional | Emitting host. |
| `project` | as applicable | Project the work belongs to. |
| `node` | as applicable | Backlog node id. |
| `task` | as applicable | Task id within the plan. Omitted for a flat plan with no task ids. |
| `parent` | when spawned | Parent spawn-lineage handle. |
| `outcome` | `task_done` / `run_summary` only | Return-contract enum: `SUCCESS` \| `DONE_WITH_CONCERNS` \| `FAILED` \| `BLOCKED`. |

## Handle grammar

A canonical handle addresses a session across harnesses as the bare first eight characters of its session id (for example, `03401fb3`).
`from` and `parent` carry canonical handles; the harness is separate metadata, never part of the address.

## Event kinds and `data`

| Event | Fires at | `data` |
|---|---|---|
| `task_started` | Just before an executor is dispatched. | `title` (capped), `executor`, `wave`. |
| `task_done` | When an executor's return is parsed. Carries `outcome`. | `commit`, `concerns`. |
| `blocked` | On a `RESULT: BLOCKED` return, or an in-session `<help>` distress. | `reason` (capped), `evidence` (capped). |
| `run_summary` | At the loop terminal (finalize). Carries `outcome`. | `tasks_started`, `tasks_done`, `tasks_failed`, `termination_reason`, `pr_url`. |

Free-text `data` strings (`title`, `reason`, `evidence`, `termination_reason`)
are truncated to a 500-byte cap at emit time; an oversized `run_summary` payload
is replaced by an `event_payload_too_large` meta-event on the Rust path.

## Push vs pull

- **Pull (all four):** every event lands in `events.jsonl` behind the file lock. A log tailer or peek reads them. Ticks (`task_started`, `task_done`) are never pushed.
- **Push (`blocked`, `run_summary`):** additionally sent to the `parent` handle over the mail bus (`fno mail send`) WHEN spawn lineage exists; no parent means no push. The mail verb writes the envelope durably BEFORE attempting live delivery, so the push is at-least-once. The push fires after the durable `events.jsonl` append, so the pull leg never depends on it.

## Sink contract

A consumer integrating this family:

- **Delivery is at-least-once.** Keep a cursor and dedup on the key `(run, task, type)`. A retried boundary emits `task_started` twice for the same `(run, task)`; a conforming consumer collapses them.
- **Order within a run by `ts`.** Cross-task ordering is not guaranteed (parallel-wave tasks interleave); order within a `run` by `ts`.
- **`blocked` obligates action.** It is the only event that requests action: surface it to a human or a supervisor within your routing policy. The other three are informational.
- **A gap is diagnosable.** `run_summary` carries `tasks_started` / `tasks_done`; `tasks_started > tasks_done` means an executor died without a parseable return. Detect it rather than assuming completeness.
- **Two producers on one node** are separable by `run` (distinct target-run ids); you see the collision instead of a merged mystery.

## CloudEvents mapping (webhook wrapper)

An outbound webhook sender (fno's fanout layer) maps an envelope to a CloudEvents
1.0 wrapper so consumers route on `type` without parsing internals:

| CloudEvents attribute | Value |
|---|---|
| `id` | hash of `(run, task, type, ts)` |
| `source` | `fno/<project>` |
| `type` | `sh.fno.<event>` (e.g. `sh.fno.blocked`) |
| `time` | the envelope `ts` |
| `data` | the full envelope object |

The wrapper is applied by the fanout sender; this family defines the mapping,
not the transport.

## Stability

Wire version `1` is additive-only: new optional envelope or `data` fields may be
added, but no field is removed or renamed, and no field changes meaning. A
breaking change increments `v`. Validate against
`schemas/events-protocol-v1.json` and test against the golden fixtures; a fixture
carrying an undeclared field is rejected, proving the contract has teeth.
