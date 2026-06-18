# Memory System

## Overview

The memory system captures session learnings and writes them into project-scoped
memory files under `~/.claude/projects/{project}/memory/`. These entries persist
across sessions and surface to subsequent runs, letting the agent accumulate
corrections, validated approaches, and project facts rather than rediscovering
the same lessons. The goal is compounding: each shipped PR leaves the system
slightly smarter than it was before.

## Two-checkpoint memory pass

Memory is written at two points in the target lifecycle. Both checkpoints use the
main-thread LLM (full session context), in contrast to the deprecated Haiku
subprocess which only saw a 50-line tail of structured signals. This matters:
the main thread knows why a decision was made; the subprocess only saw that
something happened.

| Checkpoint | Trigger | Script |
|---|---|---|
| Pre-promise | target skill body, before `<promise>` emission | built into `skills/target/references/pre-promise.md` |
| Post-merge | `pr-merge.sh` success sentinel `.fno/.memory-pass-pending` | `scripts/memory/post-merge-pass.sh` |

Both checkpoints call `scripts/memory/write-memory-entry.sh` as their writer.
The file format and dedup semantics are unchanged from the deprecated distill path.

## Pre-promise pass

The pre-promise step is part of the standard target completion sequence, documented
in `skills/target/references/pre-promise.md`. Before emitting the `<promise>` tag,
target scans the just-completed session for:

- Corrections the user gave during the session
- Surprising behaviors that should not be repeated
- Validated approaches confirmed by tests or review
- Project facts that were discovered or changed (active PRs, schema decisions, etc.)

For each memory-worthy candidate, the pass calls `write-memory-entry.sh`. The
`--session-id` is taken from the current target session. The pass runs in the
same context window as the rest of the session, so no subprocess is spawned and
no extra LLM call cost is incurred beyond normal session cost.

## Post-merge pass

`pr-merge.sh` writes the sentinel file `.fno/.memory-pass-pending` for
both successful merge outcomes (`merged`: PR was merged immediately) and
queued outcomes (`queued`: PR is set to auto-merge after required checks
pass). The queued path is the dominant target auto-merge case, and reviewer
signal often arrives between queue-time and the actual server-side merge,
so the sentinel must survive until the merge actually lands.

The sentinel is consumed in one of two ways:

1. **`/pr check`** - when it polls for external review and detects a merged PR,
   it runs `post-merge-pass.sh` before returning control to target.
2. **Stop hook fallback** - the COMPLETE branch in `hooks/target-stop-hook.sh`
   checks for `.fno/.memory-pass-pending` and runs `post-merge-pass.sh`
   if present. This catches the case where `pr-merge.sh` wrote the sentinel
   but `/pr check` had already exited.

`post-merge-pass.sh` queries the PR's state and `mergedAt` first:

- **`MERGED`**: discovery runs (late comments, late reviews, ungraduated
  done-with-concerns artifacts) and emits a JSON blob to stdout. The caller
  decides what is memory-worthy and calls `write-memory-entry.sh`.
- **`OPEN`**: the merge has not landed yet (queued auto-merge). The script
  exits 0 silently and the sentinel is **preserved** so the next invocation
  can retry once the merge lands.
- **`CLOSED`** (without merge): the script removes the sentinel and exits 0.
  Nothing to capture.

The sentinel is also preserved when `gh` API calls fail mid-stream (exit
code 2) so transient outages do not silently drop signal. The sentinel is
removed only when discovery actually succeeds and the JSON output is emitted.

## Why we deprecated Haiku distillation

The original distill system (`scripts/memory/distill-session.sh`) ran a Haiku
subprocess after each COMPLETE session. It had three compounding problems:

1. **No context.** It received a 50-line tail of `convo-signals.jsonl`, not
   the actual session. That tail was typically 47 of 50 entries of
   `repeated_tool_pattern` noise.
2. **Mis-labeled signals.** The `repeated_corrections` signal-type was capturing
   HARD-GATE skill preambles, not actual corrections from the user. Every
   auto-memory that landed was a duplicate of `feedback_graph_json_*_direct_edit`.
3. **Recursion hazard.** The Haiku subprocess inherited the stop hook. Without the
   `TARGET_INSIDE_DISTILL` recursion guard (now removed), the hook would spawn
   Haiku subagents indefinitely. The guard itself was fragile and env-var-dependent.

The main-thread pass avoids all three: it has the full conversation, accurate signal
classification from the LLM that lived through the session, and no subprocess recursion.

## Migration timeline

- **2026-05-05 (this release):** Haiku distillation deprecated. `distill-session.sh`
  replaced with a stub that exits 0 and prints a deprecation message to stderr.
  The `TARGET_INSIDE_DISTILL` recursion guard is removed from the stop hook.
- **Next release:** `distill-session.sh` will be deleted entirely.

The `convo-signals.jsonl` file is unchanged. Mempalace and any manual-triage
consumers that read it are not affected by this change.

## For consumers of distill-session.sh

There is nothing to call directly. The memory pass is automatic:

- If you are running `/target`, the pre-promise pass runs in the skill body.
- If `pr-merge.sh` is in your pipeline, the post-merge pass runs via the sentinel.

If you have an external script that called `distill-session.sh`, it will now get
a deprecation message on stderr and exit 0. Remove the call; there is no
replacement entrypoint to invoke manually.

## Writer contract

Both passes write entries via `scripts/memory/write-memory-entry.sh`. The interface:

```bash
bash scripts/memory/write-memory-entry.sh \
    --memory-dir  /path/to/memory/dir  \
    --session-id  20260505T102342Z-...  \
    --candidate   '{"type":"feedback","name":"...","description":"...","body":"..."}'
```

Exit codes:

| Code | Meaning |
|---|---|
| 0 | Entry written successfully |
| 1 | Error (bad args, write failure) |
| 2 | Deduped (entry with same `name` already exists and body matches) |

The `type` field must be `feedback` or `project`. The `name` field is a
`snake_case_identifier` used as the filename and dedup key. Both passes respect
the same dedup semantics: if `name` already exists with matching body, the entry
is skipped (exit 2). If `name` exists with a different body, a `## Session {sid}
update` stanza is appended rather than overwriting the original.
