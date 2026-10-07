# Spawn an agy worker

```bash
fno agents spawn -H agy --effort high --name <name> '/fno:target <node>'
```

Agy is the Antigravity CLI. Efforts: `low`, `medium`, `high`. Permission modes: `default`, `skip`, `accept-edits`, `plan`, `sandbox`, and the `+sandbox` pairs. `--agent` is rejected.

The loop closes through a native Stop handler that `fno config setup` registers.

The seed receipt does not prove the worker read its seed. After every spawn:

1. Run `fno agents peek <name>` and look for a first worker action.
2. If the pane is idle and the composer is empty, send the prompt once. Run `fno mux pane send <pane> --text '<prompt>' --raw --submit`, then peek again.
3. If the composer already holds the seed, do not retype it. A second write appends to the buffer. Report the stranded composer instead.

Do not re-seed a pane that is already working. That queues a duplicate target.

Back to [the harness index](beat-by-harness.md#spawn-by-harness).
