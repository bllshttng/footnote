#!/usr/bin/env bash
# scripts/ci/check-workflow-timeout-comments.sh
#
# Refuses a workflow comment that names a duration in minutes exceeding the
# timeout-minutes of the job it sits in. A comment promising "near 20 minutes"
# above a 15-minute cap is a documented overshoot nothing checks: the reader
# who trusts the comment concludes the cap is wrong and raises it, when the
# measurement was stale and the cap was the thing to keep.
#
# Scope decisions, on purpose:
# - Comments only. The timeout-minutes lines themselves are the yardstick, not
#   offenders.
# - Job blocks only, and only job-level timeout-minutes. A file-header comment
#   names no job, and a step-level timeout has no job cap to contradict.
# - Strictly greater refuses. A comment naming the cap itself is documentation,
#   not an overshoot.
# - A job's header comment (contiguous comment lines whose next significant
#   line is the job key) belongs to that job, not to the previous one.
# - Compact "19m" is matched; "16m35s" is not (no word boundary after m), so
#   write second-precision history in plain seconds and this guard stays quiet.
# - A job whose cap is an expression (sized at runtime) is skipped: there is no
#   static number to contradict.
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

mode="${1:-check}"

python3 - "$mode" <<'PY'
import re
import sys
from pathlib import Path

mode = sys.argv[1]

FIXTURE = """\
jobs:          # file-header numbers like 'costs 4 minutes' name no job
  ok-job:
    timeout-minutes: 10
    steps:
      # runs 8 minutes warm and 8m cold
      - run: true
      # exactly 10 minutes is the cap, named as documentation
      - run: true
  over-job:
    # its own header says 12 minutes and must refuse against the cap below
    timeout-minutes: 10
    steps:
      # puts the shard back near 12 minutes
      - run: true
  dynamic-job:
    timeout-minutes: ${{ fromJSON(needs.sizer.outputs.timeout_minutes) }}
    steps:
      # this job can take 90 minutes; the cap is sized at runtime
      - run: true
"""

MINUTES = re.compile(r"\b(\d{1,4})\s*(?:minutes?|mins?)\b", re.IGNORECASE)
COMPACT = re.compile(r"\b(\d{1,3})m\b")
JOB = re.compile(r"^  ([A-Za-z0-9_-]+):\s*(?:#.*)?$")
CAP = re.compile(r"^    timeout-minutes:\s*(\d+)\s*$")


def scan(text: str, source: str) -> list[str]:
    lines = text.splitlines()
    job_at: dict[int, str] = {}
    job_cap: dict[str, int] = {}
    current = None
    jobs_start = None
    for i, line in enumerate(lines):
        if jobs_start is None:
            if re.match(r"^jobs:\s*(#.*)?$", line):
                jobs_start = i
            continue
        m = JOB.match(line)
        if m:
            current = m.group(1)
            job_at[i] = current
            continue
        cm = CAP.match(line)
        if cm and current and current not in job_cap:
            job_cap[current] = int(cm.group(1))

    # A comment belongs to the job whose key line follows it when that key is
    # the next line that is neither blank nor a comment (a job's header block
    # sits above its key, after the previous job's last line); otherwise it
    # belongs to the job already being read.
    def owner(i: int) -> tuple[str, int] | None:
        j = i + 1
        while j < len(lines) and (not lines[j].strip() or lines[j].lstrip().startswith("#")):
            j += 1
        if j < len(lines) and j in job_at:
            name = job_at[j]
        else:
            prior = [k for k in job_at if k < i]
            if not prior:
                return None
            name = job_at[max(prior)]
        cap = job_cap.get(name)
        if cap is None:
            return None
        return name, cap

    fails: list[str] = []
    for i, line in enumerate(lines):
        if jobs_start is None or "#" not in line:
            continue
        # Keep only the comment part, so a YAML value like `run: echo 99m`
        # never reads as a comment duration.
        comment = line[line.index("#"):]
        own = owner(i)
        if own is None:
            continue
        name, cap = own
        for pattern in (MINUTES, COMPACT):
            for dm in pattern.finditer(comment):
                minutes = int(dm.group(1))
                if minutes > cap:
                    fails.append(
                        f"{source}: job {name}: comment names {minutes} minutes "
                        f"above its {cap}-minute cap: {line.strip()}")
    return fails


def selftest() -> int:
    fails = scan(FIXTURE, "fixture")
    expected = 2  # over-job's header comment and its step comment; nothing else
    if len(fails) != expected or sum("over-job" in f for f in fails) != expected:
        print(f"FAIL: selftest expected exactly {expected} over-job refusals, got:",
              file=sys.stderr)
        for f in fails:
            print(f"  {f}", file=sys.stderr)
        return 1
    print(f"  ok: the selftest fixture yields exactly the {expected} over-job refusals")
    print("check-workflow-timeout-comments selftest: ALL PASS")
    return 0


if mode == "--selftest":
    sys.exit(selftest())

workflows = sorted(Path(".github/workflows").glob("*.yml"))
if not workflows:
    print("FAIL: no workflows found under .github/workflows", file=sys.stderr)
    sys.exit(1)

fails: list[str] = []
for wf in workflows:
    fails.extend(scan(wf.read_text(), str(wf)))

for f in fails:
    print(f"FAIL: {f}")
print(f"checked {len(workflows)} workflows, {len(fails)} comment overshoot(s)")
# A positive count: a run that read nothing must not look like a pass.
if not workflows:
    sys.exit(1)
sys.exit(1 if fails else 0)
PY
