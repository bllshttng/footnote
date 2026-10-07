# Spawn a pi worker

```bash
fno agents spawn -H pi --name <name> '/fno:target <node>'
```

Write the payload as `/fno:target`. The seed renders it as `/skill:target`, the form pi registers from its skills directory. Pi hosts the worker on a keeper-backed thread lane.

The loop closes through the `footnote.ts` extension in the pi agent dir. A `/fno:target` spawn refuses when that file is absent or differs from the packaged copy. The refusal names the path. Run `fno config plugin install pi`, then spawn again.

Known refusals:

- `spawn --resume` on the thread lane. Revive the same row by name with `fno agents resume <name>`.
- An unmeasured substrate. The spawn names it and stops.

Back to [the harness index](beat-by-harness.md#spawn-by-harness).
