#!/usr/bin/env bash
# apply-merge-ruleset.sh - create, verify, or remove the merge ruleset.
#
# The ruleset is the server-side half of the merge coverage guard: GitHub
# itself refuses a merge whose head carries no passing `fno/review-coverage`
# status (and no passing `stacked-base-guard`), for every client path - `gh
# pr merge`, the web button, raw REST, the auto-merge queue, and `fno pr
# merge` alike. The ruleset lives as committed data (merge-ruleset.json,
# reviewable in the diff) because branch rules are repository settings no
# code push can carry; this applier is the one bridge between them.
#
# bypass_actors is EMPTY and the script hard-refuses to apply a file where it
# is not. Rulesets, unlike classic branch protection, grant no implicit admin
# bypass: an empty list binds the repository owner too, and the owner account
# is the account every worker merges as - a bypass entry would reopen the
# exact hole the ruleset closes.
#
# Usage: apply-merge-ruleset.sh --check [--expect-absent]
#        apply-merge-ruleset.sh --apply
#        apply-merge-ruleset.sh --remove
# Exit:  0 the requested state holds (or was reached)
#        1 a check failed - the message names WHICH expectation broke
#        2 usage error, or the data file itself is malformed
#
# Applying changes repository settings for everyone: it is an operator step,
# run once, after a PR has proven a green status on its own head. --check is
# what the post-merge audit runs; it compares live state to the file so a
# ruleset deleted or weakened in the GitHub UI fails the next push to main.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
DATA_FILE="${HERE}/merge-ruleset.json"
RULESET_NAME="$(python3 -c 'import json;print(json.load(open("'"$DATA_FILE"'"))["name"])')" || exit 2
REPO_PATH="repos/:owner/:repo"

mode=""
expect_absent=0
for arg in "$@"; do
  case "$arg" in
    --check) mode="check" ;;
    --apply) mode="apply" ;;
    --remove) mode="remove" ;;
    --expect-absent) expect_absent=1 ;;
    *) echo "usage: $0 --check [--expect-absent] | --apply | --remove" >&2; exit 2 ;;
  esac
done
if [ -z "$mode" ]; then
  echo "usage: $0 --check [--expect-absent] | --apply | --remove" >&2
  exit 2
fi

# The data file is validated before any mode runs: a malformed file must fail
# as a data error (exit 2), never as a confusing API error halfway through.
file_contexts="$(python3 -c '
import json
d = json.load(open("'"$DATA_FILE"'"))
rules = d.get("rules", [])
pr = next((r for r in rules if r.get("type") == "pull_request"), None)
assert pr, "data file: no pull_request rule"
ctxs = pr.get("parameters", {}).get("required_status_checks", {}).get("contexts")
assert isinstance(ctxs, list) and ctxs, "data file: no required status contexts"
assert d.get("bypass_actors") == [], "data file: bypass_actors is not empty"
assert any(r.get("type") == "non_fast_forward" for r in rules), "data file: no non_fast_forward rule"
print(" ".join(sorted(ctxs)))
')" || exit 2

# The live ruleset with this name, or "" when none exists.
live_id() {
  gh api "$REPO_PATH/rulesets" --paginate \
    --jq ".[] | select(.name == \"${RULESET_NAME}\") | .id" 2>/dev/null | head -1
}

found="$(live_id)" || { echo "FAIL: could not read live rulesets" >&2; exit 1; }
found="${found:-}"

case "$mode" in
  check)
    if [ "$expect_absent" = 1 ]; then
      if [ -n "$found" ]; then
        echo "FAIL: ruleset '${RULESET_NAME}' exists (id ${found}); expected absent" >&2
        exit 1
      fi
      echo "ok: no '${RULESET_NAME}' ruleset present"
      exit 0
    fi
    if [ -z "$found" ]; then
      echo "FAIL: ruleset '${RULESET_NAME}' not found - run $0 --apply" >&2
      exit 1
    fi
    live="$(gh api "$REPO_PATH/rulesets/$found")" || {
      echo "FAIL: could not read ruleset ${found}" >&2; exit 1;
    }
    fail=0
    live_contexts="$(printf '%s' "$live" | python3 -c '
import json,sys
d = json.load(sys.stdin)
pr = next((r for r in d.get("rules", []) if r.get("type") == "pull_request"), None)
ctxs = (pr or {}).get("parameters", {}).get("required_status_checks", {}).get("contexts") or []
print(" ".join(sorted(ctxs)))
')"
    if [ "$live_contexts" != "$file_contexts" ]; then
      echo "FAIL: required contexts drifted - live [${live_contexts}] vs file [${file_contexts}]" >&2
      fail=1
    fi
    if ! printf '%s' "$live" | python3 -c '
import json,sys
d = json.load(sys.stdin)
f = json.load(open(sys.argv[1]))
# bypass_actors is withheld from tokens without administration scope, and a
# workflow GITHUB_TOKEN cannot be granted one. A missing field is a named
# note, not a failure: the visible fields still positively pin the gate, and
# constant red on main for an unreadable field is the noise that teaches
# ignoring the audit. A NON-empty list is still a hard failure.
bypass = d.get("bypass_actors")
assert bypass in (None, []), "live bypass_actors not empty: %r" % bypass
if bypass is None:
    print("note: live bypass_actors withheld from this token (no administration scope); verified the visible fields")
assert any(r.get("type") == "non_fast_forward" for r in d.get("rules", [])), "no non_fast_forward rule"
assert d.get("enforcement") == "active", "enforcement is %r" % d.get("enforcement")
assert d.get("target") == f.get("target"), "target drifted: %r vs %r" % (d.get("target"), f.get("target"))
assert d.get("conditions") == f.get("conditions"), "conditions drifted (which refs the ruleset protects)"
' "$DATA_FILE" ; then
      fail=1
    fi
    if [ "$fail" = 1 ]; then
      echo "FAIL: live ruleset '${RULESET_NAME}' (id ${found}) no longer matches the committed data" >&2
      exit 1
    fi
    echo "ok: ruleset '${RULESET_NAME}' (id ${found}) matches the committed data"
    ;;
  apply)
    # bypass_actors was already hard-asserted by the file validation above.
    if [ -n "$found" ]; then
      gh api --method PUT "$REPO_PATH/rulesets/$found" \
        -H "Content-Type: application/json" --input "$DATA_FILE" >/dev/null
      echo "applied: updated ruleset '${RULESET_NAME}' (id ${found})"
    else
      gh api --method POST "$REPO_PATH/rulesets" \
        -H "Content-Type: application/json" --input "$DATA_FILE" >/dev/null
      echo "applied: created ruleset '${RULESET_NAME}'"
    fi
    ;;
  remove)
    if [ -n "$found" ]; then
      gh api --method DELETE "$REPO_PATH/rulesets/$found" >/dev/null
      echo "removed: ruleset '${RULESET_NAME}' (id ${found})"
    else
      echo "removed: nothing to remove (no ruleset named '${RULESET_NAME}')"
    fi
    ;;
esac
