# Hermes promise-tag reader

Writes `.fno/target-promise.signal` when the assistant response contains a `<promise>...</promise>` tag. The footnote loop wrapper reads that file to decide whether to keep looping.

## Install

```bash
mkdir -p ~/.hermes/plugins
ln -sfn /path/to/abilities/plugins/hermes/promise-tag-reader \
  ~/.hermes/plugins/promise-tag-reader
```

Restart hermes-agent. Confirm the plugin is loaded by running hermes once and checking startup logs for the plugin name.

## What it does

- Scans each assistant response for `<promise>...</promise>` tags (non-greedy, DOTALL).
- Takes the last match's inner content, strips whitespace.
- Writes it to `<cwd>/.fno/target-promise.signal` via atomic rename.

## Dependencies

Python 3.9+ standard library only. No external packages.

## Protocol reference

See [`docs/providers/promise-sentinel.md`](../../../docs/providers/promise-sentinel.md) for the full protocol.

## Why this plugin is optional

The target and megawalk loop skills already instruct the assistant to write the sentinel file when emitting a promise tag. This plugin is reinforcement for cases where the model forgets the instruction. Install both for maximum robustness.
