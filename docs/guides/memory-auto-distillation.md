# Auto-distilled memory: how-to

> **Historical.** This Haiku-distillation pipeline was deprecated (see
> `docs/architecture/memory-system.md`) and its `convo-signals.jsonl`
> input has since been removed entirely. Memory is now written by a
> two-checkpoint main-thread pass instead; kept for context.

When a `/target` session ends with `<promise>MISSION COMPLETE: ...</promise>`,
footnote now auto-distills the session into memory entries without you
asking.

## What happens automatically

After every successful (`COMPLETE`) target session:

1. The stop hook calls `scripts/memory/distill-session.sh`.
2. The script reads the session's signals + transcript and asks Haiku
   to propose memory candidates.
3. Up to 5 candidates land in `~/.claude/projects/{project}/memory/`,
   each tagged `auto_generated: true`.
4. The project's `MEMORY.md` index gets the new entries appended.

This runs only on `COMPLETE`. `BLOCKED` sessions and other failure modes
are not auto-distilled.

## How to find auto-written entries

```bash
grep -l 'auto_generated: true' ~/.claude/projects/*/memory/*.md
```

Lists every entry the auto-distiller wrote, across all your projects.

To see what's in one:

```bash
cat ~/.claude/projects/-Users-yourname-yourrepo/memory/feedback_*.md
```

## How to revert a single entry

```bash
# 1. Remove the file
rm ~/.claude/projects/-Users-yourname-yourrepo/memory/feedback_unwanted.md

# 2. Remove the matching line in MEMORY.md
sed -i.bak '/feedback_unwanted/d' \
    ~/.claude/projects/-Users-yourname-yourrepo/memory/MEMORY.md
```

Or, if your memory dir is under version control:

```bash
git -C ~/.claude/projects/-Users-yourname-yourrepo restore memory/
```

## How to disable auto-distillation

Edit `.fno/config.toml` (project-local) or
`~/.fno/config.toml` (global):

```yaml
config:
  executors:
    distill:
      enabled: false
```

The COMPLETE branch will still invoke the script, but the script
short-circuits before the Haiku call.

## How to override the memory dir (for testing)

Two options:

```yaml
# .fno/config.toml
config:
  executors:
    distill:
      memory_dir_override: /tmp/test-memory
```

Or set the env var when running tests:

```bash
MEMORY_DIR_OVERRIDE=/tmp/test-memory bash tests/memory/test_distill_session.sh
```

## How to triage a session that should have produced entries but didn't

Check `.fno/distill.log` in the session's repo:

```bash
tail -20 .fno/distill.log
```

Look for lines like:

| Line | Meaning |
|------|---------|
| `disabled via settings - skipping` | `enabled: false` is set |
| `no signals to distill; skipping` | `convo-signals.jsonl` was empty or unreadable |
| `claude CLI not on PATH; skipping` | `claude` binary missing from the hook's PATH |
| `claude invocation failed (rc=N)` | Auth, quota, or model error - check the stderr lines that follow |
| `_split_candidates: parsed=0 valid=0` | Haiku returned unparseable YAML; check the raw response |
| `summary: wrote=0 deduped=N failed=0` | Candidates were proposed but all matched existing entries |
| `memory distill: no session_id in state file` | State file lost its `session_id` field; investigate the upstream session |

The hook log itself (`.fno/target-stop-hook.log`) records that the
distill block ran:

```bash
grep -i distill .fno/target-stop-hook.log
```

## Cost

Per-session cost is bounded by:

- `claude -p --max-turns 1` (single turn only)
- The prompt is capped at the last 50 convo-signal entries

The `config.executors.distill.max_cost_usd` setting is **advisory** -
read and logged, but not yet enforced as a runtime cap. Realistic
per-session cost on Haiku is well under 1 cent.

## What does NOT get auto-distilled

- `user` type memories (identity facts) - stay user-explicit.
- `reference` type memories (external system pointers) - stay
  user-explicit.
- BLOCKED-session corrections - too noisy.
- Cross-project pattern detection - each project's memory dir is
  isolated.
