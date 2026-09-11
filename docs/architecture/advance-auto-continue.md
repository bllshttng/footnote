# The advance auto-continue contract

Moved from `cli/src/fno/backlog/advance.py` under the file-budget gate's remedy (long prose lives in docs, modules ship code). Content unchanged.

Node ab-3cd195b6. When a backlog node's PR merges, a merge-detector (`fno backlog reconcile` or the /pr merged skill) calls this verb after the node-close write commits. If auto-continue is armed for the project and no live walk owns it, advance dispatches a fresh background `/target` worker (with the merge posture from `config.auto_merge.grant`, default `none`) for the next now-unblocked node, so a merge-gated epic walks itself group-by-group across merges with no manual re-invocation.

## Locked decisions this module embodies

1. Decoupled from the loop driver: driven by the merge event, so megawalk, `/target` and `/megatron` all inherit auto-continue (no driver-specific code).
4. Fire-and-forget dispatch: `fno agents spawn` -> `/target [--no-merge] <id>`, the `--no-merge` flag gated on `config.auto_merge.grant` (x-4391/x-4be1).
5. Concurrency via `fno agents claim`: honor `walker:<root>` (no double-dispatch during a live walk); reserve `dispatch:<id>` (O_EXCL dedup plus a bridge token that outlives this short-lived process until the worker owns `node:<id>`, LD#11 / AC1-CLAIM, mirroring handoff.sh and dispatch-node.sh).
6. advance never merges the PR itself; it dispatches a worker whose merge posture comes from `config.auto_merge.grant` (default `none`). An actual merge, when enabled, is still gated by the worker's own `config.auto_merge.*` review layer (x-4391, revisits epic LD#4).
7. Non-fatal: a failed spawn never wedges the host op (reconcile/post-merge).
12. Every code path emits EXACTLY ONE decision event before returning (`advance_dispatched` | `advance_skipped{reason}` | `advance_failed`), so a silent stall is impossible.

## The dispatch reservation

The `dispatch:<id>` reservation uses a TTL claim (not PID-liveness) precisely so it survives advance's exit (AC1-CLAIM): the just-dispatched node stays "claimed" for the boot window, so a concurrent reconcile/post-merge sees it as already-being-worked. The spawned worker acquires `node:<id>` cleanly on its own `fno do target init` (free at that point); the reservation then expires by TTL once the worker owns the node.
