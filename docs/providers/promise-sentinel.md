# Promise-tag sentinel protocol

The protocol that bot-side plugins (hermes, openclaw, and any future driver) implement so the external loop wrapper (`scripts/run-target-loop.sh`) can detect a `<promise>MISSION COMPLETE</promise>` tag reliably, without relying on raw stdout parsing.

## Why a sentinel file

The wrapper's fallback path is `grep <promise>MISSION COMPLETE</promise>` against the driver's captured stdout. That path works but has real edge cases:

- The tag appears nested inside a markdown code block that the agent is writing *about* (false positive).
- The response is chunked, and the tag straddles a chunk boundary.
- The driver wraps output in ANSI color codes, JSON framing, or line prefixes.

A sentinel file written by a bot-side plugin with structured access to the final assistant message parses correctly every time.

## File path and format

- **Path:** `.fno/target-promise.signal` inside the host project's working directory. The wrapper sets `$SIGNAL_FILE` to this value and exports it for the driver lib.
- **Format:** plain UTF-8 text, one line, the inner content of the *last* `<promise>...</promise>` tag in the assistant message. Trailing newline. No frontmatter, no JSON.
- **Encoding:** must be writable with `fs.writeFile` / `Path.write_text` equivalents. No binary, no magic prefixes.

Example content:

```
MISSION COMPLETE: all tasks done, tests passing, PR created
```

## Write semantics

Writes MUST be atomic. The canonical recipe:

1. Write to `.fno/target-promise.signal.tmp`.
2. Rename `.tmp` over the final path.

Python reference:

```python
tmp = signal_dir / "target-promise.signal.tmp"
dst = signal_dir / "target-promise.signal"
tmp.write_text(inner + "\n")
tmp.rename(dst)
```

TypeScript reference:

```ts
const tmp = path.join(signalDir, "target-promise.signal.tmp");
const dst = path.join(signalDir, "target-promise.signal");
await fs.writeFile(tmp, inner + "\n");
await fs.rename(tmp, dst);
```

Never write partial content. Never leave `.tmp` behind if the rename fails - the next plugin invocation should clean up stale `.tmp` files on startup.

## Read semantics

The wrapper reads the signal on every iteration's promise check. It:

1. Deletes the signal file at the start of each iteration (so a previous iteration's signal doesn't trigger a false "complete" this turn).
2. Invokes the driver.
3. Checks the signal file first. If present and contains `MISSION COMPLETE`, the loop exits 0.
4. If the signal is absent, falls back to `grep <promise>MISSION COMPLETE</promise>` against the driver's captured stdout.

The wrapper treats an absent signal as "promise not given, keep looping".

## Multi-tag handling

If the assistant response contains multiple `<promise>` tags, the **last one** wins. This lets the agent emit interim tags for observability without terminating the loop:

```
<promise>PHASE 2 DONE</promise>
... more work ...
<promise>MISSION COMPLETE: all phases shipped, PR #123 open</promise>
```

Only `MISSION COMPLETE` terminates the loop. Interim tags are advisory: the wrapper records them to the log but continues iterating.

## Absence semantics

- **Signal file absent:** the plugin either did not see a `<promise>` tag this turn, or was not installed. Wrapper falls back to stdout grep.
- **Signal file present but content does not contain `MISSION COMPLETE`:** interim tag only. Wrapper logs it and continues.
- **Signal file present and contains `MISSION COMPLETE`:** wrapper exits 0 (loop done).

## Implementing for a new driver

To add sentinel support for a fourth bot (Codex, Gemini, a custom CLI):

1. Find the bot's post-response hook primitive. The hook must receive the final assistant message text and the workspace / current-working-directory path.
2. Scan the response with the regex `<promise>([\s\S]*?)</promise>` (global, non-greedy, DOTALL).
3. Take the last match's capture group, trim whitespace.
4. Write via atomic-rename to `<cwd>/.fno/target-promise.signal`.
5. Document the install path in `docs/SETUP-<NAME>.md`.

Reference implementations:

- `plugins/hermes/promise-tag-reader/reader.py` (Python)
- `plugins/openclaw/promise-tag-reader/index.ts` (TypeScript)

Both are minimal (under 40 lines) and self-contained. Copy one as your template.

## Option A: SKILL.md-side fallback

Even without a plugin installed, the target and megawalk loop skills themselves include an instruction for the LLM: "when emitting `<promise>X</promise>`, also write `.fno/target-promise.signal` with the content `X`." This is pure prompt-level enforcement - works on every driver that honors the skill markdown.

Plugin (Option B) is reinforcement against LLM forgetfulness. Install both for maximum robustness; either alone works for most sessions.
