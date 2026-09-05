#!/usr/bin/env bash
# End-to-end acceptance for the review-cap invocation gate.
#
# The acceptance is BEHAVIOR, never a counter read: a worker on a real PR
# whose rounds are spent, attempting a third review through the same Skill
# hook path a real invocation takes, must be DENIED - and the two law
# carveouts must pass through the identical path.
#
# Fixture: a real git repo on a FEATURE branch (the hook rightly ignores the
# protected branch) whose journal carries two spent rounds with the real
# base sha, so the interdiff carveout measures real patches; the hook runs
# for real with FNO pointed at a wrapper that execs the checkout's own CLI
# (fno-py: the deployed cargo shim shadows the source `fno` on PATH).
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
WORK="$(mktemp -d /tmp/fno-cap-gate.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

CLI_DIR="$REPO/cli"
cat > "$WORK/fno-wrapper" <<WRAPPER
#!/usr/bin/env bash
exec uv run --quiet --project "$CLI_DIR" fno-py "\$@"
WRAPPER
chmod +x "$WORK/fno-wrapper"

FAILURES=0
check() { # check <name> <condition-result>
  if [ "$2" = "0" ]; then
    echo "PASS: $1"
  else
    echo "FAIL: $1"
    FAILURES=$((FAILURES + 1))
  fi
}

deny_json() { python3 -c '
import json, sys
text = open(sys.argv[1]).read()
start = text.find("{")
denied = False
while start != -1:
    try:
        row, end = json.JSONDecoder().raw_decode(text[start:])
        denied = denied or row.get("hookSpecificOutput", {}).get("permissionDecision") == "deny"
        start = text.find("{", start + end)
    except json.JSONDecodeError:
        break
sys.exit(0 if denied else 1)
' "$WORK/hook.out"; }

append_attestation() { # <repo> <head> <base> <branch>
  mkdir -p "$1/.fno"
  python3 - "$1" "$2" "$3" "$4" <<'PYEOF'
import json, sys, pathlib
repo, head, base, branch = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
row = {"ts": "2026-09-04T12:00:00Z", "type": "review_attestation", "source": "hook",
       "data": {"reviewer": "code-review", "head_sha": head, "verdict": "fail",
                "branch": branch, "reviewed_base_sha": base,
                "reviewed_head_sha": head, "reviewed_file_count": 1,
                "reviewed_line_count": 10, "findings_blocking": 0,
                "findings_nonblocking": 0, "findings": [], "findings_truncated": False,
                "dispositions": []}}
with pathlib.Path(repo, ".fno", "events.jsonl").open("a") as fh:
    fh.write(json.dumps(row) + "\n")
PYEOF
}

run_hook() { # run_hook <repo> [args]
  local repo="$1" args="${2:-}"
  local head branch
  head="$(git -C "$repo" rev-parse HEAD)"
  branch="$(git -C "$repo" rev-parse --abbrev-ref HEAD)"
  local skill_input
  skill_input="fno:review${args:+ $args}"
  printf '{"tool_name":"Skill","session_id":"cap-test","cwd":"%s","tool_input":{"skill":"%s"}}\n' \
    "$repo" "$skill_input" \
    | FNO="$WORK/fno-wrapper" bash "$REPO/hooks/review-hold.sh" acquire \
    > "$WORK/hook.out" 2> "$WORK/hook.err"
}

# make_repo <repo>: main with a 100-line base, feature/x on top with one
# 100-line rewrite commit; prints nothing, leaves HEAD at feature/x round 1.
make_repo() {
  local repo="$1"
  mkdir -p "$repo"
  git -C "$repo" init -q -b main
  git -C "$repo" config user.email t@t
  git -C "$repo" config user.name t
  python3 -c "print(''.join(f'base{i}\n' for i in range(1, 101)))" > "$repo/f.txt"
  git -C "$repo" add -A
  git -C "$repo" commit -q -m base
  git -C "$repo" checkout -q -b feature/x
  python3 -c "print(''.join(f'rev{i}\n' for i in range(1, 101)))" > "$repo/f.txt"
  git -C "$repo" add -A
  git -C "$repo" commit -q -m 'round 1'
}

rewrite_to() { # rewrite_to <repo> <n>
  python3 -c "import sys; n=sys.argv[1]; open(sys.argv[2],'w').write(''.join(f'w{n}-{i}\n' for i in range(1,101)))" "$2" "$1/f.txt"
  git -C "$1" add -A
  git -C "$1" commit -q -m "rewrite to $2"
}

# --- the acceptance: a third review at the spent cap is DENIED --------------
repo="$WORK/spent"
make_repo "$repo"
base="$(git -C "$repo" rev-parse main)"
r1="$(git -C "$repo" rev-parse HEAD)"
rewrite_to "$repo" 2
head="$(git -C "$repo" rev-parse HEAD)"
append_attestation "$repo" "$r1" "$base" feature/x
append_attestation "$repo" "$head" "$base" feature/x
run_hook "$repo"
if ! deny_json; then
  echo "--- hook stdout ---"; cat "$WORK/hook.out"
  echo "--- hook stderr ---"; cat "$WORK/hook.err"
fi
deny_json; check "third review at the spent cap is denied end to end" $?
grep -q "two-round cap is spent" "$WORK/hook.out"; check "the denial names the spent budget and remedy" $?

# --- carveout: scoped fix-verification passes through the identical path ----
run_hook "$repo" "--verify-fixes"
if deny_json 2>/dev/null; then check "verify-fixes at the cap is denied (must pass)" 1; else check "verify-fixes at the cap passes" 0; fi

# --- carveout: a rebase delta over the interdiff budget reviews freely ------
repo2="$WORK/delta"
make_repo "$repo2"
base2="$(git -C "$repo2" rev-parse main)"
r1="$(git -C "$repo2" rev-parse HEAD)"
rewrite_to "$repo2" 2
mid="$(git -C "$repo2" rev-parse HEAD)"
append_attestation "$repo2" "$r1" "$base2" feature/x
append_attestation "$repo2" "$mid" "$base2" feature/x
rewrite_to "$repo2" 3
end="$(git -C "$repo2" rev-parse HEAD)"
[ "$end" != "$mid" ]; check "the delta fixture moved the head (instrument sanity)" $?
run_hook "$repo2"
if deny_json 2>/dev/null; then check "delta review over budget is denied (must pass)" 1; else check "delta review over the interdiff budget passes" 0; fi

# --- control: a fresh budget never denies -----------------------------------
repo3="$WORK/fresh"
make_repo "$repo3"
base3="$(git -C "$repo3" rev-parse main)"
append_attestation "$repo3" "$(git -C "$repo3" rev-parse HEAD)" "$base3" feature/x
run_hook "$repo3"
if deny_json 2>/dev/null; then check "fresh budget is denied (must pass)" 1; else check "fresh budget passes" 0; fi

echo "FAILURES=$FAILURES"
[ "$FAILURES" = "0" ]
