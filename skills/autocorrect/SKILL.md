---
name: autocorrect
description: Self-improvement loop for the toolkit. Captures corrections passively (git post-commit, pre-commit verifiers, /insights tags) into ~/.fno/corrections.log, then runs a monthly review via a fresh Claude API call against the current rule text and surfaces patches the user triages in 20 minutes. Use when the user asks to review corrections, run an autocorrect review, triage proposed patches, install the autocorrect schedule, ingest /insights, check autocorrect status, or audit recurring mistake classes in the toolkit.
---

# Autocorrect

The meta-improvement layer. Replaces the deprecated feels system with a smaller, sharper mechanism: passive capture, fresh-API review, human triage in 20 minutes per month.

## Three writers, one log, one consumer

Capture surfaces (write to `~/.fno/corrections.log`, never invoked by the agent):

1. **git post-commit hook on `~/.claude/`** records every edit to rule/skill/CLAUDE.md files. Severity S1 by default; S0 if the commit subject starts with `urgent:` or `revert:`.
2. **pre-commit verifier wrapper** (`scripts/corrections-verifier-log.sh`) any verifier in any repo calls when it blocks a commit. Verifier decides the severity (S0 for secret-scanner, S1 for style/lint, S2 for drift).
3. **`/insights` tag ingester** (`scripts/corrections-insights-tag.sh`) ports `#agent-correction`-tagged `/insights` entries as S2 events.

Consumer surface (this skill):

- **Monthly review** (1st of each month, 09:00 local) sends the last 30 days of S1+S2 events plus the current full text of every implicated rule file to a fresh Claude API call. Output is a numbered patch list.
- **S0 watcher** (every 15 minutes) catches any unprocessed S0 events and fires an immediate review.
- **Triage** walks the patch list interactively; accept/reject/defer/skip/quit per item.

## Commands

| Command | What it does |
|---|---|
| `/autocorrect review [--severity S0\|S1\|S2]` | Build a packet and call the reviewer now. Severity filter optional. |
| `/autocorrect triage [--review-id <id>]` | Walk the latest (or specified) patch list interactively. |
| `/autocorrect status` | Show scheduled jobs, latest review, pending patches, watermark state. |
| `/autocorrect install` | Register the monthly cron and the S0 watcher (idempotent). |
| `/autocorrect ingest-insights` | Manually run the `/insights` ingestion path. |

All commands are thin wrappers over scripts in this plugin's `scripts/` directory.

## Invariants (read before extending)

1. **The agent is never the capture surface.** All three writers are passive (git hooks, verifier wrappers, scheduled ingester). If you find yourself proposing an agent-direct write, you're holding the wrong end of the loop.
2. **Single artifact.** Everything funnels through `~/.fno/corrections.log`. Three writers, one consumer. Resist the urge to add ad-hoc per-source logs.
3. **Severity tiers, not frequency thresholds.** S0 fires immediately on a single event; S1 aggregates monthly; S2 rolls up quarterly. The writer decides severity.
4. **Reviewer sees current full rule text, not just diffs.** Decisions are made against the rule as it stands today, not its history.
5. **L1 -> L2 migration discipline.** Rules are a holding pen; verifiers are the destination. CONVERT-TO-VERIFIER patches MUST delete the rule text in the same commit they add the verifier. `autocorrect-triage.sh` surfaces this invariant.
6. **The loop's success metric is patch volume decreasing.** Patch volume dropping is the goal, not a failure signal. The loop retires successfully when verifiers absorb every recurring correction class.

## File map

| File | Purpose |
|---|---|
| `~/.fno/corrections.log` | The canonical capture artifact. Mode 0600. |
| `~/.claude/.corrections-watermark` | Last monthly review window end. |
| `~/.claude/.s0-watcher-watermark` | Last S0 watcher tick. |
| `~/.claude/.s0-processed.log` | Per-event hashes of S0 events already dispatched. |
| `~/.claude/.insights-watermark` | Per-event hashes ingested from /insights. |
| `~/.fno/corrections-rejected.log` | Items the user rejected during triage. |
| `~/.claude/corrections-malformed.log` | Items where the patch did not apply cleanly. |
| `~/.claude/proposed-patches/{review_id}.md` | The reviewer's patch list per review. |

## Setup

```bash
# Upgrading from before ab-f063 Wave 2? corrections.log moved from ~/.claude/
# to ~/.fno/ (placement rule). Run this once first - it appends any existing
# ~/.claude/corrections.log content to the new location and tombstones the
# old file. No-op if there's nothing to migrate.
bash $CLAUDE_PLUGIN_ROOT/scripts/corrections-migrate-to-fno.sh

# One-time bootstrap (creates corrections.log, installs git post-commit hook,
# registers the launchd jobs)
bash $CLAUDE_PLUGIN_ROOT/scripts/corrections-log-init.sh
bash $CLAUDE_PLUGIN_ROOT/scripts/install-corrections-git-hook.sh
bash $CLAUDE_PLUGIN_ROOT/scripts/install-autocorrect-cron.sh

# Verify
bash $CLAUDE_PLUGIN_ROOT/scripts/install-autocorrect-cron.sh --status

# Tail the log
tail -f ~/.fno/corrections.log
```

After setup, the loop runs unattended. The user only interacts during triage (~20 min/month) or to acknowledge an S0 review.

## References

- `references/corrections-log-format.md` - canonical format for the log
- `references/wiring-existing-verifiers.md` - how to add a new verifier writer
- `references/autocorrect-prompts.md` - canonical reviewer prompt template

## Retirement

The loop self-retires when, for two consecutive years, no S0 event has been caught only by this loop (i.e., every S0 was also caught by a deterministic verifier). At that point the verifier layer has fully absorbed the work and the meta-loop is no longer load-bearing. Delete the schedule, leave the capture infrastructure running as a paper trail, and move on.
