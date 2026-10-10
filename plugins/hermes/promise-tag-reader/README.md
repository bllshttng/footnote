# Hermes promise-tag reader

For each assistant response that contains a `<promise>...</promise>` tag, this plugin writes `.fno/target-promise.signal`. The signal records the model's claim. The footnote loop runtime does not stop on it.

## Install

```bash
mkdir -p ~/.hermes/plugins
ln -sfn /path/to/fno/plugins/hermes/promise-tag-reader \
  ~/.hermes/plugins/promise-tag-reader
hermes plugins enable promise-tag-reader
```

A directory plugin needs a `plugin.yaml` and an entry under `plugins.enabled` in `config.yaml`. Without both, Hermes does not load it. Confirm with `hermes plugins list`.

## What it does

- Runs on the `post_llm_call` hook, after each model reply.
- Scans each assistant response for `<promise>...</promise>` tags (non-greedy, DOTALL).
- Takes the last match's inner content, strips whitespace.
- Writes it to `<cwd>/.fno/target-promise.signal` via atomic rename.

## Dependencies

Python 3.9+ standard library only. No external packages.

## Protocol reference

See [`docs/harnesses/promise-sentinel.md`](../../../docs/harnesses/promise-sentinel.md) for the full protocol.

## Why this plugin is optional

The target and megawalk loop skills already instruct the assistant to write the sentinel file when emitting a promise tag. This plugin is reinforcement for cases where the model forgets the instruction. Install both for maximum robustness.
