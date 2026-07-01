# Batch-lane loop integration (Wave 2/3)

**Opt-in** (`config.batch.enabled`, default false). When off, the auto-continue daemon behaves byte-for-byte as today (one PR per node). This doc covers how batched mode runs in the live loop; the design-of-record is `internal/fno/plans/batch-lane.md`.

## Why

The autonomous pipeline ships many small PRs, and GitHub Actions bills per run. Ten tiny nodes = ten CI runs. Batch-lane coalesces N same-domain ready nodes onto one branch and opens **one** PR per batch, cutting CI runs ~N×. Wave 1 (the `fno backlog batch` state primitive + pure policy engine) shipped in PR #129; this is the loop wiring (Wave 2) + per-batch ship (Wave 3).

## The flow

The auto-continue daemon (`crates/fno-agents/src/active_backlog.rs`, the keep-set - `loop_megawalk.rs` is untouched) drives it:

1. **Prepare (before dispatch).** `BatchDispatcher` shells `fno backlog batch prepare --node <id> --repo <root>`. The Python verb consults `decide_batch_action`:
   - `ship_solo` (batching off, or `size:L`/`p0`) → dispatch today's `/target no-merge <id>`.
   - `start` → `fno worktree ensure` a shared batch worktree off `origin/main` + `fno backlog batch open`.
   - `join` → reuse the open batch's recorded worktree/branch.
   On batch, the daemon dispatches `/target batched <id>` with `TARGET_BATCHED=1` + `TARGET_BATCH_WORKTREE`/`TARGET_BATCH_BRANCH`. Prepare is fail-safe: any error degrades to solo.

2. **Batched member run.** `/target batched` skips cold-start (uses the provided worktree), inits with `batched: true` in the manifest, implements + commits atomically to the shared branch, `fno backlog batch join`s + marks the graph node (`fno backlog update --batch <id>`, which `next`/`ready` then exclude), and returns **without** a PR.

3. **DoneBatched terminal.** `loopcheck.rs` reads the `batched: true` manifest flag and terminates the member as `DoneBatched` on its promise - not a hang waiting for a per-node PR. `DoneBatched` is deliberately **not** in finalize's `SHIP_REASONS`, so a member finalizes to a ledger record only and never stamps/graduates the plan (the batch PR does that once, for all members). In `active_backlog.rs`, `map_outcome` treats a `DoneBatched` unit as a successful dispatch so batched members never trip the circuit breaker.

4. **Ship on close.** After each tick the daemon shells `fno backlog batch ship-closeable`, which peeks the next ready node and, for each open batch whose `should_close` tripped (full / next node a different domain / drain), opens **one** PR for the shared branch and records the shared `pr_number`/`pr_url` on every member. Members are **not** marked `done` here (the PR is not merged yet).

5. **Completion at merge.** On merge, `fno backlog reconcile` closes each member independently by its own `pr_number` - so the "shared URL" (Locked Decision 5) is just N identical `pr_url` values the existing per-node close already handles. No `done`/`plan stamp` change was needed.

## Failure isolation (v1)

Any batched member returning FAILED/BLOCKED, or a batch PR that cannot open, **abandons the batch**: `ship_batch` calls `abandon_batch` and clears every member's `batch` mark, so they resurface in `next` and ship as individual PRs. Worst case a bad batch costs the same CI as no batching - never worse. Surgical revert-on-fail (Wave 4) is deferred to v2.

## Where each piece lives

| Concern | File |
|---|---|
| State primitive + policy + `ship`/`prepare`/`ship-closeable` | `cli/src/fno/backlog/batch.py` |
| next/ready exclusion + `update --batch` | `cli/src/fno/graph/cli.py` |
| Daemon batched dispatch + close trigger + breaker | `crates/fno-agents/src/active_backlog.rs` |
| `DoneBatched` terminal | `crates/fno-agents/src/loopcheck.rs`, `loop_target.rs` |
| `batched` manifest flag | `hooks/helpers/init-target-state.sh` |
| Batched member run | `skills/target/SKILL.md` (§0c) |
| Per-repo enable in daemon config | `cli/src/fno/active_backlog.py` |

## Acceptance

Everything above the live-daemon boundary has real unit/integration tests. The end-to-end acceptance - enable `config.batch.enabled`, run the daemon over N small same-domain nodes, confirm ~N/max_nodes PRs and each member stamps against the shared URL at merge - is owner-driven (it needs the live daemon and real GitHub).
