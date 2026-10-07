# Summary: a bounded codex pane's seed rides a widened fno turn/start

## What landed

- `crates/fno-agents/src/codex_inject.rs`: `inject` takes a cwd hint, so the widened turn/start runs without a `thread/read` round trip; `deliver_seed_via_codex_daemon` sits beside the unchanged `deliver_via_codex_daemon`; an ack loss after the send reads `turn-start-unacked` so a caller can tell "in flight" from "failed".
- `crates/fno-agents/src/mail_inject.rs`: `--seed <cwd>`, a codex-only mode that skips the mail guards (body cap, single-line, verb risk, forged envelope, raw-inject audit) - a spawn seed is the spawner's own task, usually multi-line.
- `crates/fno-agents/src/codex_fake_daemon.rs`: `with_unacked_turn_start` closes the connection on `turn/start` to model the ack loss.
- `cli/src/fno/agents/mux_spawn.py`: a bounded (non-yolo) codex pane's seed is held out of the argv (the pane argv carries no seed for `--remote`), delivered after the thread binds through `codex_pane.deliver_seed`, with the typed fallback behind a definite delivery failure; an unconfirmed typed seed reaps the pane and fails the spawn, the pre-bind consequence.
- `cli/src/fno/agents/codex_pane.py`: `deliver_seed` shells `mail-inject --seed` with `FNO_WORKER_ADD_DIRS` set explicitly, since the pane-run env strips it.
- `docs/architecture/role-based-model-routing.md`: the bounded pane seed sentence now says the seed rides a fno turn/start carrying the roots.

## Deviation from the plan

The plan's unbound fallback was a bare typed seed; the post-reconcile required-binding gate reaps an id-less codex row regardless, so the fallback runs before that gate and an unconfirmed result reaps and raises (review round 1, fixed). An ack loss after the sent turn/start was not in the plan; it is named and treated as in flight (round 1, fixed). `fno do target init` left `graph_node_id` null: the spawn handover claim held the node, so the session executed under that claim with the orienter reading "no node bound".

## Verification

- `cargo` targeted: `codex_inject` (59 tests) and `mail_inject` (88): the seed with a cwd hint carries the roots and sends no `thread/read`; danger-full-access stays policy-less; ack loss reads `turn-start-unacked`; `--seed` parse, codex-only refusal, and a 6000-byte multi-line seed reaching delivery. `cargo fmt --check`: clean.
- Python: `test_spawn_pane_codex_seed.py` (8 tests), `test_spawn_pane.py` (journey now pins the yolo argv ride), `test_spawn_pane_codex_receipt.py`: green. ruff + mypy: 610 files clean. Flag registry, menu caps, `check-no-internal-refs.sh`, style lint: green.
- `check-file-budget.sh`: cli/src/fno added +30, at the 30-line budget, no over-budget file grew.
- Defect evidence on the real spawn path before the fix (rollout 01a0e9fa): first `turn_context` read `workspace-write`, `network_access: false`, `writable_roots: None`. The post-fix live proof runs after merge and `fno doctor update`, per the fleet convention: the receipt should read `seed_source: turn-start`, the rollout's first `turn_context` should carry the fno state root with network on, and `fno agents team --json` should answer `graph_readable: true`.
- Fleet note: during the live-proof attempt the shared codex app-server daemon stopped registering new threads (a control pane on the old code also failed to bind, 47 polls, "no new codex session for this cwd"); unrelated to this diff, needs fleet attention.
