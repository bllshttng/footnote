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

The `convo-signals.jsonl` capture itself has since been removed entirely
(it had zero readers anywhere in the codebase). This section is retained
for historical context on the Haiku distillation deprecation only.

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
    --memory-dir     /path/to/memory/dir  \
    --session-id     20260505T102342Z-...  \
    --candidate      '{"type":"feedback","name":"...","description":"...","body":"..."}' \
    --source-sha256  <sha256 of the existing target file, if any>
```

Exit codes:

| Code | Meaning |
|---|---|
| 0 | Entry written successfully |
| 1 | Error (bad args, write failure) |
| 2 | Deduped (entry with same `name` already exists and body matches) |
| 3 | Staged (see [Guards against bad autonomous writes](#guards-against-bad-autonomous-writes) below) |

The `type` field must be one of `feedback`, `project`, `reference`, `user`. The `name` field is a `snake_case_identifier` used as the filename and dedup key. Both passes respect the same dedup semantics. If `name` already exists with a matching body, the entry is skipped (exit 2). If `name` exists with a different body, the writer appends a `## Session {sid} update` stanza instead of overwriting the original, subject to the guards below.

## Guards against bad autonomous writes

Two problems compound here. The trigger for what gets evaluated was biased toward failure. And there was no blocklist for lesson classes that age badly. Both are fixed below, plus three smaller guards borrowed from the same review that flagged the bias. A comparable open-source agent memory system, "Hermes", bans the same lesson classes for the same stated reason. Their rule: these harden into refusals the agent cites against itself for months after the actual problem was fixed.

**Blocklist.** `skills/target/references/pre-promise.md` ("Blocklist" under the Memory Pass section) is the canonical list of four lesson classes never to write. The classes: env-dependent findings stated as durable fact, negative claims about a tool, transient/one-off errors, and an unresolved failure written up as validated workflow. The live specimen that motivated this: a memory entry once claimed `timeout`/`gtimeout` were absent from a machine, causing watchers to no-op at exit 127. Both binaries were installed. The false claim survived until someone checked it against the running system. `cli/src/fno/retro/classify.py` carries a small deterministic backstop (`_PM_BLOCKLIST_RE` in `classify_postmortem`) that archives two of the four classes on sight: negative tool claims and transient error signatures. It fires only on a postmortem that is not also a genuine wedge. The other two classes stay judgment calls the prompt-text blocklist governs.

**Read-before-write.** `write-memory-entry.sh` refuses to mutate an existing memory file unless the caller passes `--source-sha256` matching that file's CURRENT on-disk content. This proves the write is grounded in a read this turn, not a hallucinated memory of what the file says. New-file writes are unaffected, since there is nothing to hallucinate about. A dedup hit is unaffected too, since no mutation happens.

**Provenance tag.** Every autonomously-written entry already carried `auto_generated: true` in its frontmatter. The writer now enforces the other half: it refuses an update to an existing entry that is NOT `auto_generated: true`. That means a human wrote or edited it directly. This writer has no caller that is a human editing their own memory by hand. Every call is an autonomous pass, pre-promise or post-merge, so the refusal is unconditional, never gated on an attended/unattended flag. The same defect shows up elsewhere. `groom`'s stale-ideas leg re-deferred a backlog node hours after a human undeferred it. It reads `created_at` age with no notion that a human had just touched it. A provenance check on the write path is the general shape of that fix. The specific `maintain` fix is tracked separately.

**Stage as a third gate outcome.** Neither guard above hard-blocks. Both refuse the LIVE write and instead write the proposed content to `{memory-dir}/.staged/{filename}.md` (exit 3), for a human to review and apply by hand. `allow` (exit 0/2) and `block` (exit 1, a real error) were the only two outcomes before. `stage` is the third, for a write attempted with no human in the loop to confirm it on the spot.

## Completion eval (a separate autonomous writer)

`crates/fno-agents/src/finalize.rs::write_postmortem` is a second, older autonomous writer, unrelated to `write-memory-entry.sh` above. It writes one structured artifact per `fno-agents finalize` call to `~/.fno/postmortems/`. `cli/src/fno/retro/harvest.py` and `classify.py` consume that artifact into a backlog node or an inbox item: the autocorrect monthly review's corpus.

It used to fire only for a stuck terminal (`NoProgress`/`Budget`/`Interrupted`/`Aborted`, then named `POSTMORTEM_REASONS`), a failure-only sample. A failure-only sample writes rules in a predictable direction. Nothing ever confirmed what a clean session did right, so every lesson skewed toward caution nobody asked for. `finalize.rs::eval_should_fire` now fires this for every terminal reason except `NoWork` (nothing happened, nothing to evaluate). That covers every ship reason and every stuck reason alike. The body still branches on `STUCK_REASONS` (renamed from `POSTMORTEM_REASONS`). A stuck session gets the original failure-triage prose. Every other reason gets a lighter completion-eval prose pointing at the blocklist above.
