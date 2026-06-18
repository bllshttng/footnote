---
name: pr
description: "Drive a PR through its lifecycle. Routes to create (open a PR via a Haiku worker), check (poll for external review and implement it), or merged (the post-merge ritual). Use when: 'create pr', 'open pr', 'submit pr', 'check pr', 'get review', 'post merge', 'process the merged PR'."
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
| `create` | open a PR: push the branch, generate a description from the commits, create the PR | a **Haiku** worker (the router stays in the main context) |
| `check` | poll for external review, implement findings, reply per-thread | the router's own main context |
| `merged` | the post-merge ritual: close the backlog node, harvest retro items, file follow-ups | the router's own main context |

This is a **router**, not a monolith. It parses the first argument as a mode, announces the resolved mode, then either dispatches a subagent (`create`) or loads that mode's body and follows it in this same context (`check`, `merged`). It never calls another skill at runtime - it dispatches the create worker via the Task/Agent tool and loads modes via Read.

**No default mode.** Unlike reviewing or fixing a diff, the three PR stages are distinct lifecycle actions: silently defaulting bare `/pr` to one of them could open a PR when you meant to check one, or run a ritual against the wrong PR. So bare `/pr` lists the modes and stops - you pick the stage.

## Step 1: Resolve the mode (ALWAYS announce it)

Parse the first argument token:

- **no argument** -> do NOT default and do NOT guess. Print the mode menu and stop with a non-zero result (dispatch nothing, open no PR):

  ```
  /pr needs a mode. valid modes:
    create       open a PR for the current branch (runs a Haiku worker)
    check        poll for external review on a PR and implement it
    merged       run the post-merge ritual for a merged PR
  ```

- **`create`** -> mode is `create`. Print `running create (PR via Haiku worker)`. The remaining tokens are create's own arguments. Go to "Step 2".
- **`check`** -> mode is `check`. Print `running check (poll for review)`. The remaining tokens are check's arguments (`[PR#]`). Go to "Step 3".
- **`merged`** -> mode is `merged`. Print `running merged (post-merge ritual)`. The remaining tokens are merged's arguments (`[PR#]`). Go to "Step 4".
- **any other non-empty token** -> this is an unknown mode (likely a typo). Do NOT default, do NOT guess. Print:

  ```
  unknown pr mode: '<token>'
  valid modes: create, check, merged (no default - pick a stage)
  ```

  and stop with a non-zero result. This is the locked router contract: an unknown or empty mode never silently falls through to an action.

## Step 2: create mode (open a PR via a Haiku worker)

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

### 2b. Dispatch the Haiku PR worker (the router stays in main context)

Announce the dispatch, then dispatch the bundled **pr-creator** subagent via the Task/Agent tool. The heavy PR-description generation runs in Haiku's cheap, fresh context; the router never does it inline - that is the whole cost property:

> State to the user: `dispatching the Haiku PR worker (pr-creator)`.

Dispatch with the Task/Agent tool:

- subagent type: **pr-creator** (the bundled Haiku agent at `agents/pr-creator.md`). On a runtime that resolves subagents by name, use that name; otherwise dispatch a general worker with the `agents/pr-creator.md` prompt and `model: "haiku"`.
- Pass ONLY the gathered context the worker needs - the current branch, the base branch, a one-line summary of the change, and the no-merge / auto-merge posture. Do NOT pass the full session transcript: Haiku's window is small and a fork would blow it.

`create.md` is the canonical create flow (the bundled copy of the standalone create-pr skill); `agents/pr-creator.md` is the same flow rewritten as the Haiku subagent. The router dispatches the agent via the Task/Agent tool - it never reaches a create skill through a runtime skill call.

### 2c. Parse the worker's RESULT line (no false success)

The pr-creator worker returns a `RESULT:` line. Parse it:

- `RESULT: SUCCESS` with a PR number + URL -> report the PR number and URL to the user.
- `RESULT: FAILED`, `RESULT: BLOCKED`, a dead / API-errored worker, or no PR created -> surface the worker's error line verbatim, do NOT claim a PR was created, and stop with a non-zero result.

A failed worker is never reported as a silent success. If the worker died without a `RESULT:` line, treat it as a failure and report that no PR was created.

## Step 3: check mode (poll for external review)

Load [check.md](check.md) and execute it in full, in this context. That body is the canonical review-polling flow: determine the configured reviewers, wait for review, fetch inline comments, parse priority badges, implement the findings, push fixes, and reply to each reviewer in-thread. It runs in the router's own main context (no subagent) and reaches no other skill at runtime.

## Step 4: merged mode (the post-merge ritual)

Load [merged.md](merged.md) and execute it in full, in this context. That body is the canonical post-merge ritual: resolve the per-project inbox path from settings (fail loud if unset), close + stamp the backlog node via `fno backlog reconcile`, harvest retro / carveout items, write prose follow-ups to the project's vault inbox, file triage-worthy work as backlog nodes, and offer a backfill / handoff slot before close. It runs in the router's own main context.

## Multi-CLI

Claude-Code primary. All three modes need `fno`, `gh`, and `git`. The create worker additionally needs the Task/Agent dispatch surface and a Haiku-capable provider; check needs the review bots configured in settings; merged needs the project's `config.post_merge.parking_lot_path`. If a dependency is missing, the mode fails loud and reports it - it never fakes a PR, a review, or a ritual.
