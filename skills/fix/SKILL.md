---
name: fix
description: "Repair a broken state. Routes to the fast one-fix-per-iteration loop with auto-revert (fix, default) or the scientific-method hypothesis loop (investigate). Use when: 'fix all errors', 'make tests pass', 'fix the build', 'debug this', 'investigate this failure'."
argument-hint: "[fix|investigate]  (fix: [from-debug] [--scope <glob>] [--guard <cmd>] [--category test|type|lint|build] [Iterations: N])"
metadata:
  requires:
    binaries:
      - "fno >= 0.1"
      - "git >= 2.0"
---

<!-- style-exception: mechanical verb rename preserves pre-existing prose -->
# Fix

**One verb on a broken state.** `/fix` routes between fast repair and methodical diagnosis.

| Mode | What runs | Use when |
|------|-----------|----------|
| `fix` (default) | fast one-fix-per-iteration loop, auto-revert on regression | you know roughly what is broken and want it green |
| `investigate` | scientific-method hypothesis loop (BDD criteria + failing repro) | the cause is unknown and you need to find it first |

This is a **router**. It parses the first argument as a mode, announces the resolved mode, then either runs the default loop here or loads the investigate reference and follows it. It never calls another skill at runtime (it dispatches the tournament-debugger via the Task/Agent tool and loads the investigate flow via Read).

## Step 0: Location preflight (before any write)

`/fix` writes code. Before resolving the mode, consult the shared location verdict (the SAME one `/target` and `/execute` use, so the canonical-main rule never drifts). Resolve the plugin root portably so the helper is found on non-Claude surfaces too (where `CLAUDE_PLUGIN_ROOT` is unset and the project checkout is NOT the fno plugin): try `CLAUDE_PLUGIN_ROOT`, then `CODEX_PLUGIN_ROOT`, then the persisted `~/.fno/plugin-root` pointer (written by `session-start.sh`), then the git root.

```bash
PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-${CODEX_PLUGIN_ROOT:-$(cat "$HOME/.fno/plugin-root" 2>/dev/null || git rev-parse --show-toplevel 2>/dev/null)}}"
LOC_HELPER="$PLUGIN_ROOT/hooks/helpers/check-impl-location.sh"
[[ -f "$LOC_HELPER" ]] && bash "$LOC_HELPER" || echo "verdict=ok"
```

## Known Limitations and Deferred Work

- Unreadable probes cannot prove a repair worked. See [LIMITATIONS.md](LIMITATIONS.md).

If the output carries `verdict=canonical-protected` AND `TARGET_LOCATION_OK` is not `main-acknowledged`, REFUSE: do not resolve the mode, do not write. Name the branch (from the `branch=` line) and print the exact escape, then stop:

```
/fix refused: canonical checkout on '<branch>' (sibling terminals share .fno/).
  worktree:  wt=$(fno agents workspace worktree ensure --repo . --name <slug> --harness <yours>) && cd "$wt"
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

Repair a broken state iteratively until the error count reaches zero or the loop exhausts its budget. The contract:

- **Target:** the detected failure queue; `from-debug` seeds it from the latest debug findings. An empty queue is a clean `nothing to fix` exit, never an iteration.
- **Guard:** the command a fix must not regress; inferred from the repo (test command in config.toml) or given. No guard means detection delta decides.
- **Bound:** `Iterations: 15` default; two rework attempts per item, then blocked.
- **Completion:** baseline vs remaining error counts, reverted fixes with reasons, and blocked items in `fix/{YYMMDD}-{HHMM}-{slug}/summary.md` - the receipts, not a claim.

Load [references/fix-loop.md](references/fix-loop.md) and execute it in full, in this context: detection, priority order, the atomic one-fix-per-iteration loop with the builder_step crumb, and the summary.

**Ask only unresolved consequential choices.** Infer what to fix, the guard, and the scope from the request and the repository (detected failures, config.toml, debug findings). When target information is genuinely missing, ask once in a single batched AskUserQuestion (what, guard, scope, iteration mode) with the detected failure counts in the first question - do not run the setup ceremony when the request already names its target.

### Decision rules (invariant, whichever body runs)

- Keep when `delta > 0` and the guard passes; revert when `delta <= 0`; revert immediately on regression and record the revert in the summary.
- Regression prevention is part of the fix: a fix that silences a check (`@ts-ignore`, `# noqa`, deleted or skipped failing tests, empty catch) is discarded and re-queued, never kept.
