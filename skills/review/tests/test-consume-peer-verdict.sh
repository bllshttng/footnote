#!/usr/bin/env bash
set -euo pipefail

REVIEW_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONSUME="$REVIEW_DIR/scripts/consume-peer-verdict.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

REPO="$TMP/repo"
BIN="$TMP/bin"
mkdir -p "$REPO" "$BIN"
git -C "$REPO" init -q
git -C "$REPO" config user.email test@example.com
git -C "$REPO" config user.name Test
touch "$REPO/tracked"
git -C "$REPO" add tracked
git -C "$REPO" commit -qm init

cat > "$BIN/fno" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$FNO_TEST_LOG"
SH
chmod +x "$BIN/fno"
export PATH="$BIN:$PATH"
export FNO_TEST_LOG="$TMP/events"

run_case() {
  name="$1"
  expected_rc="$2"
  expected_verdict="$3"
  body="$4"
  file="$TMP/$name.txt"
  printf '%s' "$body" > "$file"
  : > "$FNO_TEST_LOG"
  set +e
  (cd "$REPO" && bash "$CONSUME" "$file") >"$TMP/$name.out" 2>"$TMP/$name.err"
  rc=$?
  set -e
  if [[ "$rc" -ne "$expected_rc" ]]; then
    echo "$name: expected rc=$expected_rc, got $rc" >&2
    cat "$TMP/$name.err" >&2
    exit 1
  fi
  grep -q "verdict\\\":\\\"$expected_verdict" "$FNO_TEST_LOG" || {
    echo "$name: expected emitted verdict $expected_verdict" >&2
    cat "$FNO_TEST_LOG" >&2
    exit 1
  }
}

run_case clean 0 pass $'No findings.\nfno-peer-verdict: {"verdict":"clean","blocking_findings":0}\n'
run_case blocked 1 fail $'P1 src/lib.rs:9 - panic - handle the error\nfno-peer-verdict: {"verdict":"blocked","blocking_findings":1}\n'
run_case contradictory 1 fail $'P1 src/lib.rs:9 - panic - handle the error\nfno-peer-verdict: {"verdict":"clean","blocking_findings":0}\n'
run_case malformed 1 fail $'Looks fine.\nverdict: clean\n'
run_case empty 1 fail ''
run_case count_mismatch 1 fail $'P1 a:1 - one - fix\nfno-peer-verdict: {"verdict":"blocked","blocking_findings":2}\n'

echo "PASS consume-peer-verdict"
