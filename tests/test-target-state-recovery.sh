#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

# Every ambient input to detect_provider has to go, or a case inherits its
# verdict from the shell instead of from its fixture. Each case sets the one
# hint it needs inside its own subshell, so nothing cleared here is re-exported.
#
# Precedence is why the whole set matters and not just the session markers:
# detect_provider checks session markers, THEN CODEX_PLUGIN_ROOT, THEN
# GEMINI_PROJECT_DIR, so an ambient CLAUDE_CODE_SESSION_ID makes every case
# detect "claude" and an ambient CODEX_PLUGIN_ROOT makes a gemini case detect
# "codex".
unset CLAUDE_CODE_SESSION_ID CODEX_THREAD_ID CODEX_SESSION_ID GEMINI_SESSION_ID \
      CODEX_PLUGIN_ROOT GEMINI_PROJECT_DIR CLAUDE_PLUGIN_ROOT

# Isolate HOME so init never touches the developer's real ~/.fno, and so no
# global config can reach into a case's verdict.
export HOME="$TMP_DIR/fake-home"
mkdir -p "$HOME/.fno"

# Recovery is the subject here, not process-tree identity proof.  Smoke puts a
# real fno on PATH, whose resolver correctly rejects plugin-root hints as proof
# of session ownership; a bare run historically had no fno and fell back to
# those hints.  Pin the resolver boundary so both environments exercise the
# same recovery path and each case declares the harness it expects.
FAKE_BIN="$TMP_DIR/fake-bin"
mkdir -p "$FAKE_BIN"
cat > "$FAKE_BIN/fno" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-} ${2:-}" == "target resolve-owned-identity" ]]; then
  printf 'HARNESS=%s\nSESSION_ID=fixture-session\nDISPOSITION=proven\nCOLLISION=\n' \
    "${FNO_TEST_HARNESS:-}"
  exit 0
fi
exit 1
EOF
chmod +x "$FAKE_BIN/fno"

run_recovery_case() {
  local case_name="$1"
  local fixture_content="$2"
  local case_dir="$TMP_DIR/$case_name"

  mkdir -p "$case_dir/.fno"
  printf '%s\n' "$fixture_content" > "$case_dir/.fno/target-state.md"

  (
    cd "$case_dir"
    PATH="$FAKE_BIN:$PATH" FNO_TEST_HARNESS=codex FNO_TARGET_INIT_GATED=1 \
      CODEX_PLUGIN_ROOT="$case_dir" TARGET_START=1 \
      bash "$ROOT_DIR/hooks/helpers/init-target-state.sh" >/dev/null
  )

  if ! ls "$case_dir/.fno"/target-state.corrupt.*.md >/dev/null 2>&1; then
    echo "Expected corrupted state archive to be created for $case_name" >&2
    exit 1
  fi

  grep -q '^---$' "$case_dir/.fno/target-state.md"
  # session_id, not status: the control-plane collapse removed `status`,
  # `current_phase`, and `session_start_context_loaded` from the manifest.
  # A real session_id is what proves init wrote a live manifest, not a stub.
  grep -q '^session_id: ' "$case_dir/.fno/target-state.md"
  grep -q '^harness: codex' "$case_dir/.fno/target-state.md"
  grep -q '^provider: codex' "$case_dir/.fno/target-state.md"
  grep -q '^provider_mode:' "$case_dir/.fno/target-state.md"
}

run_recovery_case "plain-malformed" $'status: IN_PROGRESS\ncurrent_phase: do'
run_recovery_case "partial-frontmatter" $'---\nstatus: IN_PROGRESS\ncurrent_phase: do'

# detect_provider's GEMINI_PROJECT_DIR branch, on a dir with no prior manifest.
# harness_mode/provider_mode are constants now that the experimental
# project-agent mode is retired, so `standard` here is asserting the field is
# still emitted, not that a mode was resolved.
GEMINI_CASE_DIR="$TMP_DIR/gemini-detect"
mkdir -p "$GEMINI_CASE_DIR/.fno"

(
  cd "$GEMINI_CASE_DIR"
  PATH="$FAKE_BIN:$PATH" FNO_TEST_HARNESS=gemini FNO_TARGET_INIT_GATED=1 \
    GEMINI_PROJECT_DIR="$GEMINI_CASE_DIR" TARGET_START=1 \
    bash "$ROOT_DIR/hooks/helpers/init-target-state.sh" >/dev/null
)

grep -q '^harness: gemini' "$GEMINI_CASE_DIR/.fno/target-state.md"
grep -q '^provider: gemini' "$GEMINI_CASE_DIR/.fno/target-state.md"
grep -q '^harness_mode: standard' "$GEMINI_CASE_DIR/.fno/target-state.md"
grep -q '^provider_mode: standard' "$GEMINI_CASE_DIR/.fno/target-state.md"

echo "Target state recovery validation passed"
