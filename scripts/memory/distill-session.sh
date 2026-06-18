#!/usr/bin/env bash
# distill-session.sh - DEPRECATED 2026-05-05.
#
# The Haiku-based session distillation has been replaced with a main-thread
# memory pass at two checkpoints:
#   1. Pre-promise (target skill body, before <promise> emission)
#   2. Post-merge (post-merge-pass.sh triggered by pr-merge.sh sentinel)
#
# Both passes call scripts/memory/write-memory-entry.sh - the same writer this
# script used to invoke - so the file format and dedup semantics are unchanged.
#
# This stub stays for one release so any external caller (cron job, test, etc.)
# surfaces with a clear message before the file is removed. After the next
# release, this file will be deleted entirely.
set -uo pipefail
echo "distill-session.sh: DEPRECATED. Use the main-thread memory pass instead." >&2
echo "  Pre-promise: built into skills/target/references/pre-promise.md" >&2
echo "  Post-merge: scripts/memory/post-merge-pass.sh" >&2
echo "  See docs/architecture/memory-system.md for the migration." >&2
exit 0
