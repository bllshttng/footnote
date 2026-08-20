#!/usr/bin/env bash
# Advisory-only worktree peer detection for both SessionStart carriers.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HELPER="$REPO_ROOT/hooks/helpers/worktree-live-peers.sh"
CARRIER="$REPO_ROOT/hooks/worktree-peers-session-start.sh"
CODEX_WRAPPER="$REPO_ROOT/hooks/session-start.sh"
HEARTBEAT="$REPO_ROOT/hooks/claim-heartbeat.sh"
PASS=0
FAIL=0

pass() { PASS=$((PASS + 1)); printf '[worktree-peers] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL + 1)); printf '[worktree-peers] FAIL: %s\n' "$*" >&2; }

if [[ ! -f "$HELPER" || ! -f "$CARRIER" ]]; then
  fail "helper and SessionStart carrier must exist"
  exit 1
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT
PROJECT="$TMP_DIR/project"
git init -q "$PROJECT"
PROJECT_GIT_DIR="$(git -C "$PROJECT" rev-parse --absolute-git-dir)"
LIVE_DIR="$PROJECT_GIT_DIR/fno/live"
mkdir -p "$LIVE_DIR"

run_helper() {
  printf '{"cwd":"%s","session_id":"%s"}' "$PROJECT" "$1" \
    | CODEX_THREAD_ID="${TEST_CODEX_THREAD_ID:-}" bash "$HELPER"
}

run_carrier() {
  printf '{"cwd":"%s","session_id":"%s"}' "$PROJECT" "$1" \
    | CODEX_THREAD_ID="${TEST_CODEX_THREAD_ID:-}" bash "$CARRIER"
}

reset_live() { rm -rf "$LIVE_DIR"; mkdir -p "$LIVE_DIR"; }

# A fno shim on PATH makes carrier recording deterministic and fast (the real
# fno may be absent, old, or slow) and keeps the global journal unpolluted. Each
# shim emulates `fno worktree overlap-record --stdin` and echoes one fixed
# result line, ignoring its stdin.
FNO_SHIM_DIR="$TMP_DIR/fno-shim"
make_fno_shim() {
  mkdir -p "$FNO_SHIM_DIR"
  cat >"$FNO_SHIM_DIR/fno" <<EOF
#!/usr/bin/env bash
cat >/dev/null 2>&1 || true
printf '%s\n' '$1'
exit 0
EOF
  chmod +x "$FNO_SHIM_DIR/fno"
}
# Default: recording failed (lock contention / old CLI) -> unrecorded marker.
make_fno_shim '{"recorded":false,"record_reason":"lock-timeout"}'

# The production defaults are a coupled contract: the writer must refresh
# before the reader can classify an active session as stale.
writer_throttle="$(sed -n 's/^LIVE_THROTTLE=\([0-9][0-9]*\)$/\1/p' "$HEARTBEAT")"
reader_window="$(sed -n 's/^WINDOW=\([0-9][0-9]*\)$/\1/p' "$HELPER")"
if [[ "$writer_throttle" == 30 && "$reader_window" == 120 ]] \
    && (( writer_throttle < reader_window )); then
  pass "production defaults keep the writer throttle inside the reader window"
else
  fail "default coupling: writer=[$writer_throttle] reader=[$reader_window]"
fi

# AC1-HP: another fresh session produces the advisory.
touch "$LIVE_DIR/peer-session"
out="$(run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && "$out" == *"Another session is working in this worktree."* \
      && "$out" == *"fno-overlap-observed"* ]]; then
  pass "fresh peer stamp emits the advisory"
else
  fail "fresh peer stamp: rc=$rc out=[$out]"
fi

# AC2-HP: a session never warns about its own stamp.
reset_live
touch "$LIVE_DIR/self-session"
out="$(run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && -z "$out" ]]; then
  pass "self-only stamp is silent"
else
  fail "self-only stamp: rc=$rc out=[$out]"
fi

# Exercise the production writer-to-reader boundary without fabricating a stamp.
reset_live
printf '{"cwd":"%s","session_id":"peer-session","tool_name":"Edit"}' "$PROJECT" \
  | CODEX_THREAD_ID= bash "$HEARTBEAT" >/dev/null 2>&1
out="$(run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && "$out" == *"fno-overlap-observed"* ]]; then
  pass "PostToolUse activity is consumed by the SessionStart reader"
else
  fail "writer-to-reader journey: rc=$rc out=[$out]"
fi

# The same journey must use Codex's thread identity as the stamp name.
reset_live
printf '{"cwd":"%s","session_id":"ignored","tool_name":"Edit"}' "$PROJECT" \
  | FNO_PLATFORM=codex CODEX_THREAD_ID=peer-codex bash "$HEARTBEAT" >/dev/null 2>&1
out="$(run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && -f "$LIVE_DIR/peer-codex" \
      && "$out" == *"fno-overlap-observed"* ]]; then
  pass "Codex PostToolUse activity is consumed by the shared reader"
else
  fail "Codex writer-to-reader journey: rc=$rc out=[$out]"
fi

# GNU stat accepts -f as a filesystem-report flag and prints text, so the
# reader must prefer -c rather than trusting a successful BSD-shaped call.
reset_live
touch "$LIVE_DIR/peer-session"
GNU_STAT_BIN="$TMP_DIR/gnu-stat-bin"
mkdir -p "$GNU_STAT_BIN"
# The stamp means "written a moment ago": bake a fixed recent-past time into the
# stub. A read-time date +%s can tick past the helper's frozen now on a slow
# runner, and the (correct) future-stamp rejection then silences the advisory.
GNU_STAT_STAMP=$(( $(date +%s) - 5 ))
cat >"$GNU_STAT_BIN/stat" <<EOF
#!/usr/bin/env bash
if [[ "\${1:-}" == -c && "\${2:-}" == %Y ]]; then
  echo "$GNU_STAT_STAMP"
  exit 0
fi
if [[ "\${1:-}" == -f ]]; then
  printf '  File: "fake"\n    ID: 0\n'
  exit 0
fi
exit 1
EOF
chmod +x "$GNU_STAT_BIN/stat"
out="$(PATH="$GNU_STAT_BIN:$PATH" run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && "$out" == *"fno-overlap-observed"* ]]; then
  pass "GNU stat output cannot break peer-stamp arithmetic"
else
  fail "GNU stat reader compatibility: rc=$rc out=[$out]"
fi

# A whole-directory .fno symlink must not leak activity across worktrees.
REPO="$TMP_DIR/repo"
PEER_WORKTREE="$TMP_DIR/peer-worktree"
SHARED_FNO="$TMP_DIR/shared-fno"
git init -q "$REPO"
git -C "$REPO" config user.email test@example.com
git -C "$REPO" config user.name Test
touch "$REPO/tracked"
git -C "$REPO" add tracked
git -C "$REPO" commit -qm init
git -C "$REPO" worktree add -q "$PEER_WORKTREE"
mkdir -p "$SHARED_FNO"
ln -s "$SHARED_FNO" "$REPO/.fno"
ln -s "$SHARED_FNO" "$PEER_WORKTREE/.fno"
ambient_dir="$(GIT_DIR="$(git -C "$REPO" rev-parse --absolute-git-dir)" \
  GIT_WORK_TREE="$REPO" bash "$HELPER" --live-dir "$PROJECT")"
if [[ "$ambient_dir" == "$LIVE_DIR" ]]; then
  pass "explicit cwd overrides ambient Git repository selection"
else
  fail "ambient Git environment redirected live dir to [$ambient_dir]"
fi
printf '{"cwd":"%s","session_id":"peer-session","tool_name":"Edit"}' "$REPO" \
  | CODEX_THREAD_ID= bash "$HEARTBEAT" >/dev/null 2>&1
other_out="$(printf '{"cwd":"%s","session_id":"self-session"}' "$PEER_WORKTREE" \
  | CODEX_THREAD_ID= bash "$HELPER" 2>/dev/null)"
same_out="$(printf '{"cwd":"%s","session_id":"self-session"}' "$REPO" \
  | CODEX_THREAD_ID= bash "$HELPER" 2>/dev/null)"
if [[ -z "$other_out" && "$same_out" == *"Another session is working in this worktree."* ]]; then
  pass "shared .fno state remains isolated by physical worktree"
else
  fail "shared .fno isolation: other=[$other_out] same=[$same_out]"
fi

# AC3: a stale predecessor is a handoff, regardless of harness identity.
reset_live
touch -t 202001010000 "$LIVE_DIR/prior-harness-session"
out="$(TEST_CODEX_THREAD_ID=self-session run_helper ignored-input 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && -z "$out" ]]; then
  pass "stale cross-harness handoff is silent"
else
  fail "stale handoff: rc=$rc out=[$out]"
fi

# AC4-ERR: absent and malformed live directories fail open and stay silent.
rm -rf "$LIVE_DIR"
out="$(run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && -z "$out" ]]; then
  pass "absent live directory is silent"
else
  fail "absent directory: rc=$rc out=[$out]"
fi
touch "$LIVE_DIR"
out="$(run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && -z "$out" ]]; then
  pass "malformed live path is silent"
else
  fail "malformed directory: rc=$rc out=[$out]"
fi
rm -f "$LIVE_DIR"
mkdir -p "$LIVE_DIR"
touch "$LIVE_DIR/peer-session"
chmod 000 "$LIVE_DIR"
out="$(run_helper self-session 2>/dev/null)"; rc=$?
chmod 700 "$LIVE_DIR"
if [[ "$rc" -eq 0 && -z "$out" ]]; then
  pass "unreadable live directory is silent"
else
  fail "unreadable directory: rc=$rc out=[$out]"
fi

# Claude's direct carrier renders the advisory; with recording failing it adds
# the visible unrecorded marker and still exits zero.
reset_live
touch "$LIVE_DIR/peer-session"
out="$(printf '{"cwd":"%s","session_id":"self-session"}' "$PROJECT" \
  | FNO_HOME="$TMP_DIR/fno-home" PATH="$FNO_SHIM_DIR:$PATH" \
      bash "$CARRIER" 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && "$out" == *"## Worktree hygiene"* \
      && "$out" == *"Another session is working in this worktree."* \
      && "$out" == *"[fno-overlap-unrecorded]"* ]]; then
  pass "Claude carrier renders advisory + unrecorded marker, exits zero"
else
  fail "Claude carrier: rc=$rc out=[$out]"
fi

# AC1-HP (structured): --machine mode emits one JSON observation with the fresh
# peer, the worktree/repo Git identity, and the 120s window.
reset_live
touch "$LIVE_DIR/peer-session"
mout="$(printf '{"cwd":"%s","session_id":"self-session"}' "$PROJECT" \
  | CODEX_THREAD_ID= bash "$HELPER" --machine 2>/dev/null)"; mrc=$?
if [[ "$mrc" -eq 0 \
      && "$(printf '%s' "$mout" | jq -r '.peer_session_ids[0] // empty' 2>/dev/null)" == "peer-session" \
      && "$(printf '%s' "$mout" | jq -r '.live_window_seconds // empty' 2>/dev/null)" == "120" \
      && -n "$(printf '%s' "$mout" | jq -r '.worktree_git_dir // empty' 2>/dev/null)" \
      && -n "$(printf '%s' "$mout" | jq -r '.repository_common_dir // empty' 2>/dev/null)" ]]; then
  pass "machine mode emits a structured peer observation"
else
  fail "machine mode observation: rc=$mrc out=[$mout]"
fi

# AC3-EDGE: machine mode is silent without a fresh peer (no evidence manufactured).
reset_live
touch "$LIVE_DIR/self-session"
mout="$(printf '{"cwd":"%s","session_id":"self-session"}' "$PROJECT" \
  | CODEX_THREAD_ID= bash "$HELPER" --machine 2>/dev/null)"; mrc=$?
if [[ "$mrc" -eq 0 && -z "$mout" ]]; then
  pass "machine mode is silent without a fresh peer"
else
  fail "machine mode silence: rc=$mrc out=[$mout]"
fi

# Carrier --body-only omits the section header (the Codex wrapper owns it).
reset_live
touch "$LIVE_DIR/peer-session"
bout="$(printf '{"cwd":"%s","session_id":"self-session"}' "$PROJECT" \
  | FNO_HOME="$TMP_DIR/fno-home" PATH="$FNO_SHIM_DIR:$PATH" \
      bash "$CARRIER" --body-only 2>/dev/null)"; brc=$?
if [[ "$brc" -eq 0 && "$bout" != *"## Worktree hygiene"* \
      && "$bout" == *"fno-overlap-observed"* ]]; then
  pass "body-only carrier omits the header"
else
  fail "body-only carrier: rc=$brc out=[$bout]"
fi

# AC5-UI: a recorded observation that crosses recurrence surfaces the notice.
make_fno_shim '{"recorded":true,"observation_id":"x","fold":{"state":"complete","distinct_observations":3,"recurrence_threshold":3,"recurrence_threshold_met":true}}'
reset_live
touch "$LIVE_DIR/peer-session"
tout="$(printf '{"cwd":"%s","session_id":"self-session"}' "$PROJECT" \
  | FNO_HOME="$TMP_DIR/fno-home" PATH="$FNO_SHIM_DIR:$PATH" \
      bash "$CARRIER" 2>/dev/null)"; trc=$?
if [[ "$trc" -eq 0 && "$tout" == *"recurrence reached 3/3"* \
      && "$tout" == *"fno worktree overlaps --since 28"* \
      && "$tout" != *"[fno-overlap-unrecorded]"* ]]; then
  pass "recurrence crossing surfaces the threshold notice"
else
  fail "recurrence notice: rc=$trc out=[$tout]"
fi

# AC6-ERR: append succeeded but the fold degraded -> count-unavailable, not unrecorded.
make_fno_shim '{"recorded":true,"observation_id":"x","fold":{"state":"partial"}}'
reset_live
touch "$LIVE_DIR/peer-session"
cout="$(printf '{"cwd":"%s","session_id":"self-session"}' "$PROJECT" \
  | FNO_HOME="$TMP_DIR/fno-home" PATH="$FNO_SHIM_DIR:$PATH" \
      bash "$CARRIER" 2>/dev/null)"; crc=$?
if [[ "$crc" -eq 0 && "$cout" == *"[fno-overlap-count-unavailable]"* \
      && "$cout" != *"[fno-overlap-unrecorded]"* ]]; then
  pass "degraded fold surfaces count-unavailable, not unrecorded"
else
  fail "count-unavailable: rc=$crc out=[$cout]"
fi
# Restore the default unrecorded shim for any later carrier checks.
make_fno_shim '{"recorded":false,"record_reason":"lock-timeout"}'

# The stranded refresh shares reconcile-throttle.sh's cross-platform mtime
# implementation. If that helper is absent, cached reporting may continue but
# refresh must stay disabled instead of starting a costly sweep every session.
MISSING_THROTTLE_ROOT="$TMP_DIR/missing-throttle-root"
MISSING_THROTTLE_LOG="$TMP_DIR/missing-throttle-fno.log"
MISSING_THROTTLE_BIN="$TMP_DIR/missing-throttle-bin"
mkdir -p "$MISSING_THROTTLE_ROOT/hooks/helpers" "$MISSING_THROTTLE_ROOT/.fno" "$MISSING_THROTTLE_BIN"
cp "$CARRIER" "$MISSING_THROTTLE_ROOT/hooks/worktree-peers-session-start.sh"
cat > "$MISSING_THROTTLE_ROOT/hooks/helpers/worktree-live-peers.sh" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
chmod +x "$MISSING_THROTTLE_ROOT/hooks/helpers/worktree-live-peers.sh"
cat > "$MISSING_THROTTLE_ROOT/.fno/.worktree-stranded-cache.json" <<'EOF'
{"rows":[{"class":"UNKNOWN","node":"x-test","path":"/tmp/test-worktree"}]}
EOF
cat > "$MISSING_THROTTLE_BIN/fno" <<'EOF'
#!/usr/bin/env bash
printf 'stranded-called\n' >> "$FNO_STRANDED_LOG"
printf '{"rows":[]}\n'
EOF
chmod +x "$MISSING_THROTTLE_BIN/fno"
missing_throttle_out="$(
  FNO_STRANDED_LOG="$MISSING_THROTTLE_LOG" PATH="$MISSING_THROTTLE_BIN:$PATH" \
    bash "$MISSING_THROTTLE_ROOT/hooks/worktree-peers-session-start.sh" 2>/dev/null
)"
for _ in 1 2 3 4 5 6 7 8 9 10; do
  [[ -s "$MISSING_THROTTLE_LOG" ]] && break
  sleep 0.1
done
if [[ "$missing_throttle_out" == *"[fno-stranded-unknown]"* \
      && ! -s "$MISSING_THROTTLE_LOG" ]]; then
  pass "missing throttle helper preserves cache reporting without launching refresh"
else
  fail "missing throttle helper: out=[$missing_throttle_out] refresh=[$(cat "$MISSING_THROTTLE_LOG" 2>/dev/null)]"
fi

# Codex's existing wrapper calls the same helper from its hygiene block.
WRAPPER_HOME="$TMP_DIR/home"
WRAPPER_BIN="$TMP_DIR/bin"
mkdir -p "$WRAPPER_HOME/.fno" "$WRAPPER_BIN"
touch "$WRAPPER_HOME/.fno/config.toml"
printf '#!/usr/bin/env bash\nexit 1\n' >"$WRAPPER_BIN/fno"
printf '#!/usr/bin/env bash\nexit 1\n' >"$WRAPPER_BIN/uv"
chmod +x "$WRAPPER_BIN/fno" "$WRAPPER_BIN/uv"
out="$(printf '{"cwd":"%s","session_id":"self-session"}' "$PROJECT" \
  | (cd "$PROJECT" && HOME="$WRAPPER_HOME" FNO_HOME="$WRAPPER_HOME/.fno" \
      PATH="$WRAPPER_BIN:$PATH" FNO_PLATFORM=codex CODEX_THREAD_ID=self-session \
      CLAUDE_PROJECT_DIR="$PROJECT" bash "$CODEX_WRAPPER") 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && "$out" == *"Another session is working in this worktree."* ]]; then
  pass "Codex wrapper renders the shared advisory"
else
  fail "Codex wrapper: rc=$rc out=[$out]"
fi

# AC5-CON: this mechanism is absent from PreToolUse and its PostToolUse writer
# exits 0 after Edit, Write, and Bash payloads in every stamp state.
pretool="$(jq -r '.hooks.PreToolUse[]?.hooks[]?.command // empty' \
  "$REPO_ROOT/hooks/hooks.json" "$REPO_ROOT/hooks/codex-hooks.json")"
if [[ "$pretool" == *"worktree-live-peers"* || "$pretool" == *"worktree-peers-session-start"* ]]; then
  fail "peer advisory is reachable from PreToolUse"
else
  pass "peer advisory is absent from both PreToolUse surfaces"
fi
never_refused=1
for state in absent malformed unreadable empty self fresh-peer stale-peer; do
  for tool in Edit Write Bash; do
    rm -rf "$LIVE_DIR"
    case "$state" in
      absent) ;;
      malformed) mkdir -p "${LIVE_DIR%/*}"; touch "$LIVE_DIR" ;;
      unreadable) mkdir -p "$LIVE_DIR"; chmod 000 "$LIVE_DIR" ;;
      empty) mkdir -p "$LIVE_DIR" ;;
      self) mkdir -p "$LIVE_DIR"; touch "$LIVE_DIR/self-session" ;;
      fresh-peer) mkdir -p "$LIVE_DIR"; touch "$LIVE_DIR/peer-session" ;;
      stale-peer) mkdir -p "$LIVE_DIR"; touch -t 202001010000 "$LIVE_DIR/peer-session" ;;
    esac
    printf '{"cwd":"%s","session_id":"self-session","tool_name":"%s"}' "$PROJECT" "$tool" \
      | CODEX_THREAD_ID= bash "$HEARTBEAT" >/dev/null 2>&1 || never_refused=0
    [[ "$state" == unreadable ]] && chmod 700 "$LIVE_DIR"
  done
done
if [[ "$never_refused" -eq 1 ]]; then
  pass "Edit, Write, and Bash never refuse across every stamp state"
else
  fail "an Edit, Write, or Bash activity payload returned non-zero"
fi

printf '[worktree-peers] %d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
