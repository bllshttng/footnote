# Spawn an opencode worker

```bash
fno agents spawn -H opencode -m glm-5.3-flash --name <name> '/fno:target <node>'
```

The payload starts with `/fno:`. Opencode declares no efforts, so leave `--effort` off. The one permission mode is `auto`. `--add-dir` is rejected.

The loop closes through a JS extension that fno installs, not through a shell hook. A `/fno:target` payload therefore needs a current install on this machine. The loop gate checks it before the spawn.

Known refusal: `closes its loop through a fno-installed extension that is absent or stale`. The text in parentheses names the cause.

- `opencode binary was not found`: fno looks at `FNO_OPENCODE_BIN`, then `~/.opencode/bin/opencode`, then `opencode` on PATH. Set the variable or install the binary.
- `installed X vs source Y`: the extension is behind the plugin source. Run `fno config plugin install opencode`.
- `the opencode contract changed since install`: opencode crossed a major version after the install. Run the same install command.

A one-shot payload with no `/fno:target` skips this gate.

Back to [the harness index](beat-by-harness.md#spawn-by-harness).
