---
name: fix
description: "Repair a broken state. Routes to the fast one-fix-per-iteration loop with auto-revert (fix, default) or the scientific-method hypothesis loop (investigate). Use when: 'fix all errors', 'make tests pass', 'fix the build', 'debug this', 'investigate this failure'."
argument-hint: "[fix|investigate]  (fix: [from-debug] [--scope <glob>] [--guard <cmd>] [--category test|type|lint|build] [Iterations: N])"
requires:
  binaries:
    - "fno >= 0.1"
    - "git >= 2.0"
---

# Fix

**One verb on a broken state.** `/fix` routes between fast repair and methodical diagnosis.

| Mode | What runs | Use when |
|------|-----------|----------|
| `fix` (default) | fast one-fix-per-iteration loop, auto-revert on regression | you know roughly what is broken and want it green |
| `investigate` | scientific-method hypothesis loop (BDD criteria + failing repro) | the cause is unknown and you need to find it first |

This is a **router**. It parses the first argument as a mode, announces the resolved mode, then either runs the default loop here or loads the investigate reference and follows it. It never calls another skill at runtime (it dispatches the tournament-debugger via the Task/Agent tool and loads the investigate flow via Read).

## Step 0: Location preflight (before any write)

`/fix` writes code. Before resolving the mode, consult the shared location verdict (the SAME one `/target` and `/do` use, so the canonical-main rule never drifts). Resolve the plugin root portably so the helper is found on non-Claude surfaces too (where `CLAUDE_PLUGIN_ROOT` is unset and the project checkout is NOT the abilities plugin): try `CLAUDE_PLUGIN_ROOT`, then `CODEX_PLUGIN_ROOT`, then the persisted `~/.fno/plugin-root` pointer (written by `session-start.sh`), then the git root.

```bash
PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-${CODEX_PLUGIN_ROOT:-$(cat "$HOME/.fno/plugin-root" 2>/dev/null || git rev-parse --show-toplevel 2>/dev/null)}}"
LOC_HELPER="$PLUGIN_ROOT/hooks/helpers/check-impl-location.sh"
[[ -f "$LOC_HELPER" ]] && bash "$LOC_HELPER" || echo "verdict=ok"
```

If the output carries `verdict=canonical-protected` AND `TARGET_LOCATION_OK` is not `main-acknowledged`, REFUSE: do not resolve the mode, do not write. Name the branch (from the `branch=` line) and print the exact escape, then stop:

```
/fix refused: canonical checkout on '<branch>' (sibling terminals share .fno/).
  worktree:  git worktree add ~/conductor/workspaces/<repo>/<slug> -b feature/<slug>
  branch:    git checkout -b feature/<slug>
  override:  re-run with TARGET_LOCATION_OK=main-acknowledged
```

Otherwise (`verdict=ok`, a linked worktree, or the helper absent) continue to Step 1.

## Step 1: Resolve the mode (ALWAYS announce it)

Parse the first argument token:

- **`investigate`** -> mode is `investigate`. Print `running investigate (hypothesis loop)`. Go to "Investigate mode".
- **`fix`** -> mode is `fix` (explicit). Print `running fix (repair loop)`. Consume the token; the rest are fix's arguments. Go to "Fix mode".
- **empty, `from-debug`, a `--flag`, or `Iterations: N`** -> mode is `fix` (default). Print `running fix (default)`. Keep the token as fix's own argument. Go to "Fix mode".
- **any other bare non-flag word** -> this is an unknown mode (likely a typo). Do NOT default, do NOT guess. Print:

  ```
  unknown fix mode: '<token>'
  valid modes: fix (default), investigate
  ```

  and stop with a non-zero result. This is the locked router contract: an unknown non-empty mode never silently falls through.

## Investigate mode

Load [investigate.md](references/investigate.md) and execute it in full, in this context. That reference is the canonical scientific-method debugging loop: define acceptance criteria, prove the bug with a failing reproduction, then test one falsifiable hypothesis per iteration. When it finishes with confirmed findings, the natural next step is the default repair loop - `/fix from-debug` consumes those findings in severity order.

## Fix mode (default)

Repair a broken state iteratively until the error count reaches zero or the loop exhausts its budget.

### Reference Materials

Load these references as needed:

- [references/iteration-loop.md](references/iteration-loop.md)
- [references/verification-patterns.md](references/verification-patterns.md)

### Defaults

- Default bounded run: `Iterations: 15`
- Output root: `fix/{YYMMDD}-{HHMM}-{slug}/`

### Interactive Setup

If the user did not provide explicit target information, auto-detect failures first and then ask in one batched AskUserQuestion call:

1. what to fix
2. guard command
3. scope
4. iteration mode

The first question should summarize the detected failure counts.

### Process

#### 1. Detect

Use `verification-patterns.md` to identify:

- build failures
- critical/high debug findings
- type errors
- test failures
- lint errors
- warnings

If `from-debug` modifier is set, read the latest `debug/*/findings.md` (or `.fno/debug/*.md`) and populate the queue from confirmed bugs first.

**Nothing-to-fix exit (EDGE).** If detection finds zero failures across every category (clean working tree, no failing test, no debug findings), report `nothing to fix` and exit cleanly. Do NOT enter the iteration loop on an empty queue.

#### 2. Prioritize

Fix order:

1. build
2. critical/high bugs
3. type
4. test
5. medium/low bugs
6. lint
7. warnings

#### 3. Iterate

Load `iteration-loop.md` and run this atomic loop:

1. pick the highest-priority unfixed item
2. read the relevant code and error context
3. make one focused fix
4. `git commit` before verify
5. re-run detection to compute `delta`
6. run the guard command if provided
7. keep, revert, or rework
8. log to `fix-results.tsv`
9. emit ONE `builder_step` crumb (below)

Maximum rework attempts per item: 2. After that, add it to the blocked list and continue.

#### Per-iteration crumb (builder trail)

After the keep/revert/rework decision, append one `builder_step` to `.fno/events.jsonl` so a resume or self-handoff successor picks up from the attempt trail instead of repeating a failed approach. One crumb per iteration, at the boundary - never per tool call. Map the loop's own result: kept -> `worked`, reverted -> `failed`, blocked-after-rework -> `abandoned`.

```bash
fno event emit --type builder_step \
  -d '{"tried":"<the fix attempted>","found":"<what detection/guard showed>","fix":"<the change made>","outcome":"worked|failed|abandoned"}' \
  || echo "warning: builder_step crumb not recorded (continuing)" >&2
```

Truncate `tried`/`found`/`fix` to ~500 chars each (a crumb is a pointer, not a transcript). `found`/`fix` are optional; `tried` and `outcome` are required. Degrade, never block: a failed emit prints exactly the one warning above and the loop continues - no retry.

#### 4. Summary

Write:

- `fix/{YYMMDD}-{HHMM}-{slug}/fix-results.tsv`
- `fix/{YYMMDD}-{HHMM}-{slug}/summary.md`

`summary.md` must include:

- baseline error count
- fixed count by category
- remaining errors
- blocked items
- every reverted fix and why it reverted (regression / non-positive delta), so an auto-reverted attempt is reported, not silently dropped
- suggestion to run a code review skill if available (e.g., `/review`), or run the project's test/lint/build commands to verify fixes

### Category Strategies

| Category | Strategy |
|----------|----------|
| build | fix the exact import, syntax, or config break |
| type | proper types, null handling, generics, explicit narrowing |
| test | fix implementation, not the test, unless the test is provably wrong |
| lint | satisfy the rule, do not suppress it |
| bug | apply the concrete fix implied by the debug evidence |

### Anti-Pattern Blocklist

Discard and re-queue any fix that uses:

- `@ts-ignore`
- `eslint-disable`
- `# type: ignore`
- `# noqa`
- `any` used only to silence type errors
- deleted or skipped failing tests
- empty catch blocks
- hardcoded values chosen only to satisfy one test

### Decision Rules

- Keep when `delta > 0` and the guard passes
- Rework when `delta > 0` but the guard fails
- Revert when `delta <= 0`
- Revert immediately on regression, and record the revert in the summary

### Composite Metric

```text
fix_score = reduction * 0.60 + guard_health * 0.25 + quality * 0.15
```
