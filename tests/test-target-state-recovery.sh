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
if [[ "${1:-} ${2:-} ${3:-}" == "do target resolve-owned-identity" ]]; then
  printf 'HARNESS=%s\nSESSION_ID=fixture-session\nDISPOSITION=proven\nCOLLISION=\n' \
    "${FNO_TEST_HARNESS:-}"
  exit 0
fi
# in_review adopt guard: one live node whose PR #4242 heads
# feature/adopt-holds. The binding verdict is native; this fake applies its
# adoption proof (OPEN state, head branch == this worktree's branch) so the
# cases flip the outcome with git and FNO_TEST_PR_STATE only.
if [[ "${1:-} ${2:-}" == "backlog get" ]]; then
  case " $* " in
    *" --field _archived"*) printf 'null\n'; exit 0 ;;
    *" --field id"*)
      node_id="${3:-unknown}"
      [[ "$node_id" == "--strict" ]] && node_id="${4:-unknown}"
      printf '%s\n' "$node_id"
      exit 0
      ;;
  esac
  exit 1
fi
if [[ "${1:-} ${2:-}" == "backlog target-binding" ]]; then
  branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null)"
  if [[ "${FNO_TEST_PR_STATE:-OPEN}" == "OPEN" && "$branch" == "feature/adopt-holds" ]]; then
    echo "target binding: ADOPTED: re-binding this session to node x-b424242 on the open PR #4242 (branch $branch is that PR's head)." >&2
    printf 'verdict=adopt\npr=4242\n'
    exit 0
  fi
  echo "target binding: REFUSED: node x-b424242 is in_review (PR #4242)." >&2
  printf 'verdict=refused\nnext=fno do target start "x-b424242 <follow-up scope, one sentence>"\n'
  exit 1
fi
exit 1
EOF
chmod +x "$FAKE_BIN/fno"

# State-path resolver: init must plant and recover its manifest at the same
# path in every environment, so the stub pins the space regime here instead
# of pinning fno-agents absent. The no-binary fallback leg stays pinned by
# the dedicated PATH=/usr/bin:/bin harnesses.
cp "$ROOT_DIR/tests/helpers/fno-agents-state-path-stub.sh" "$FAKE_BIN/fno-agents"
chmod +x "$FAKE_BIN/fno-agents"

init_fixture_repo() {
  local root="$1"
  git -C "$root" init -q
  git -C "$root" config user.email fno@test
  git -C "$root" config user.name fno
  git -C "$root" commit -q --allow-empty -m init
  git -C "$root" checkout -q -b feature/target-state-recovery
}

run_recovery_case() {
  local case_name="$1"
  local fixture_content="$2"
  local case_dir="$TMP_DIR/$case_name"

  mkdir -p "$case_dir/space"
  init_fixture_repo "$case_dir"
  printf '%s\n' "$fixture_content" > "$case_dir/space/target-state.md"

  (
    cd "$case_dir"
    PATH="$FAKE_BIN:$PATH" FNO_TEST_HARNESS=codex FNO_TARGET_INIT_GATED=1 \
      FNO_TEST_SPACE="$case_dir/space" \
      CODEX_PLUGIN_ROOT="$case_dir" TARGET_START=1 \
      bash "$ROOT_DIR/hooks/helpers/init-target-state.sh" >/dev/null
  )

  if ! ls "$case_dir/space"/target-state.corrupt.*.md >/dev/null 2>&1; then
    echo "Expected corrupted state archive to be created for $case_name" >&2
    exit 1
  fi

  grep -q '^---$' "$case_dir/space/target-state.md"
  # session_id, not status: the control-plane collapse removed `status`,
  # `current_phase`, and `session_start_context_loaded` from the manifest.
  # A real session_id is what proves init wrote a live manifest, not a stub.
  grep -q '^session_id: ' "$case_dir/space/target-state.md"
  grep -q '^harness: codex' "$case_dir/space/target-state.md"
  grep -q '^provider: codex' "$case_dir/space/target-state.md"
  grep -q '^provider_mode:' "$case_dir/space/target-state.md"
}

run_recovery_case "plain-malformed" $'status: IN_PROGRESS\ncurrent_phase: do'
run_recovery_case "partial-frontmatter" $'---\nstatus: IN_PROGRESS\ncurrent_phase: do'

# detect_provider's GEMINI_PROJECT_DIR branch, on a dir with no prior manifest.
# harness_mode/provider_mode are constants now that the experimental
# project-agent mode is retired, so `standard` here is asserting the field is
# still emitted, not that a mode was resolved.
GEMINI_CASE_DIR="$TMP_DIR/gemini-detect"
mkdir -p "$GEMINI_CASE_DIR/space"
init_fixture_repo "$GEMINI_CASE_DIR"

(
  cd "$GEMINI_CASE_DIR"
  PATH="$FAKE_BIN:$PATH" FNO_TEST_HARNESS=gemini FNO_TARGET_INIT_GATED=1 \
    FNO_TEST_SPACE="$GEMINI_CASE_DIR/space" \
    GEMINI_PROJECT_DIR="$GEMINI_CASE_DIR" TARGET_START=1 \
    bash "$ROOT_DIR/hooks/helpers/init-target-state.sh" >/dev/null
)

grep -q '^harness: gemini' "$GEMINI_CASE_DIR/space/target-state.md"
grep -q '^provider: gemini' "$GEMINI_CASE_DIR/space/target-state.md"
grep -q '^harness_mode: standard' "$GEMINI_CASE_DIR/space/target-state.md"
grep -q '^provider_mode: standard' "$GEMINI_CASE_DIR/space/target-state.md"

# ── in_review adopt branch ─────────────────────────────────────────────
# A caller standing in the open PR's own worktree, on the PR's own head
# branch, is the author asking to be re-bound, not a fresh dispatch. Proof
# holds -> init proceeds, stamps target_adopted_pr, prints the ADOPTED
# receipt, and never needs TARGET_ALLOW_IN_REVIEW. Proof fails -> the
# refusal is unchanged and no state file is written.
ADOPT_NODE="x-b424242"
ADOPT_MAIN="$TMP_DIR/adopt-main"
ADOPT_HOLDS_WT="$TMP_DIR/adopt-holds-wt"
git init -q "$ADOPT_MAIN"
git -C "$ADOPT_MAIN" -c user.email=fno@test -c user.name=fno commit -q --allow-empty -m init
git -C "$ADOPT_MAIN" worktree add -q -b feature/adopt-holds "$ADOPT_HOLDS_WT"
mkdir -p "$ADOPT_HOLDS_WT/space"

(
  cd "$ADOPT_HOLDS_WT"
  PATH="$FAKE_BIN:$PATH" FNO_TEST_HARNESS=codex FNO_TARGET_INIT_GATED=1 \
    FNO_TEST_SPACE="$ADOPT_HOLDS_WT/space" \
    TARGET_START=1 TARGET_INPUT="$ADOPT_NODE" \
    bash "$ROOT_DIR/hooks/helpers/init-target-state.sh"
) >"$TMP_DIR/adopt-holds.out" 2>"$TMP_DIR/adopt-holds.err"

grep -qF 'ADOPTED: re-binding this session to node x-b424242 on the open PR #4242' \
  "$TMP_DIR/adopt-holds.err"
grep -q '^target_adopted_pr: 4242' "$ADOPT_HOLDS_WT/space/target-state.md"
grep -q '^session_id: ' "$ADOPT_HOLDS_WT/space/target-state.md"

# Proof fails: same in_review node, but this worktree's branch is not the
# PR's head. Refuses exactly as before the adopt branch existed.
ADOPT_FAILS_WT="$TMP_DIR/adopt-fails-wt"
git -C "$ADOPT_MAIN" worktree add -q -b feature/adopt-other "$ADOPT_FAILS_WT"
mkdir -p "$ADOPT_FAILS_WT/space"

adopt_rc=0
(
  cd "$ADOPT_FAILS_WT"
  PATH="$FAKE_BIN:$PATH" FNO_TEST_HARNESS=codex FNO_TARGET_INIT_GATED=1 \
    FNO_TEST_SPACE="$ADOPT_FAILS_WT/space" \
    TARGET_START=1 TARGET_INPUT="$ADOPT_NODE" \
    bash "$ROOT_DIR/hooks/helpers/init-target-state.sh"
) >"$TMP_DIR/adopt-fails.out" 2>"$TMP_DIR/adopt-fails.err" || adopt_rc=$?

[[ "$adopt_rc" -eq 1 ]]
grep -qF "REFUSED: node $ADOPT_NODE is in_review (PR #4242)" "$TMP_DIR/adopt-fails.err"
[[ ! -f "$ADOPT_FAILS_WT/.fno/target-state.md" && ! -f "$ADOPT_FAILS_WT/space/target-state.md" ]]

# Proof fails: the PR's own worktree and branch, but the PR already merged.
# Same branch is not enough; a merged PR is never adopted.
ADOPT_MERGED_WT="$TMP_DIR/adopt-merged-wt"
git -C "$ADOPT_MAIN" worktree add -q "$ADOPT_MERGED_WT" feature/adopt-holds 2>/dev/null \
  || git -C "$ADOPT_MAIN" worktree add -q --force "$ADOPT_MERGED_WT" feature/adopt-holds
mkdir -p "$ADOPT_MERGED_WT/space"
merged_rc=0
(
  cd "$ADOPT_MERGED_WT"
  PATH="$FAKE_BIN:$PATH" FNO_TEST_HARNESS=codex FNO_TARGET_INIT_GATED=1 \
    FNO_TEST_SPACE="$ADOPT_MERGED_WT/space" FNO_TEST_PR_STATE=MERGED \
    TARGET_START=1 TARGET_INPUT="$ADOPT_NODE" \
    bash "$ROOT_DIR/hooks/helpers/init-target-state.sh"
) >"$TMP_DIR/adopt-merged.out" 2>"$TMP_DIR/adopt-merged.err" || merged_rc=$?

[[ "$merged_rc" -eq 1 ]]
grep -qF "REFUSED: node $ADOPT_NODE is in_review (PR #4242)" "$TMP_DIR/adopt-merged.err"
[[ ! -f "$ADOPT_MERGED_WT/space/target-state.md" ]]

echo "Target state recovery validation passed"
