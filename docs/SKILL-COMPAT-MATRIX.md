# footnote skill compatibility by harness

Which footnote skills run on each harness. The table itself is generated: [docs/harnesses/verb-matrix.md](harnesses/verb-matrix.md), one row per `skills/*/SKILL.md`, one column per harness in `KNOWN_HARNESSES`. This page explains how to read it and what a wrapper-driven harness still needs.

## Where a cell comes from

A cell is a projection of two sources, never a hand-filled claim. The harness side is the capability table, `crates/fno-agents/src/harness_capabilities.toml`, rendered as [capability-matrix.md](harnesses/capability-matrix.md). The skill side is the `requires.harness` list in each skill's frontmatter, a closed vocabulary of `loop`, `spawn`, and `subagent_dispatch`. `fno doctor harness-matrix --write` joins the two. The rust-ci generated-copies step fails on a stale copy.

The states are the features vocabulary: `native` (fno drives it through a wired lane), `capable` (real on the harness, no wired arm), `absent` (measured not to exist), `unmeasured` (nobody has looked). A harness with no capability row reads `unmeasured` on every verb, which is the honest default: hermes and openclaw read that way today. A harness whose dispatch surface is refused reads `absent` on every verb. Gemini is that harness.

A row covers the skill's default lane. `execute` runs flat, `review` runs inline, `think` researches inline, so those rows declare no needs. Their sub-modes (`execute waves`, `review peer`, `think subagent`) need subagent dispatch and are not rows. A need the capability table has no key for, such as the claude prompt cache `cache-keepalive` depends on, cannot be declared. Add the key and its probe to the table first.

## Loop participation: which harnesses can close the loop

A verb that declares `loop` (target, reign, king-for-a-day) runs only where the stop-hook loop can STOP. That answer lives in one machine-readable field, `loop_participation` in `harness_capabilities.toml`, and `docs/harness-command-matrix.md` carries the per-harness evidence. `hooks/target-stop-hook.sh` shims `fno-agents loop-check`, which decides stop or allow from external truth. No lifecycle boundary means no `loop-check` invocation and no loop, however well the harness spawns, resumes, and receives mail.

| Value | What a caller gets | Cell |
|---|---|---|
| `native` | A shell hook fires at the lifecycle boundary and invokes `loop-check`. | `native` |
| `extension`, `loop_extension` set | No shell hook. The loop closes through a harness-native plugin fno ships. | `native` |
| `extension`, `loop_extension` empty | The harness exposes an extension point and fno ships nothing for it yet. | `capable` |
| `none` | No boundary at all. | `absent` |

The dispatch seam refuses a `/target` at a `none` harness and at an `extension` harness whose artifact fno has not written, naming the harness and the reason. A one-shot at either still dispatches, because the gate is scoped to the `/target` family. When you need the current answer, read the field rather than this page. The page is prose. The field is the contract.

## Wrapper-driven harnesses

A harness with no stop boundary can still run a looping skill through `scripts/run-target-loop.sh --driver <driver>`. The wrapper runs the bot as a subprocess and scans the output for `<promise>MISSION COMPLETE</promise>`. It re-invokes the bot with conversation history re-hydrated until the tag appears or the iteration cap is hit. The wrapper does not change a cell: it is a lane outside the harness, and the matrix reports the harness. See [SETUP-HERMES.md](./SETUP-HERMES.md) and [SETUP-OPENCLAW.md](./SETUP-OPENCLAW.md) for install and first-run recipes.

Driver-specific functions (`driver_invoke`, `driver_check_promise`, `driver_persist_history`, `driver_default_max`) live in `scripts/lib/driver-claude-code.sh`, `scripts/lib/driver-hermes.sh`, and `scripts/lib/driver-openclaw.sh`.

## How to add a new harness

1. Add the harness to `KNOWN_HARNESSES` in `cli/src/fno/harness_names.py`. It appears in the matrix as an all-`unmeasured` column at once.
2. Add its capability row to `harness_capabilities.toml`, measured cell by cell. `fno doctor harness <name> --live` is the onboarding gate.
3. Run `fno doctor harness-matrix --write` and commit both generated docs.
4. For a wrapper-driven harness, implement `scripts/lib/driver-<name>.sh` with the four function contract, optionally a promise sentinel plugin (`docs/harnesses/promise-sentinel.md`), and write `docs/SETUP-<NAME>.md`.

A new skill needs no step here. Its row appears from its `SKILL.md`. When its default lane needs the loop, a spawn, or a subagent, declare `requires.harness`.
