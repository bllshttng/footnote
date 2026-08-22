#!/usr/bin/env bash
# tests/ci/test_smoke_duration_report.sh
#
# Behavior guard over scripts/ci/smoke-duration-report.sh, the thing that
# finally READS a smoke shard's duration instead of only printing it.
#
# Three properties matter more than the arithmetic:
#
# 1. The existing `<shard>-duration-seconds=` line must stay byte-identical.
#    It is the documented instrument and anything already grepping it has to
#    keep matching.
# 2. The reporter lives in an EXIT trap, so it must never be the thing that
#    reddens a green suite. It exits 0 on every input, including garbage.
# 3. The two shards run in PARALLEL, so wall clock is the MAX of the two and
#    never the total. Their sum is 36m45s against a 32m unsharded baseline, so
#    a summing consumer would report the suite got SLOWER when it got 41
#    percent faster. The script therefore takes exactly one duration and
#    refuses a second, and this file asserts that refusal so a later edge
#    cannot quietly enable the sum.

set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

SCRIPT="scripts/ci/smoke-duration-report.sh"
[[ -f "$SCRIPT" ]] || { echo "FAIL: $SCRIPT missing"; exit 1; }

fails=0
ok()   { echo "  ok: $1"; }
bad()  { echo "  FAIL: $1"; fails=$((fails + 1)); }
check() { if [[ -n "$1" ]]; then ok "$2"; else bad "$3"; fi }

# --- the key line is unchanged --------------------------------------------
# Compared against a literal printf of the OLD emitter, not against a
# restatement of the new one: a test that reformats both sides in the same way
# would pass through any change to the format.
expected="$(printf 'smoke-rest-duration-seconds=%s\n' 1063)"
actual="$(bash "$SCRIPT" smoke-rest 1063 30 2>/dev/null | grep '^smoke-rest-duration-seconds=')"
check "$([[ "$actual" == "$expected" ]] && echo 1)" \
      "the duration key line is byte-identical to the old printf" \
      "key line changed: got '$actual', want '$expected'"

# --- a healthy run reports ok and stays quiet ------------------------------
out="$(bash "$SCRIPT" smoke-rest 1063 30 2>&1)"
check "$(grep -q 'verdict=ok' <<<"$out" && echo 1)" \
      "a run well under the cap reports verdict=ok" \
      "no verdict=ok for 1063s against a 30m cap"
check "$(grep -q '::warning' <<<"$out" || echo 1)" \
      "a healthy run emits no warning annotation" \
      "a healthy run emitted a ::warning - the threshold would be permanent noise"
check "$(grep -q 'shard=smoke-rest' <<<"$out" && echo 1)" \
      "the verdict line names the shard it measured" \
      "verdict line does not name its shard"

# --- an approaching run warns where a reader will actually see it ----------
summary="$(mktemp)"
out="$(GITHUB_STEP_SUMMARY="$summary" bash "$SCRIPT" smoke-pytest 1500 30 2>&1)"
check "$(grep -q 'verdict=approaching' <<<"$out" && echo 1)" \
      "a run above the fraction reports verdict=approaching" \
      "1500s against a 30m cap did not report approaching"
check "$(grep -q '::warning' <<<"$out" && echo 1)" \
      "an approaching run emits a ::warning annotation" \
      "no ::warning annotation for an approaching run"
check "$([[ -s "$summary" ]] && echo 1)" \
      "an approaching run writes to GITHUB_STEP_SUMMARY" \
      "GITHUB_STEP_SUMMARY was set and writable but nothing was written"
rm -f "$summary"

# An unset GITHUB_STEP_SUMMARY is the local case and must not error.
out="$(env -u GITHUB_STEP_SUMMARY bash "$SCRIPT" smoke-pytest 1500 30 2>&1)"; rc=$?
check "$([[ $rc -eq 0 ]] && echo 1)" \
      "an approaching run with no GITHUB_STEP_SUMMARY still exits 0" \
      "exited $rc with GITHUB_STEP_SUMMARY unset"

# --- the verdict word is always present -----------------------------------
# A missing line must be distinguishable from a pass. Absence of a warning is
# not evidence the checker ran; the verdict word is.
for secs in 1 1063 1500 99999; do
  out="$(bash "$SCRIPT" smoke-rest "$secs" 30 2>&1)"
  check "$(grep -qE 'verdict=(ok|approaching)' <<<"$out" && echo 1)" \
        "a verdict word is printed for ${secs}s" \
        "no verdict word for ${secs}s - absence would read as a pass"
done

# --- two durations are refused, never summed ------------------------------
out="$(bash "$SCRIPT" smoke-pytest 1142 30 1063 2>&1)"; rc=$?
check "$(grep -qi 'parallel' <<<"$out" && echo 1)" \
      "a fourth argument is refused with a diagnostic naming the parallel trap" \
      "a fourth argument was accepted or refused without naming why"
check "$(grep -q '2205' <<<"$out" || echo 1)" \
      "the refusal prints no summed number" \
      "the output contains 1142+1063=2205 - the sum this script exists to prevent"
check "$([[ $rc -eq 0 ]] && echo 1)" \
      "even the refusal exits 0 (it runs inside an EXIT trap)" \
      "the refusal exited $rc and could redden a green suite"

# --- garbage input cannot redden a green suite ----------------------------
for arg in "not-a-number" "" "-5"; do
  out="$(bash "$SCRIPT" smoke-rest "$arg" 30 2>&1)"; rc=$?
  check "$([[ $rc -eq 0 ]] && echo 1)" \
        "a bad duration (${arg:-empty}) still exits 0" \
        "a bad duration (${arg:-empty}) exited $rc"
  check "$(grep -qi 'smoke-duration' <<<"$out" && echo 1)" \
        "a bad duration (${arg:-empty}) still says something" \
        "a bad duration (${arg:-empty}) was silent"
done

# --- the reporter cannot mask the step's real exit status -----------------
# The whole point of the trap: a failing suite stays red.
bash -c 'set -e; trap "bash scripts/ci/smoke-duration-report.sh smoke-rest 10 30 || true" EXIT; false' >/dev/null 2>&1
rc=$?
check "$([[ $rc -eq 1 ]] && echo 1)" \
      "a failing command under the reporter's trap still exits 1" \
      "exit status was $rc, not 1 - the reporter masked a failure"

bash -c 'set -e; trap "bash scripts/ci/smoke-duration-report.sh smoke-rest 10 30 || true" EXIT; true' >/dev/null 2>&1
rc=$?
check "$([[ $rc -eq 0 ]] && echo 1)" \
      "a passing command under the reporter's trap still exits 0" \
      "exit status was $rc, not 0 - the reporter reddened a green run"


# --- the cap in the trap must match the job's real ceiling ----------------
# The cap now lives in two places per shard: `timeout-minutes` on the job, and
# the third argument to the reporter, because GitHub does not expose
# timeout-minutes to a step. A raise applied to only one would move the warning
# threshold away from the real ceiling and nothing would say so.
echo ""
echo "== cap agreement between timeout-minutes and the reporter argument =="
python3 - <<'PY'
import re
import sys

try:
    import yaml
except ImportError:
    sys.exit("SKIP-AS-FAIL: PyYAML not installed (pip install pyyaml); "
             "this guard must not silently pass")

jobs = yaml.safe_load(open(".github/workflows/cli-ci.yml"))["jobs"]
needs = jobs["smoke"].get("needs") or []
if isinstance(needs, str):
    needs = [needs]

bad = 0
if not needs:
    print("  FAIL: the smoke gate needs no shard jobs; nothing to check")
    bad += 1

checked = 0
for name in needs:
    job = jobs.get(name, {})
    run = "\n".join(st.get("run", "") for st in job.get("steps", []))
    # The cap is the integer immediately before the trap's `|| true`; the
    # arithmetic expansion in between rules out a positional match.
    m = re.search(r"smoke-duration-report\.sh\s+\S+\s+.*?(\d+)\s*\|\|\s*true", run)
    if not m:
        print(f"  FAIL: {name} never calls the duration reporter")
        bad += 1
        continue
    arg_cap, job_cap = int(m.group(1)), job.get("timeout-minutes")
    if job_cap != arg_cap:
        print(f"  FAIL: {name} timeout-minutes={job_cap} but reporter cap={arg_cap}")
        bad += 1
    else:
        print(f"  ok: {name} cap agrees ({job_cap}m in both places)")
        checked += 1
    # A trap that can redden a green suite defeats the reporter's contract.
    if "|| true" not in run:
        print(f"  FAIL: {name} calls the reporter without '|| true'")
        bad += 1

# Assert a positive count, not merely the absence of failures: an empty needs
# list or a renamed job would otherwise report a clean pass having read nothing.
if checked == 0:
    print("  FAIL: no shard job was actually checked")
    bad += 1

sys.exit(1 if bad else 0)
PY
if [[ $? -ne 0 ]]; then
    fails=$((fails + 1))
fi
echo ""
if [[ $fails -eq 0 ]]; then
  echo "test_smoke_duration_report: ALL PASS"
else
  echo "test_smoke_duration_report: FAILED ($fails)"
fi
exit $((fails > 0))
