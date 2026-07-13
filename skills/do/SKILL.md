---
name: do
description: "Execute a plan. Routes between a lightweight single-session executor (flat, default) and full wave orchestration (waves, alias operator). Use when: 'do this plan', 'execute the plan', 'run the waves'."
argument-hint: "[flat|waves|operator] <plan-path>"
requires:
  binaries:
    - "fno >= 0.1"
    - "git >= 2.0"
---

# Do

**One verb on a plan.** `/do` executes a plan. It routes between a lightweight single-session executor (the default) and full wave orchestration.

When `$CODEX_THREAD_ID` is nonblank, before any routing or work, Print exactly once:
`codex posture: do uses spawn_agent for wave tasks when available, with main-thread sequential fallback.`

| Mode | What runs | Use when |
|------|-----------|----------|
| `flat` (default) | lightweight single-session executor: read the plan, make the changes, verify, done | a focused plan: a bug fix, a 1-session feature, a single `.md` plan |
| `waves` (alias `operator`) | wave orchestration: validate the strategy, run waves sequential/parallel, dispatch TDD subagents, verify from a fresh perspective | a multi-phase plan whose Execution Strategy declares `- wave:` entries |

This is a **router**, not a monolith. It parses the first argument token as a mode, announces the resolved mode, then loads that mode's body and follows it in this same context. It never calls another skill at runtime (it dispatches subagents via the Task/Agent tool and loads mode bodies via Read).

> **Multi-CLI:** If not on Claude Code, see [references/cli-tool-mapping.md](references/cli-tool-mapping.md) for tool equivalents.

## Step 0: Location preflight (before any write)

`/do` writes code. Before resolving the mode, consult the shared location verdict (the SAME one `/target` and `/fix` use, so the canonical-main rule never drifts). Resolve the plugin root portably so the helper is found on non-Claude surfaces too (where `CLAUDE_PLUGIN_ROOT` is unset and the project checkout is NOT the abilities plugin): try `CLAUDE_PLUGIN_ROOT`, then `CODEX_PLUGIN_ROOT`, then the persisted `~/.fno/plugin-root` pointer (written by `session-start.sh`), then the git root.

```bash
PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-${CODEX_PLUGIN_ROOT:-$(cat "$HOME/.fno/plugin-root" 2>/dev/null || git rev-parse --show-toplevel 2>/dev/null)}}"
LOC_HELPER="$PLUGIN_ROOT/hooks/helpers/check-impl-location.sh"
[[ -f "$LOC_HELPER" ]] && bash "$LOC_HELPER" || echo "verdict=ok"
```

If the output carries `verdict=canonical-protected` AND `TARGET_LOCATION_OK` is not `main-acknowledged`, REFUSE: do not resolve the mode, do not write. Name the branch (from the `branch=` line) and print the exact escape, then stop:

```
/do refused: canonical checkout on '<branch>' (sibling terminals share .fno/).
  worktree:  git worktree add ~/conductor/workspaces/<repo>/<slug> -b feature/<slug>
  branch:    git checkout -b feature/<slug>
  override:  re-run with TARGET_LOCATION_OK=main-acknowledged
```

Otherwise (`verdict=ok`, a linked worktree, or the helper absent) continue to Step 1.

## Step 1: Resolve the mode (ALWAYS announce it)

The default mode `flat` takes a **plan path**. The `waves` mode (and its one-release alias `operator`) is selected by an **exact** leading keyword. Parse the first whitespace-delimited token:

- **no argument** -> error. The plan path is required. Print:
  > "Usage: `/do <plan>` (flat, default) or `/do waves <plan>` (wave orchestration). Pass a plan path."

  Stop with a non-zero result.
- **`flat`** -> mode is `flat` (explicit). Print `running flat (single-session)`. Consume the token; the remaining text is the plan path (error as above if empty). Go to Step 2.
- **`waves`** (or the alias **`operator`**) -> mode is `waves`. Print `running waves (wave orchestration)`. Consume the token; the remaining text is the plan path. An **empty** remainder is allowed here (unlike flat): bare `/do waves` runs wave orchestration on the current-directory plan. Go to Step 3.
- **a first token that is none of `flat`/`waves`/`operator` AND resolves as a path** (an existing file or directory) -> mode is `flat` (default). Print `running flat (default)`. The entire argument is the plan path. Go to Step 2.
- **a first token that is none of the above AND does not resolve as a path** -> this is an unknown mode / unresolvable plan (almost always a typo'd mode keyword or a wrong path). Do NOT default, do NOT guess. Print:

  ```
  unknown do mode / unresolvable plan: '<token>'
  valid modes: flat (default), waves (alias operator)
  usage: /do <plan>   |   /do waves <plan>
  ```

  and stop with a non-zero result (run nothing, dispatch nothing). This is the locked router contract: an unknown non-empty first token that is not a real plan path never silently falls through.

## Step 1.5: Stamp do provenance (execution entry, x-b6e4)

`/do` is the point where execution actually begins - the truthful `do` boundary,
unlike `target init` which fires before design/planning. Stamp the node the live
target is executing (Locked Decision 9). Two guards make the attribution honest:

1. **Trust `graph_node_id` only from a LIVE manifest** - a dead/stale
   `.fno/target-state.md` left in a reused worktree still carries an old node id.
2. **The plan being executed must agree** - if `/do <plan>` runs a plan whose
   `claims:` names a DIFFERENT node than the live manifest, the manifest node is
   not what this `/do` is executing, so skip rather than mis-attribute (never
   guess).

`$DO_PLAN_PATH` below is the plan path resolved in Step 1 (the `/do` argument, or
the current-directory plan for a bare `/do waves`); leave it empty if none.
Stamp once, here, before dispatching any wave/task:

```bash
LIVE="$(fno target status --json 2>/dev/null | jq -r '."manifest-live" // ""' 2>/dev/null)"
if [[ "$LIVE" == live* ]]; then
  # xargs trims whitespace and strips any surrounding quotes from the scalar.
  NODE_ID="$(sed -n 's/^graph_node_id:[[:space:]]*//p' .fno/target-state.md 2>/dev/null | head -1 | xargs)"
  PLAN_CLAIM=""
  [[ -f "$DO_PLAN_PATH" ]] && PLAN_CLAIM="$(awk '/^---[[:space:]]*$/{c++; next} c==1 && /^claims:/{sub(/^claims:[[:space:]]*/,""); print; exit}' "$DO_PLAN_PATH" | xargs)"
  if [[ -n "$PLAN_CLAIM" && "$PLAN_CLAIM" != null && -n "$NODE_ID" && "$PLAN_CLAIM" != "$NODE_ID" ]]; then
    echo "do provenance: plan claims '$PLAN_CLAIM' != live node '$NODE_ID' - conflict, skipping do stamp (never guess)." >&2
  elif [[ -n "$NODE_ID" && "$NODE_ID" != null ]]; then
    fno backlog session add "$NODE_ID" --phase do || true
  fi
fi
```

Idempotent, append-only, best-effort: harness + session id default from the
ambient identity; a missing-identity warning is non-fatal and never blocks
execution. No live target (standalone `/do` on a raw plan, or a dead manifest),
or a plan-vs-manifest node conflict -> skip silently; the node's `do` provenance
is stamped by the pipeline run whose live manifest and plan agree.

## Step 2: flat mode (lightweight single-session, default)

### 2a. Wave-declaration notice (flat on a wave plan)

Before executing, check whether the resolved plan declares waves. A plan "declares waves" when its single-doc body carries an Execution Strategy with `- wave:` entries - the same wave declarations target's blueprint treats as the source of truth. If it does, print a one-line notice and **proceed flat anyway** (flat is never silently upgraded to parallel):

```bash
PLAN_ARG="$1"  # the single .md plan path resolved in Step 1
if [[ -f "$PLAN_ARG" ]]; then
  # grep -c prints "0" AND exits 1 on zero matches, so `|| echo 0` would
  # append a second line ("0\n0") and break the numeric test. `|| true`
  # keeps grep's own single-line count; ${WAVES:-0} covers a grep error
  # (exit 2, no output).
  WAVES=$(grep -cE '^[[:space:]]*-[[:space:]]*wave:' "$PLAN_ARG" 2>/dev/null || true)
  if [[ "${WAVES:-0}" -gt 0 ]]; then
    echo "notice: this plan declares $WAVES waves; run \`/do waves $PLAN_ARG\` to parallelize. Proceeding flat."
  fi
fi
```

Defensive: an absent plan file or one with no wave entries yields zero waves and prints no notice - flat proceeds without crashing.

### 2b. Run the flat executor

Load [flat.md](references/flat.md) and execute it in full, in this context. That body is the lightweight single-session flow: read the plan, resolve the executor (frontend craft pass when the surface matches), execute each numbered change atomically (evaluating kill criteria first), verify, and report. It dispatches no subagents and tracks no STATE.md - you ARE the executor.

## Step 3: waves mode (wave orchestration)

Load [waves.md](references/waves.md) and execute it in full, in this context. That body is the canonical wave-orchestration flow: validate the plan structure, load the execution strategy from the plan's Execution Strategy section, optimize parallelism from the file ownership map, run waves in sequential/parallel mode, dispatch per-task executors (archer / frontend-executor) via Task/Agent, verify from a fresh perspective, and report completion.

**Self-containment (no recursion).** The router loads the waves flow **inline via Read** and follows it here. It never re-invokes a skill to reach wave orchestration; per-task work is dispatched via the Task/Agent tool.

**Zero-wave plan.** `/do waves` on a plan with no declared waves runs it as a single wave (the waves flow falls back to sequential execution of all phases when no execution strategy is found).

## Multi-CLI

Claude-Code primary. Both modes need `fno` and `git`. If a dependency is missing, the mode fails loud and reports it - it never fakes execution. On a CLI without parallel subagent dispatch (e.g. Gemini sequential fallback), waves mode degrades to main-thread sequential execution with an explicit downgrade reason; the rest of each flow is markdown the runtime follows directly.
