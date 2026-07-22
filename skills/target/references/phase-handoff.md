# Phase Handoff Artifacts (best-effort)

Read this only for a **multi-phase run** (think -> plan -> do -> ... -> external). A **flat single-file plan skips `ph_write` entirely** (G4: "scaffolding I skipped and nothing missed them") - the exemption lives in SKILL.md, this file is the schema for when you do write them.

Best-effort, not a gate: `loop-check` never reads these, so a missing artifact
never blocks completion. They are a convenience - each phase writes a small
structured artifact at the end of its work and reads the prior phase's at the
start, so a pipeline transition has a clean handoff without the next phase
reconstructing context from the full session transcript. Write them when the run
spans multiple phases; skip them for a short single-phase change.

**Source the helper at the start of each phase:**

```bash
source "${CLAUDE_PLUGIN_ROOT:-$(git rev-parse --show-toplevel)}/scripts/lib/phase-handoff.sh"
```

**Prior-phase read (at phase start, after init):**

```bash
# Each phase reads the immediately preceding phase's artifact.
# If no prior artifact exists, proceed with reduced context - do NOT block.
PRIOR=$(ph_read <prior-phase> "$SESSION_ID" 2>/dev/null || echo "")
if [[ -n "$PRIOR" ]]; then
  echo "handoff loaded from <prior-phase>: $(echo "$PRIOR" | head -3)" >&2
else
  echo "no prior handoff from <prior-phase> - proceeding with reduced context" >&2
fi
```

Prior-phase mapping (fixed by pipeline order):

| Current phase | Reads artifact from |
|---------------|---------------------|
| plan | think |
| do | plan |
| clean | do |
| review | clean |
| validate | review |
| docs | validate |
| ship | docs |
| external | ship |

The `think` phase has no prior and skips the read.

**Artifact write (at phase end, before yielding to next phase):**

```bash
# think phase example
ph_write think "$SESSION_ID" "$(cat <<EOF
design_docs_produced: [${THINK_DOCS:-}]
key_decisions:
  - "${KEY_DECISION_1:-}"
open_questions: [${OPEN_QUESTIONS:-}]
EOF
)"

# plan phase example
ph_write plan "$SESSION_ID" "$(cat <<EOF
plan_path: ${PLAN_PATH:-}
phases_planned: ${PHASES_PLANNED:-}
scope_classification: ${SCOPE:-feature}
EOF
)"

# do phase example
ph_write do "$SESSION_ID" "$(cat <<EOF
stories_completed: [${DONE_IDS:-}]
files_changed: $(git diff --name-only HEAD 2>/dev/null | jq -R . | jq -s . 2>/dev/null || echo "[]")
notes_for_next_phase: |
  ${NOTES_FOR_NEXT:-}
EOF
)"

# clean phase example
ph_write clean "$SESSION_ID" "$(cat <<EOF
files_simplified: [${SIMPLIFIED:-}]
patterns_removed: [${PATTERNS:-}]
notes_for_review: |
  ${CLEAN_NOTES:-}
EOF
)"

# review phase - extends the existing gate artifact; write handoff with same session
ph_write review "$SESSION_ID" "$(cat <<EOF
sigma_review_artifact_path: .fno/artifacts/review-${SESSION_ID}.md
blocking_issues: [${BLOCKING:-}]
advisory_notes: [${ADVISORY:-}]
EOF
)" 2>/dev/null || true  # gate artifact already written by sigma-review; handoff is supplemental

# validate phase example
ph_write validate "$SESSION_ID" "$(cat <<EOF
build_command: ${BUILD_CMD:-}
test_command: ${TEST_CMD:-}
output_summary: "${VALIDATE_SUMMARY:-}"
exit_codes:
  build: ${BUILD_EXIT:-0}
  test: ${TEST_EXIT:-0}
EOF
)"

# ship phase example
ph_write ship "$SESSION_ID" "$(cat <<EOF
pr_number: ${PR_NUMBER:-}
pr_url: ${PR_URL:-}
branch_name: ${BRANCH:-}
base_branch: ${BASE_BRANCH:-main}
EOF
)"

# external phase example
ph_write external "$SESSION_ID" "$(cat <<EOF
review_status: ${EXT_STATUS:-}
blocking_comments: [${BLOCKING_COMMENTS:-}]
approval_state: ${APPROVAL_STATE:-}
EOF
)"

# docs phase example
ph_write docs "$SESSION_ID" "$(cat <<EOF
docs_updated: [${DOCS_UPDATED:-}]
sections_added: [${SECTIONS_ADDED:-}]
EOF
)"
```

Artifacts are written to `.fno/artifacts/handoff/{phase}-{session_id}.md`.
The `handoff/` subdirectory namespaces away from gate-attestation artifacts owned
by /review, /pr create, /pr check, etc.

**Concurrency safety:** two target runs in different worktrees use different
`session_id` values so artifact files never collide even when they share the
same project directory.

See [phase-artifacts.md](phase-artifacts.md) for the full
per-phase schema and the complete size invariant (500-token cap).
