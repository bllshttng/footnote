# Spawn a codex worker

```bash
fno agents spawn -H codex -m gpt-6-sol --effort high -Y --name <name> '$fno:target <node>'
```

The payload starts with `$fno:`, never `/`. Single-quote it. Inside double quotes the shell expands `$fno` to nothing, and the worker reads the rest as prose. The spawn refuses both bad shapes and names the fix.

Efforts: `low`, `medium`, `high`, `xhigh`, `max`. Models: `gpt-6-astra`, `gpt-6-sol`, `gpt-6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`, `gpt-5.5`. `-Y` maps to codex `--dangerously-bypass-approvals-and-sandbox`.

Known refusals:

- The footnote plugin is `missing` or `wrong-channel` in the codex home. The spawn refuses a verb-shaped seed instead of delivering prose. Run `fno config plugin install codex`, then spawn again.
- `--permission-mode` on a thread or headless one-shot. Use `--substrate pane`, or use `-Y`.
- `--agent` is rejected. Codex reaches sub-agents through its own project agents.
- A busy worker leaves mail in the composer. Peek the pane after every send.

The loop closes through the native Stop hook, so `$fno:target` is always admitted.

Back to [the harness index](beat-by-harness.md#spawn-by-harness).
