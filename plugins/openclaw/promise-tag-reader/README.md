# Openclaw promise-tag reader

Writes `.fno/target-promise.signal` when the assistant draft response contains a `<promise>...</promise>` tag. The footnote loop wrapper reads that file to decide whether to keep looping.

## Install

```bash
mkdir -p ~/.openclaw/plugins
ln -sfn /path/to/abilities/plugins/openclaw/promise-tag-reader \
  ~/.openclaw/plugins/promise-tag-reader
```

Restart openclaw. Confirm the plugin is loaded:

```bash
openclaw --list-plugins 2>&1 | grep promise-tag-reader
```

## What it does

- Registers a `before_agent_reply` hook (`src/plugins/hook-types.ts:55-84`).
- Scans `event.cleanedBody` for `<promise>...</promise>` tags.
- Takes the last match's inner content, strips whitespace.
- Writes it to `<ctx.workspaceDir>/.fno/target-promise.signal` via atomic rename.

## Dependencies

Node.js 18+ standard library only. No external packages.

## Protocol reference

See [`docs/providers/promise-sentinel.md`](../../../docs/providers/promise-sentinel.md) for the full protocol.

## Why this plugin is optional

The target and megawalk loop skills already instruct the assistant to write the sentinel file when emitting a promise tag. This plugin is reinforcement for cases where the model forgets the instruction. Install both for maximum robustness.
