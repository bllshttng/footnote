# Spawn a claude worker

```bash
fno agents spawn -H claude -m sonnet --effort high --name <name> '/fno:target <node>'
```

The payload starts with `/fno:`. The default substrate is `thread`. Add `-Y` to bypass permissions (claude `bypassPermissions`). Add `--substrate pane` only when a human must watch the worker.

Efforts: `low`, `medium`, `high`, `xhigh`, `max`. Permission modes: `default`, `acceptEdits`, `auto`, `dontAsk`, `plan`, `bypassPermissions`. Models: `opus`, `sonnet`, `haiku`, `fable`, or a full model id.

Only claude takes these flags. Every other harness refuses them.

- `--route <provider/model>` sends the worker to another vendor, for example `--route zai/glm-5.3`. An unknown provider or a missing key refuses the spawn.
- `--account <name>` pins one registered claude account.
- `--tools` and `--deny-tools` scope the tool list.

The loop closes through the native Stop hook, so `/fno:target` is always admitted.

Back to [the harness index](beat-by-harness.md#spawn-by-harness).
