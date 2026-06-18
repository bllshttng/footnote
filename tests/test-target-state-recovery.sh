#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

run_recovery_case() {
  local case_name="$1"
  local fixture_content="$2"
  local case_dir="$TMP_DIR/$case_name"

  mkdir -p "$case_dir/.fno"
  printf '%s\n' "$fixture_content" > "$case_dir/.fno/target-state.md"

  (
    cd "$case_dir"
    CODEX_PLUGIN_ROOT="$case_dir" TARGET_START=1 bash "$ROOT_DIR/hooks/helpers/init-target-state.sh" >/dev/null
  )

  if ! ls "$case_dir/.fno"/target-state.corrupt.*.md >/dev/null 2>&1; then
    echo "Expected corrupted state archive to be created for $case_name" >&2
    exit 1
  fi

  grep -q '^---$' "$case_dir/.fno/target-state.md"
  grep -q '^status: IN_PROGRESS' "$case_dir/.fno/target-state.md"
  grep -q '^provider: codex' "$case_dir/.fno/target-state.md"
  grep -q '^provider_mode:' "$case_dir/.fno/target-state.md"
  grep -q '^session_start_context_loaded: false' "$case_dir/.fno/target-state.md"
}

run_recovery_case "plain-malformed" $'status: IN_PROGRESS\ncurrent_phase: do'
run_recovery_case "partial-frontmatter" $'---\nstatus: IN_PROGRESS\ncurrent_phase: do'

GEMINI_CASE_DIR="$TMP_DIR/gemini-upgrade"
mkdir -p "$GEMINI_CASE_DIR/.fno" "$GEMINI_CASE_DIR/.gemini/agents"
cat > "$GEMINI_CASE_DIR/.fno/settings.yaml" <<'EOF'
config:
  gemini_experimental_agents: true
EOF

for agent in archer.md reviewer.md roadmap-generator.md verifier.md; do
  printf -- '---\nname: sample\ndescription: sample\n---\nbody\n' > "$GEMINI_CASE_DIR/.gemini/agents/$agent"
done

(
  cd "$GEMINI_CASE_DIR"
  GEMINI_PROJECT_DIR="$GEMINI_CASE_DIR" TARGET_START=1 bash "$ROOT_DIR/hooks/helpers/init-target-state.sh" >/dev/null
)

grep -q '^provider: gemini' "$GEMINI_CASE_DIR/.fno/target-state.md"
grep -q '^provider_mode: experimental_agents' "$GEMINI_CASE_DIR/.fno/target-state.md"

echo "Target state recovery validation passed"
