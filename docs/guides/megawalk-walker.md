# Walking the backlog (the megawalk successor)

The megawalk walker is retired: the `/megawalk` skill, `fno megawalk` CLI verbs, and the `--driver megawalk` loop arm are all deleted. This page is what to run instead. The runtime design of the retired walker lives on as a historical record in [unified-loop.md](../architecture/unified-loop.md).

## The supported routes

Pick by authority:

| You want | Run |
|---|---|
| Ship the next ready node | `/fno:target <node-id>` (or `fno backlog next` to see which node that is) |
| Dispatch the whole ready board | `/fno:target bg --all-ready` - every ready, non-deferred node becomes a background worker |
| Ship dependents after a merge | `fno backlog advance` - opt-in, merge-triggered; dispatches the next unblocked node |
| Always-on drain of an active mission | the active-backlog daemon ([dispatcher doc](../architecture/active-backlog-dispatcher.md)) |
| A crowned session that keeps driving a scope for days | `/fno:reign` |

Single-feature work still needs no backlog at all: `/fno:target "feature"` runs end to end.

## Watch progress

```bash
fno backlog next                          # what a dispatcher would pick next
fno agents claim list --prefix node:      # in-flight or parked node claims
```

Termination events land in `.fno/events.jsonl`. The loop terminates on `DonePRGreen`, `NoWork`, `Budget`, or `NoProgress`.

## Cancel

```bash
touch .fno/.target-cancelled   # the loop checks this sentinel between iterations
```

## Parked nodes

A parked node holds its `node:<id>` claim.

```bash
fno agents claim list --prefix node:                      # see held claims
fno agents claim release node:<id> --force --reason "..." # release for re-dispatch
```

## History

The old `fno megawalk status / pause / resume / bootstrap / reset / watch` subcommands were removed with the Python walker. The `/megawalk` skill, the `--driver megawalk` arm, and `loop_megawalk.rs` followed (removed 2026-08-03, see [path-census.md](../architecture/path-census.md)). The surface-change record from the earlier megawalk cleanup is in [megawalk-migration.md](../architecture/megawalk-migration.md).
