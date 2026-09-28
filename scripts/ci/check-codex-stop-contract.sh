#!/usr/bin/env bash
# Codex Stop-hook output contract.
#
# Codex reads a Stop hook group's output as ONE stop decision: any non-empty
# exit-0 stdout that is not a single JSON object fails the group, and a failed
# Stop hook ends the turn the target loop meant to continue. A worker that
# finishes a turn with the loop still open then sits at an idle prompt until a
# human types into it - the failure mode this check exists to keep impossible.
#
# The contract every codex Stop command must hold on every reachable branch:
#   - exit code 0 (allow) or 2 (block: the continuation reason rides stderr)
#   - stdout empty, or exactly one JSON object
#
# The check replays every command in hooks/codex-hooks.json's Stop group
# against three codex Stop payloads (plain turn, review-with-findings turn,
# review-clean turn) and asserts the contract on each run. The payload cwd is
# a bare temp dir, so the review hooks take their refusal branches instead of
# touching a real checkout; `--self-test` proves the harness fails on a hook
# that prints plain text at exit 0 (the exact drift this guards against)
# before the green is trusted on the real tree.
#
# jq and python3 are the only tool dependencies; no fno install is required
# (the hooks' own fno calls degrade to their refusal exits inside the sandbox,
# and those exits are contract-shaped too).

set -euo pipefail

SELF_TEST=0
[[ "${1:-}" == "--self-test" ]] && SELF_TEST=1

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"
[[ -n "$REPO_ROOT" ]] || REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

if [[ "$SELF_TEST" == "1" ]]; then
  sandbox="$(mktemp -d)"
  trap 'rm -rf "$sandbox"' EXIT
  mkdir -p "$sandbox/hooks"
  # One honest hook, one drifted hook: the drifted one prints plain text at
  # exit 0, which is the output shape that fails a codex Stop group.
  cat > "$sandbox/hooks/codex-hooks.json" <<'JSON'
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          { "type": "command", "command": "bash ${PLUGIN_ROOT}/hooks/good.sh" },
          { "type": "command", "command": "bash ${PLUGIN_ROOT}/hooks/drifted.sh" }
        ]
      }
    ]
  }
}
JSON
  printf '#!/usr/bin/env bash\nexit 0\n' > "$sandbox/hooks/good.sh"
  printf '#!/usr/bin/env bash\necho "review held: 1 finding"\nexit 0\n' > "$sandbox/hooks/drifted.sh"
  if bash "$0" --check-root "$sandbox" > /dev/null 2>&1; then
    echo "self-test FAILED: plain-text stdout at exit 0 passed the contract" >&2
    exit 1
  fi
  echo "self-test ok: plain-text stdout at exit 0 fails the contract"
  exit 0
fi

CHECK_ROOT="${2:-}"
if [[ "${1:-}" == "--check-root" && -n "$CHECK_ROOT" ]]; then
  hooks_json="$CHECK_ROOT/hooks/codex-hooks.json"
  plugin_root="$CHECK_ROOT"
else
  hooks_json="$REPO_ROOT/hooks/codex-hooks.json"
  plugin_root="$REPO_ROOT"
fi
[[ -f "$hooks_json" ]] || { echo "missing $hooks_json" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
sandbox="$work/cwd"
mkdir -p "$sandbox"

# Three Stop fixtures. The transcript rows mirror the shapes the review
# readers select on: an item_completed carrying an ExitedReviewMode item.
# Every run gets a FRESH turn id: the findings hook nudges once per turn id
# and exits 0 on a repeat, so a reused id would mute the positive control.
make_pair() {
  local name="$1" tid="$2" findings="$3"
  if [[ "$findings" == "none" ]]; then
    printf '%s\n' '{"type":"event_msg","payload":{"turn_id":"'"$tid"'","type":"item_completed","item":{"type":"agent_message","text":"done"}}}' > "$work/$name.jsonl"
  else
    printf '%s\n' '{"type":"event_msg","payload":{"turn_id":"'"$tid"'","type":"item_completed","item":{"type":"ExitedReviewMode","review_output":{"findings":'"$findings"'}}}}' > "$work/$name.jsonl"
  fi
  printf '%s' "{\"session_id\":\"sess-fixture\",\"transcript_path\":\"$work/$name.jsonl\",\"cwd\":\"$sandbox\",\"hook_event_name\":\"Stop\",\"stop_hook_active\":false,\"turn_id\":\"$tid\",\"last_assistant_message\":\"turn finished\"}" > "$work/$name.payload.json"
}
make_pair plain turn-plain-1 none
make_pair review_bad turn-bad-1 '[{"title":"[P1] broken invariant","priority":"P1"}]'
make_pair review_clean turn-clean-1 '[]'

commands=()
while IFS= read -r line; do
  [[ -n "$line" ]] && commands+=("$line")
done < <(jq -r '.hooks.Stop[0].hooks[].command' "$hooks_json")
if [[ "${#commands[@]}" -eq 0 ]]; then
  echo "no Stop commands found in $hooks_json" >&2
  exit 1
fi

assert_output_contract() {
  local label="$1" stdout="$2"
  [[ -z "$stdout" ]] && return 0
  printf '%s' "$stdout" | python3 -c '
import json, sys
raw = sys.stdin.read()
value = json.loads(raw)
assert isinstance(value, dict), f"stdout is {type(value).__name__}, not one JSON object"
' || { echo "CONTRACT FAIL [$label]: stdout is not empty or one JSON object: ${stdout:0:200}" >&2; return 1; }
}

failures=0
runs=0
fixture_findings() {
  case "$1" in
    review_bad) printf '[{"title":"[P1] broken invariant","priority":"P1"}]' ;;
    review_clean) printf '[]' ;;
    *) printf 'none' ;;
  esac
}
for command in "${commands[@]}"; do
  expanded="${command//\$\{PLUGIN_ROOT\}/$plugin_root}"
  for fixture in plain review_bad review_clean; do
    label="$(basename "${expanded##* }") x $fixture"
    make_pair "$fixture" "turn-$fixture-$runs" "$(fixture_findings "$fixture")"
    rc=0
    out="$(printf '%s' "$(cat "$work/$fixture.payload.json")" | bash -c "$expanded" 2>/dev/null)" || rc=$?
    runs=$((runs + 1))
    case "$rc" in
      0|2) ;;
      *) echo "CONTRACT FAIL [$label]: exit $rc (only 0 allow / 2 block are contract-shaped)" >&2; failures=$((failures + 1)); continue ;;
    esac
    assert_output_contract "$label" "$out" || failures=$((failures + 1))
  done
done

# Positive control: the review-with-findings fixture must actually engage the
# findings hook (exit 2, the block branch). A fixture that engages nothing
# would prove nothing about the branches that matter.
findings_command=""
while IFS= read -r command; do
  case "$command" in *codex-review-findings*) findings_command="$command" ;; esac
done < <(printf '%s\n' "${commands[@]}")
if [[ -n "$findings_command" ]]; then
  expanded="${findings_command//\$\{PLUGIN_ROOT\}/$plugin_root}"
  make_pair review_bad turn-bad-control '[{"title":"[P1] broken invariant","priority":"P1"}]'
  rc=0
  printf '%s' "$(cat "$work/review_bad.payload.json")" | bash -c "$expanded" > /dev/null 2>&1 || rc=$?
  if [[ "$rc" != "2" ]]; then
    echo "POSITIVE CONTROL FAIL: codex-review-findings on the findings fixture exited $rc, expected 2" >&2
    failures=$((failures + 1))
  fi
fi

if [[ "$failures" -gt 0 ]]; then
  echo "codex stop contract: $failures failure(s) across $runs run(s)" >&2
  exit 1
fi
echo "codex stop contract ok: $runs run(s), stdout empty or one JSON object, exits 0/2 only"
