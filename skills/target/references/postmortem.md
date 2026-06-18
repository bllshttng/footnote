> **SUPERSEDED (2026-06-05, ab-d0337fbc):** the machinery this file describes was deleted by the control-plane collapse wedge. Kept for historical context; see docs/architecture/control-plane-loop.md.

# Postmortem (auto-generated)

Reference for the structured artifact produced on every target BLOCKED transition. The actual generator is `skills/target/scripts/postmortem/generate-postmortem.sh` and the format is fixed by [postmortem-format.md](postmortem-format.md). The stop hook owns invocation; users rarely call it directly.

Turn a BLOCKED target run into a structured, auditable artifact.

## When this fires

Auto-invoked by `hooks/target-stop-hook.sh` in its BLOCKED-acceptance branch. **Every** BLOCKED transition produces a postmortem - no exceptions, no skip flags, no opt-out. The plan's architectural invariants forbid suppression: "target will try to quit if given the chance; it should be very hard to do so."

Specifically the hook calls the generator after:
1. `ensure_session_registered` (cost recorded in ledger)
2. `release_graph_claim` (graph node returned to `ready`)
3. **Postmortem generation** (this reference's owner)
4. Sentinel cleanup + `emit_approve` + exit 0

If postmortem generation itself errors (disk full, permission denied, script missing), the hook logs the failure and continues with the BLOCKED exit. A missing postmortem is better than a stuck target loop, but the failure path is loud (stderr + log file) so it surfaces.

## What it produces

A single file at `~/.fno/postmortems/{YYYY-MM-DD}-{session_id_short}.md` with:

- YAML frontmatter (machine-readable): session_id, blocked_phase, blocked_reason (kind + trip_signal + details), iteration_count, cost_usd, duration_minutes
- `## Phase timeline` table built from `.fno/artifacts/handoff/{phase}-{session_id}.md` files
- `## Last output of failed phase` (last 50 lines of the failing phase's artifact body OR the stop-hook log)
- `## Hypotheses` (deterministic, templated from a lookup table keyed on `blocked_reason.kind`)

A one-line entry is also appended to `~/.claude/corrections.log` (when present, which it will be once the autocorrect feature is in steady state):

```
2026-04-27T15:32:04Z | S1 | target-postmortem | ~/.fno/postmortems/2026-04-27-f31156b3.md | test_failure: 3 tests failing in src/auth/login.test.ts
```

The autocorrect monthly review reads `~/.claude/corrections.log` and follows the link to the postmortem.

## Manual invocation (rare)

The script is idempotent on the same session_id (filename collisions get a `.2`, `.3` suffix), so re-running is safe but produces a fresh file.

```bash
# Generate postmortem for the current target-state.md
bash skills/target/scripts/postmortem/generate-postmortem.sh

# Or point at a specific archived state file
bash skills/target/scripts/postmortem/generate-postmortem.sh \
  --state-file .fno/target-state.prior-ab-XXXXXXXX.md \
  --output-dir ~/.fno/postmortems/
```

Both forms print the generated path to stdout. The script never crashes on partial input - missing fields become the literal string `unknown` so the autocorrect parser can detect them.

## See also

- [postmortem-format.md](postmortem-format.md) - the file format spec the script implements
- `scripts/postmortem/generate-postmortem.sh` - the generator
- `hooks/target-stop-hook.sh` (BLOCKED branch, around line 4464) - the call site
- `skills/autocorrect/` - the self-improvement loop that consumes these postmortems

## Append-only invariant

Postmortem files are append-only by convention. No skill in this project deletes them. The directory `~/.fno/postmortems/` is meant to accumulate; pruning is the user's call. The plan explicitly forbids:

- `--skip-postmortem` flags (none exist; never add one)
- Settings keys that suppress generation (none exist; never add one)
- Hook paths that silently swallow generation errors to let target exit clean

If you find yourself wanting to bypass this, the answer is no.
