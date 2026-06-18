# Memory auto-distillation

## What it does

When a target session ends with `status: COMPLETE`, the stop hook fires
`scripts/memory/distill-session.sh`. The script reads the session's
`.fno/convo-signals.jsonl` and the Claude transcript, calls Haiku
through `claude -p`, and writes deduplicated entries to the project's
memory dir at `~/.claude/projects/{project}/memory/`.

The goal is passive memory capture. Today, entries land only when the
user asks ("save this as a memory") or when the LLM spots a clear save
moment. Most corrections, confirmations, and project-state changes fall
through. The distillation step is the autonomous capture path.

## Trigger surface

| Signal | Result |
|--------|--------|
| `status: COMPLETE` | Distillation runs after `run_completion_accounting` |
| `status: BLOCKED` | NOT triggered (BLOCKED sessions are noisy by design) |
| Other / no state | NOT triggered |
| `enabled: false` in settings | Script is invoked but exits early |
| Missing `claude` on PATH | Script logs and exits 0 |
| Missing convo-signals | Script logs "no signals to distill" and exits 0 |

The COMPLETE-only restriction is a locked decision in plan
`2026-05-04-auto-distill-session-memory/00-INDEX.md`. BLOCKED sessions
typically reflect failures whose corrections would pollute memory.

## Data flow

```
target session ends
        │
        ▼
hooks/target-stop-hook.sh COMPLETE branch
        │
        ▼ run_completion_accounting + ensure_session_registered
        │
        ▼ if [[ -x scripts/memory/distill-session.sh ]]
        │
        ▼ STATE_DIR + STATE_FILE + SESSION_ID env vars
        │
        ▼
scripts/memory/distill-session.sh
        │
        ├─ _resolve_memory_dir (override → settings → transcript path → owner_cwd slug)
        ├─ _read_setting "enabled" → bail if false
        ├─ _build_distill_prompt (tail 50 convo-signals)
        ├─ claude -p --model haiku --max-turns 1
        │
        ▼ YAML response
        │
        ▼ _split_candidates (Python parser, max 5 valid candidates)
        │
        ▼ for each candidate
        │
        ▼
scripts/memory/write-memory-entry.sh
        │
        ├─ rc 0 (wrote new / appended update)
        ├─ rc 2 (deduped, no-op)
        ├─ rc 1 (real error)
        │
        ▼ atomic write via tmp+rename
        ▼ atomic MEMORY.md update (idempotent grep)
        │
        ▼
~/.claude/projects/{project}/memory/{type}_{slug}.md
~/.claude/projects/{project}/memory/MEMORY.md
```

## Frontmatter shape

Every auto-written entry carries:

```yaml
---
name: snake_case_identifier
description: one short line
type: feedback | project
auto_generated: true
source_session: 20260504T184801Z-97791-05b7ab
created_at: 2026-05-04T18:54:07Z
---
```

The `auto_generated: true` field is the user's revert signal:

```bash
grep -l 'auto_generated: true' ~/.claude/projects/*/memory/*.md
```

surfaces every auto-written entry. To revert, delete the file and remove
the matching line in that project's `MEMORY.md`. `git restore` works if
the memory dir is under version control.

## Dedup-or-update

Match key is the `name` field, not the filename. Three outcomes when a
candidate matches an existing entry's name:

| Body match | Description match | Action | rc |
|------------|-------------------|--------|-----|
| same | same | log "deduped", no-op | 2 |
| different | same or different | append `## Session {sid} update` stanza, preserve original body | 0 |

Filenames may collide via slug collisions; `name` is the load-bearing
identity. The original body is never overwritten.

## Bounds and budget

Spend is bounded structurally:

- `claude -p --max-turns 1` caps Haiku to a single turn.
- `_build_distill_prompt` tails 50 convo-signal entries. The prompt does
  not grow unboundedly with session length.
- The Python parser caps valid candidates at 5 even if Haiku emits more.
- `config.executors.distill.max_cost_usd` is read and logged. **It is
  advisory only** — the value is not yet enforced as a runtime cap.
  Wiring a real per-call cost cap is a follow-up.

## Failure modes

The COMPLETE-branch invocation is **non-blocking by contract**: a
distillation failure must never regress a clean COMPLETE.

| Failure | Result |
|---------|--------|
| `distill-session.sh` exits non-zero | Logged via `\|\| log` in hook; `.target-completed` still touched; `emit_approve` still runs |
| `python3` missing | distill exits 0 with log line |
| `claude` CLI missing | distill exits 0 with log line |
| Haiku returns malformed YAML | parser logs `parsed=N valid=0` to stderr; loop runs 0 iterations |
| Memory dir unwritable | script exits 0 with log line; no partial state on disk |
| Two sessions race on MEMORY.md | last writer wins (known limitation; race window is the grep-then-append in `write-memory-entry.sh`) |

Failures are recorded in `.fno/distill.log` (the parent hook
redirects stderr there).

## Why this design

Three guardrails matter:

1. **Trust boundary stays explicit.** Every auto-write is tagged
   `auto_generated: true`. The user audits via grep + git, not by
   manually inspecting each file.
2. **Promotion is passive.** Entries land directly in the live memory
   dir; there is no `_pending` queue or acceptance prompt. The user
   explicitly rejected that friction during planning (locked decision
   #6 in 00-INDEX.md).
3. **Surface is narrow.** Only `feedback` and `project` types
   auto-write. `user` (identity facts) and `reference` (external system
   pointers) stay user-explicit.

## Files

| File | Role |
|------|------|
| `scripts/memory/distill-session.sh` | Main orchestrator: resolve memory dir, build prompt, call Haiku, parse YAML, dispatch per candidate |
| `scripts/memory/write-memory-entry.sh` | Per-candidate writer: dedup check, atomic write, atomic MEMORY.md update |
| `hooks/target-stop-hook.sh` | COMPLETE-branch invocation (non-blocking) |
| `.fno/settings.yaml.example` | `config.executors.distill.{max_cost_usd, enabled, memory_dir_override}` |
| `tests/memory/test_distill_session.sh` | End-to-end with stubbed `claude -p` |
| `tests/memory/test_dedup.sh` | New / no-op / update three-path coverage |
| `tests/memory/test_completion_only.sh` | Static-analysis: distill is in COMPLETE, not BLOCKED |
| `tests/memory/test_distill_failure_nonblocking.sh` | Runtime: hook still exits 0 when distill exits 1 |
| `tests/memory/test_parser_fixtures.sh` | Six fixtures for the YAML candidate parser |
