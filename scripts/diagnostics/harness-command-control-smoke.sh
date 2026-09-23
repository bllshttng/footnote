#!/usr/bin/env bash
# Exact-session native command journey. This diagnostic is deliberately
# fail-closed: it never resolves a session from a short name and never falls
# back to the operator's live FNO or Codex roots.
set -euo pipefail

ROOT="${FNO_SMOKE_ROOT:-/private/tmp/fno-continuation-proof}"
SESSION=""
HARNESS=""

usage() {
  cat >&2 <<'USAGE'
usage: harness-command-control-smoke.sh --session <full-session-uuid> --harness <codex|claude>

The selected session must already be a disposable session registered below
the isolated FNO roots. This command does not create, restart, or re-point a
live session. Set FNO_EMPTY_COMPOSER_EXPECT to an anchored line regex for the empty prompt
and FNO_SCREEN_EXPECT_PICKER/STATUS/COMPACT/RESUME to post-submit markers absent
before the matching screen action. FNO_SCREEN_EXPECT is the shared fallback.
USAGE
}

while (($#)); do
  case "$1" in
    --session)
      (($# >= 2)) || { usage; exit 64; }
      SESSION="$2"
      shift 2
      ;;
    --harness)
      (($# >= 2)) || { usage; exit 64; }
      HARNESS="$2"
      shift 2
      ;;
    -h|--help)
      usage >&1
      exit 0
      ;;
    *)
      usage
      exit 64
      ;;
  esac
done

if [[ ! "$SESSION" =~ ^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$ ]]; then
  echo "command-control-blocked: --session must be a full UUID; short or inferred identities are refused" >&2
  exit 2
fi
if [[ "$HARNESS" != codex && "$HARNESS" != claude ]]; then
  echo "command-control-blocked: --harness must be codex or claude" >&2
  exit 2
fi

for spec in \
  "FNO_HOME|$ROOT" \
  "FNO_AGENTS_HOME|$ROOT/agents" \
  "FNO_CLAIMS_ROOT|$ROOT/claims" \
  "FNO_SPACES_DIR|$ROOT/spaces" \
  "HOME|$ROOT/home" \
  "CODEX_HOME|$ROOT/codex"; do
  key="${spec%%|*}"
  expected="${spec#*|}"
  case "$key" in
    FNO_HOME) actual="${FNO_HOME-}" ;;
    FNO_AGENTS_HOME) actual="${FNO_AGENTS_HOME-}" ;;
    FNO_CLAIMS_ROOT) actual="${FNO_CLAIMS_ROOT-}" ;;
    FNO_SPACES_DIR) actual="${FNO_SPACES_DIR-}" ;;
    HOME) actual="${HOME-}" ;;
    CODEX_HOME) actual="${CODEX_HOME-}" ;;
  esac
  if [[ -n "$actual" && "$actual" != "$expected" ]]; then
    echo "command-control-blocked: $key=$actual is outside the disposable root; expected $expected" >&2
    exit 2
  fi
  export "$key=$expected"
  mkdir -p "$expected"
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MUX_BIN="${FNO_MUX_BIN:-$REPO_ROOT/crates/fno/target/debug/fno}"
if [[ ! -x "$MUX_BIN" ]]; then
  echo "command-control-blocked: native fno binary is missing or not executable: $MUX_BIN" >&2
  exit 2
fi

RUN_DIR="$ROOT/command-control-smoke"
mkdir -p "$RUN_DIR"
RUN_ID="command-control-$(date -u +%Y%m%dT%H%M%SZ)-$$"
SCREEN_EXPECT="${FNO_SCREEN_EXPECT:-}"
EMPTY_COMPOSER_EXPECT="${FNO_EMPTY_COMPOSER_EXPECT:-}"
LS_OUT="$RUN_DIR/mux-ls.json"
LS_ERR="$RUN_DIR/mux-ls.stderr"
if ! "$MUX_BIN" mux ls --json >"$LS_OUT" 2>"$LS_ERR"; then
  echo "command-control-blocked: isolated mux registry cannot be read for $SESSION" >&2
  sed -n '1,120p' "$LS_ERR" >&2
  exit 2
fi

if ! python3 - "$LS_OUT" "$SESSION" "$HARNESS" <<'PY'
import json
import sys
from pathlib import Path

path, session, harness = sys.argv[1:]
try:
    rows = json.loads(Path(path).read_text())
except (OSError, json.JSONDecodeError) as error:
    print(f"command-control-blocked: malformed isolated mux registry: {error}", file=sys.stderr)
    raise SystemExit(2)
if not isinstance(rows, list):
    print("command-control-blocked: mux registry is not a row list", file=sys.stderr)
    raise SystemExit(2)
matches = [row for row in rows if isinstance(row, dict) and (row.get("harness_session_id") or row.get("session_id")) == session]
if len(matches) != 1:
    print(f"command-control-blocked: expected exactly one disposable row for full session {session}", file=sys.stderr)
    raise SystemExit(2)
row = matches[0]
if row.get("harness") != harness and row.get("provider") != harness:
    print(f"command-control-blocked: selected session harness mismatch: {row.get('harness') or row.get('provider')!r}", file=sys.stderr)
    raise SystemExit(2)
if row.get("harness_session_id") != session:
    print("command-control-blocked: registry did not preserve the full session id", file=sys.stderr)
    raise SystemExit(2)
PY
then
  exit 2
fi

run_case() {
  local case_name="$1"
  local text="$2"
  local proof="$3"
  local expected="$4"
  local timeout_seconds="$5"
  local screen_expect="${6:-$SCREEN_EXPECT}"
  local expected_error="${7:-}"
  local empty_composer_expect="${8:-$EMPTY_COMPOSER_EXPECT}"
  local request_id="${RUN_ID}-${case_name}"
  local out="$RUN_DIR/${case_name}.stdout"
  local err="$RUN_DIR/${case_name}.stderr"
  local code=0
  local expect_args=()
  if [[ "$proof" == screen ]]; then
    if [[ -z "$screen_expect" || -z "$empty_composer_expect" ]]; then
      echo "command-control-blocked: $case_name needs FNO_SCREEN_EXPECT and FNO_EMPTY_COMPOSER_EXPECT" >&2
      exit 2
    fi
    expect_args=(--expect "$screen_expect" --empty-composer "$empty_composer_expect")
  fi
  set +e
  "$MUX_BIN" mux command "$SESSION" \
    --text "$text" \
    --proof "$proof" \
    --timeout-seconds "$timeout_seconds" \
    --request-id "$request_id" \
    "${expect_args[@]}" >"$out" 2>"$err"
  code=$?
  set -e
  local replay_out="$RUN_DIR/${case_name}.replay.stdout"
  local replay_err="$RUN_DIR/${case_name}.replay.stderr"
  local replay_code=0
  if [[ "$expected" == unknown ]]; then
    set +e
    "$MUX_BIN" mux command "$SESSION" \
      --text "$text" \
      --proof "$proof" \
      --timeout-seconds "$timeout_seconds" \
      --request-id "$request_id" \
      "${expect_args[@]}" >"$replay_out" 2>"$replay_err"
    replay_code=$?
    set -e
  fi
  CASE_CODE="$code" CASE_OUT="$out" CASE_ERR="$err" CASE_EXPECTED="$expected" CASE_EXPECTED_ERROR="$expected_error" CASE_NAME="$case_name" SESSION="$SESSION" CASE_REPLAY_OUT="$replay_out" CASE_REPLAY_CODE="$replay_code" python3 - <<'PY'
import json
import os
import sys
from pathlib import Path

name = os.environ["CASE_NAME"]
code = int(os.environ["CASE_CODE"])
expected = os.environ["CASE_EXPECTED"]
expected_error = os.environ["CASE_EXPECTED_ERROR"]
out = Path(os.environ["CASE_OUT"]).read_text(errors="replace")
err = Path(os.environ["CASE_ERR"]).read_text(errors="replace")
row = None
for line in out.splitlines():
    try:
        candidate = json.loads(line)
    except json.JSONDecodeError:
        continue
    if isinstance(candidate, dict) and "status" in candidate:
        row = candidate
        break
if expected == "refused":
    if (
        code == 0
        or not expected_error
        or expected_error not in err
        or row is None
        or row.get("status") != "refused"
        or row.get("session_id") != os.environ["SESSION"]
    ):
        print(f"command-control-blocked: {name} refusal lacked its expected reason: {err.strip()}", file=sys.stderr)
        raise SystemExit(2)
    if name == "pending-composer" and (
        not row.get("before_digest")
        or row.get("before_digest") != row.get("after_digest")
        or "screen unchanged" not in row.get("detail", "")
    ):
        print(f"command-control-blocked: {name} did not prove the composer screen stayed unchanged", file=sys.stderr)
        raise SystemExit(2)
    print(json.dumps({"case": name, "expected": expected, "status": row["status"], "receipt": row, "exit": code}))
    raise SystemExit(0)
if row is None:
    print(f"command-control-blocked: {name} returned no native receipt: {err.strip()}", file=sys.stderr)
    raise SystemExit(2)
if row.get("status") != expected or row.get("session_id") != os.environ["SESSION"]:
    print(f"command-control-blocked: {name} receipt is not {expected} for the selected full session", file=sys.stderr)
    raise SystemExit(2)
if expected == "unknown":
    replay = Path(os.environ["CASE_REPLAY_OUT"]).read_text(errors="replace")
    replay_rows = []
    for line in replay.splitlines():
        try:
            candidate = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(candidate, dict) and "status" in candidate:
            replay_rows.append(candidate)
    if int(os.environ["CASE_REPLAY_CODE"]) == 0 or replay_rows != [row]:
        print(f"command-control-blocked: {name} retry did not return the same unknown receipt", file=sys.stderr)
        raise SystemExit(2)
print(json.dumps({"case": name, "expected": expected, "status": row["status"], "receipt": row, "exit": code}))
PY
}

export SESSION
# The sequence is intentionally explicit and ordered. Negative cases must
# refuse before typing; positive cases must return the native postcondition.
if [[ "$HARNESS" == codex ]]; then
  run_case paused-to-active "/goal resume" goal-active verified 30
  run_case idle-goal "/goal" goal-active verified 30
  run_case busy-refusal "/goal" goal-active refused 30 "" "refusing before typing"
  run_case pending-composer "/rc" screen refused 30 "${FNO_SCREEN_EXPECT_PICKER:-$SCREEN_EXPECT}" "composer"
  run_case screen-picker "/rc" screen verified 30 "${FNO_SCREEN_EXPECT_PICKER:-$SCREEN_EXPECT}"
  run_case timeout-no-retry "/compact" compact unknown 1
  run_case provider-compact "/compact" compact verified 30
else
  run_case idle-status "/status" screen verified 30 "${FNO_SCREEN_EXPECT_STATUS:-$SCREEN_EXPECT}"
  run_case busy-refusal "/status" screen refused 30 "${FNO_SCREEN_EXPECT_STATUS:-$SCREEN_EXPECT}" "refusing before typing"
  run_case pending-composer "/rc" screen refused 30 "${FNO_SCREEN_EXPECT_PICKER:-$SCREEN_EXPECT}" "composer"
  run_case screen-picker "/rc" screen verified 30 "${FNO_SCREEN_EXPECT_PICKER:-$SCREEN_EXPECT}"
  run_case timeout-no-retry "/status" screen unknown 1 "${FNO_SCREEN_EXPECT_TIMEOUT:-$SCREEN_EXPECT}"
  run_case provider-compact "/compact" screen verified 30 "${FNO_COMPACT_SCREEN_EXPECT:-$SCREEN_EXPECT}"
  run_case paused-to-active "/rc" screen verified 30 "${FNO_SCREEN_EXPECT_RESUME:-$SCREEN_EXPECT}"
fi

python3 - "$RUN_DIR" "$SESSION" "$HARNESS" "$ROOT" "$RUN_ID" <<'PY'
import datetime as dt
import hashlib
import json
import sys
from pathlib import Path

run_dir = Path(sys.argv[1])
session, harness, root, run_id = sys.argv[2:]
rows = []
for path in sorted(run_dir.glob("*.stdout")):
    for line in path.read_text(errors="replace").splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if value.get("case"):
            rows.append(value)
if len(rows) != 7 or any(
    row.get("status") != row.get("expected")
    or row.get("status") not in {"verified", "refused", "unknown"}
    for row in rows
):
    print("command-control-blocked: command journey did not produce all seven classified receipts", file=sys.stderr)
    raise SystemExit(2)
positive = [row for row in rows if row.get("status") == "verified"]
if len(positive) < 4:
    print("command-control-blocked: command journey lacks the four positive receipts", file=sys.stderr)
    raise SystemExit(2)
payload = "\n".join(path.read_text(errors="replace") for path in sorted(run_dir.glob("*.stdout")))
receipt = {
    "schema_version": 1,
    "created_at": dt.datetime.now(dt.timezone.utc).isoformat(),
    "versions": {"fno": "native", "harness": harness},
    "session_id": session,
    "harness": harness,
    "run_id": run_id,
    "action_hash": "sha256:" + hashlib.sha256(payload.encode()).hexdigest(),
    "cases": rows,
    "root": str(root),
    "status": "verified",
}
stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
path = run_dir / f"command_control_{stamp}.json"
path.write_text(json.dumps(receipt, indent=2) + "\n")
print(f"harness_command_control_verified session={session} receipt={path}")
PY
