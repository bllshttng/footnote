#!/usr/bin/env bash
# Verify the Codex Stop hook nudges the author in-session on one non-clean
# native review result, without a king or daemon transport.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/codex-review-findings.sh"
TMP="$(mktemp -d -t codex-review-findings.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

WORK="$TMP/repo"
mkdir -p "$WORK/.fno"
git -C "$WORK" init -q
git -C "$WORK" config user.email t@t.t
git -C "$WORK" config user.name t
git -C "$WORK" commit -qm init --allow-empty

run_hook() {
  local turn="$1" findings="$2"
  local transcript="$TMP/$turn.jsonl"
  jq -nc --arg turn "$turn" --argjson findings "$findings" \
    '{type:"event_msg",payload:{type:"exited_review_mode",turn_id:$turn,
      review_output:{findings:$findings}}}' > "$transcript"
  jq -nc --arg cwd "$WORK" --arg turn "$turn" --arg transcript "$transcript" \
    '{hook_event_name:"Stop",cwd:$cwd,turn_id:$turn,transcript_path:$transcript}' \
    | (cd "$WORK" && bash "$HOOK") 2>"$TMP/$turn.err"
  echo "$?"
}

findings='[{"priority":"P1","file":"a.py"},{"priority":"P2","file":"b.py"},{"priority":"P3","file":"c.py"}]'
rc="$(run_hook turn-findings "$findings")"
[[ "$rc" == "2" ]] || { echo "FAIL: findings stop must block with rc=2, got $rc"; exit 1; }
grep -q 'P1: 1, P2: 1' "$TMP/turn-findings.err" \
  || { echo "FAIL: nudge does not report positive P1/P2 counts"; exit 1; }
grep -q 'Act on the findings in your current context' "$TMP/turn-findings.err" \
  || { echo "FAIL: nudge does not tell the worker to act in-session"; exit 1; }
grep -q 'request-self-review --pr <n>' "$TMP/turn-findings.err" \
  || { echo "FAIL: nudge does not require a new-head review"; exit 1; }
! grep -qE 'fno agents mail|daemon|king' "$TMP/turn-findings.err" \
  || { echo "FAIL: nudge routed through forbidden king/daemon/mail path"; exit 1; }

rc="$(run_hook turn-findings "$findings")"
[[ "$rc" == "0" ]] || { echo "FAIL: same review turn was nudged twice"; exit 1; }

rc="$(run_hook turn-clean '[]')"
[[ "$rc" == "0" ]] || { echo "FAIL: clean review was blocked"; exit 1; }
[[ ! -s "$TMP/turn-clean.err" ]] || { echo "FAIL: clean review produced a nudge"; exit 1; }

echo "PASS: Codex review findings nudge the owning worker once at Stop"
