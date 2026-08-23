---
name: pr
description: "Drive a PR through its lifecycle. Routes to create (open a PR via a routed pr-create worker), check (poll for external review and implement it), or merged (the post-merge ritual). Use when: 'create pr', 'open pr', 'submit pr', 'check pr', 'get review', 'post merge', 'process the merged PR'."
argument-hint: "<create|check|merged>  (create: opens a PR; check: [PR#]; merged: [PR#])  - a mode is required, there is no default"
requires:
  binaries:
    - "fno >= 0.1"
    - "gh >= 2.0"
    - "git >= 2.0"
---

# PR

**One verb for the PR lifecycle.** `/pr` routes to the right stage of getting a change reviewed and landed.

| Mode | What runs | Where it runs |
|------|-----------|---------------|
| `create` | open a PR: push the branch, generate a description from the commits, create the PR | a **pr-create** role worker (the router stays in the main context) |
| `check` | poll for external review, implement findings, reply per-thread | the router's own main context |
| `merged` | the post-merge ritual: close the backlog node, harvest retro items, file follow-ups | the router's own main context |

This is a **router**, not a monolith. It parses the first argument as a mode, announces the resolved mode, then either dispatches a subagent (`create`) or loads that mode's body and follows it in this same context (`check`, `merged`). It never calls another skill at runtime - it dispatches the create worker via the Task/Agent tool and loads modes via Read.

**No default mode.** Unlike reviewing or fixing a diff, the three PR stages are distinct lifecycle actions: silently defaulting bare `/pr` to one of them could open a PR when you meant to check one, or run a ritual against the wrong PR. So bare `/pr` lists the modes and stops - you pick the stage.

## Step 1: Resolve the mode (ALWAYS announce it)

Parse the first argument token:

- **no argument** -> do NOT default and do NOT guess. Print the mode menu and stop with a non-zero result (dispatch nothing, open no PR):

  ```
  /pr needs a mode. valid modes:
    create       open a PR for the current branch (runs a routed pr-create worker)
    check        poll for external review on a PR and implement it
    merged       run the post-merge ritual for a merged PR
  ```

- **`create`** -> mode is `create`. Print `running create (PR via pr-create worker)`. The remaining tokens are create's own arguments. Go to "Step 2".
- **`check`** -> mode is `check`. Print `running check (poll for review)`. The remaining tokens are check's arguments (`[PR#]`). Go to "Step 3".
- **`merged`** -> mode is `merged`. Print `running merged (post-merge ritual)`. The remaining tokens are merged's arguments (`[PR#] [autonomous]`); pass them through - a dispatched run appends `autonomous` so the ritual takes every no-prompt branch. Go to "Step 4".
- **`merge`** -> ambiguous: one word off `merged`, and on the opposite side of the merge event. Do NOT guess. Print and stop with a non-zero result:

  ```
  '/pr merge' is ambiguous - did you mean:
    fno do pr merge   land the PR now (the merge primitive, a CLI verb)
    /pr merged     run the post-merge ritual on an already-merged PR
  ```

  If the caller meant `fno do pr merge` and the verb REFUSES, read the refusal text. It names the sanctioned override. Those levers are the operator's. A worker escalates to the king/operator instead of pulling one. Never route around a gate refusal. Do not synthesize a config file. Do not flip a shared config key for the call's duration. Do not export a merge-time env var. Do not export TARGET_AUTO_MERGE before spawning a run. Unattended runs scrub it anyway. Two workers improvised the first three moves within sixty seconds of each other on 2026-08-19. One left a global merge switch ON machine-wide mid-merge. That exposure is not reversible the way a merge is.

- **any other non-empty token** -> this is an unknown mode (likely a typo). Do NOT default, do NOT guess. Print:

  ```
  unknown pr mode: '<token>'
  valid modes: create, check, merged (no default - pick a stage)
  ```

  and stop with a non-zero result. This is the locked router contract: an unknown or empty mode never silently falls through to an action.

## Step 2: create mode (open a PR via the pr-create role worker)

### 2a. Nothing-to-PR guard (before any dispatch)

If there are no commits ahead of the base, there is nothing to open a PR for. Report it and exit cleanly - never dispatch the worker against an empty branch:

```bash
BASE="${BASE:-origin/main}"
git fetch -q origin 2>/dev/null || true
if git rev-parse --verify --quiet "$BASE" >/dev/null 2>&1 \
   && [ -z "$(git log "$BASE"..HEAD --oneline 2>/dev/null)" ]; then
  echo "nothing to PR (no commits ahead of $BASE)"
  exit 0
fi
```

(If `origin/main` is not the right base for this repo, set `BASE` accordingly. If the base does not resolve, fall through and let the worker resolve it - do not block on an unknown base.)

### 2a-bis. Stale-base guard (before any dispatch)

A branch cut from a stale local HEAD ships a PR full of phantom deletions (changes you never made appear as reverts). Refuse before dispatching the worker; the check fails open on a fetch flake and points at `fno do pr rebase`:

```bash
BASE="${BASE:-origin/main}"   # re-default: this block may run standalone
ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
candidate_fno() {
  if [ -x "$ROOT/cli/.venv/bin/python" ]; then
    PYTHONPATH="$ROOT/cli/src" "$ROOT/cli/.venv/bin/python" -m fno.cli "$@"
  elif [ -f "$ROOT/cli/pyproject.toml" ] && command -v uv >/dev/null 2>&1; then
    PYTHONPATH="$ROOT/cli/src" uv run --project "$ROOT/cli" fno-py "$@"
  elif command -v fno >/dev/null 2>&1; then
    fno "$@"
  else
    echo "PR creation refused: fno CLI is unavailable." >&2
    return 127
  fi
}
candidate_fno do pr base-check --base "$BASE" || {
  rc=$?
  # 3 = stale, 4 = unrelated histories, 127 = missing CLI; all refuse.
  [ "$rc" -ge 3 ] && { echo "refusing to open a PR from a bad base (see above)."; exit "$rc"; }
}
policy_json="$(candidate_fno do pr evidence-required --base "$BASE")" || {
  echo "PR creation refused: verification policy could not be evaluated." >&2
  exit 1
}
policy_required="$(printf '%s' "$policy_json" | jq -er 'if .required == true then "true" elif .required == false then "false" else error("missing required state") end')" || {
  echo "PR creation refused: verification policy returned malformed state." >&2
  exit 1
}
if [ "$policy_required" = "true" ]; then
  candidate_fno do pr evidence-check --allow-rebase-equivalent || {
    echo "PR creation refused: no full/passed verification receipt for HEAD, and no earlier receipt whose patches match it." >&2
    echo "Run scripts/ci/preflight.sh (required by config.preflight.required = true)." >&2
    exit 1
  }
fi
```

### 2b. Dispatch the pr-create role worker (the router stays in main context)

Announce the dispatch, then dispatch the bundled **pr-creator** subagent via the Task/Agent tool. The heavy PR-description generation runs in a cheap, fresh context on the `pr-create` role's model; the router never does it inline - that is the whole cost property:

> State to the user: `dispatching the pr-create worker (pr-creator)`.

Dispatch with the Task/Agent tool:

- subagent type: **pr-creator** (the bundled agent at `agents/pr-creator.md`). Declare the `pr-create` role at the spawn boundary (`fno agents spawn --role pr-create`, or omit any `model:` override) so the model is resolved through `config.model_routing.roles.pr-create`; unconfigured, it runs on the invoking harness's primary model. No tier or model literal is hardcoded. On a runtime that resolves subagents by name, use that name; otherwise dispatch a general worker with the `agents/pr-creator.md` prompt and the same role declaration.
- Pass ONLY the gathered context the worker needs - the current branch, the base branch, a one-line summary of the change, and the no-merge / auto-merge posture. Do NOT pass the full session transcript: the worker's context is small and a fork would blow it.

`references/create.md` is the canonical create flow (the bundled copy of the standalone create-pr skill); `agents/pr-creator.md` is the same flow rewritten as the pr-create role subagent. The router dispatches the agent via the Task/Agent tool - it never reaches a create skill through a runtime skill call.

### 2c. Parse the worker's RESULT line (no false success)

The pr-creator worker returns a `RESULT:` line. Parse it:

- `RESULT: SUCCESS` with a PR number + URL -> report the PR number and URL to the user.
- `RESULT: FAILED`, `RESULT: BLOCKED`, a dead / API-errored worker, or no PR created -> surface the worker's error line verbatim, do NOT claim a PR was created, and stop with a non-zero result.

A failed worker is never reported as a silent success. If the worker died without a `RESULT:` line, treat it as a failure and report that no PR was created.

## Step 3: check mode (poll for external review)

Load [check.md](references/check.md) and execute it in full, in this context. That body is the canonical review-polling flow: determine the configured reviewers, wait for review, fetch inline comments, parse priority badges, implement the findings, push fixes, and reply to each reviewer in-thread. It runs in the router's own main context (no subagent) and reaches no other skill at runtime.

## Step 4: merged mode (the post-merge ritual)

Load [merged.md](references/merged.md) and execute it in full, in this context. That body is the canonical post-merge ritual: resolve the per-project inbox path from settings (fail loud if unset), close + stamp the backlog node via `fno backlog reconcile`, project stale plan frontmatter status from graph truth via `fno do plan reconcile-status --apply` (x-f34f), harvest retro / carveout items, write prose follow-ups to the project's vault inbox, file triage-worthy work as backlog nodes, and offer a backfill / handoff slot before close. It runs in the router's own main context.

## Known Limitations and Deferred Work

- PR reconciliation cannot prove unreadable external state. See [LIMITATIONS.md](LIMITATIONS.md).

## Multi-CLI

Claude-Code primary. All three modes need `fno`, `gh`, and `git`. The create worker additionally needs the Task/Agent dispatch surface and a provider for the configured `pr-create` route (or the invoking harness's primary model when unconfigured); check needs the review bots configured in settings; merged needs the project's `config.post_merge.parking_lot_path`. If a dependency is missing, the mode fails loud and reports it - it never fakes a PR, a review, or a ritual.
