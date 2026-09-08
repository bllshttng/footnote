# Summary

Ported the ready-selection decision into `fno-agents` (`backlog_ready::select`), served it over a new store-keeper `ready` verb, moved every caller onto it, deleted the Python cascade, converted the parity test to characterization over 23 frozen goldens.

## Deviations from the plan

- Parity oracle is `fno.backlog.explain.build_selection_filters` (the cascade driver), not `cmd_ready`: the characterization provenance gate requires an oracle symbol that is gone, and cmd_ready survives as the client verb.
- Seam crossings land at 64 of 64, not the planned 63: the king-board ready spawn was never a counted crossing, so the plan's target was miscounted.
- File budget passes with the CLI package net zero, not net smaller: surviving Python helpers each keep a named caller and are recorded as port-owed in the dual-implementation inventory.
- The three historical project-detection inputs collapse to one post-scope input, frozen by the goldens.
- `--parent` resolution narrows to exact id plus a unique 4-7 hex prefix; the childless-parent stderr hint is dropped.

## Notes

- Review round 1 (level max) fixed a client bug before ship: the transport-prefix on `no such node` defeated a `startswith` match, so a bad `--parent` reported a stale-keeper failure; it now classifies parent-missing (exit 1) and stale keepers separately. `find_node` gained `resolve_id`'s 4-7 hex partial gate.
- Live differential check matched on the real graph (`ready[0] == next`, 661 rows); a stale installed keeper answering `unknown store method` was bounced and now yields a named remedy instead of a raw error.
