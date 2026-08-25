#!/usr/bin/env bash
# The preflight receipt scope is written by hand in three files, and it has
# already drifted once, silently.
#
# scripts/ci/preflight.sh declares the legs a full run must record;
# cli/src/fno/pr/_preflight.py and crates/fno-agents/src/verify_evidence.rs
# each decide whether a receipt is trustworthy by comparing against their own
# copy. The check is "every required leg present, nothing unknown added", so a
# rename on one side fails it in BOTH directions: the receipt is DISCARDED
# rather than rejected, and with preflight.required = true a green preflight can
# never clear the gate. preflight.sh itself carries the array twice, so all
# copies are compared, not the first one a search happens to land on.
#
# The integration-target lists this also used to guard are gone: both callers
# now pass `--test '*'`, and a glob cannot drop a file the way the hand-written
# enumeration dropped three.
#
# The check reports the size of every set it compared. A count is the positive
# marker that the parse found anything: a regex that matches nothing reports
# "0 legs" rather than passing quietly.
set -uo pipefail

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || {
    echo "parity=unavailable reason=not-a-git-checkout" >&2
    exit 2
}
cd "$repo_root" || exit 2

python3 - <<'PY'
import re
import sys

fail = 0


def die(msg):
    global fail
    print(f"  FAIL: {msg}")
    fail = 1


preflight = open("scripts/ci/preflight.sh").read()


print("== the preflight receipt scope reads the same in all three files ==")
# EVERY copy, not the first one a search happens to land on. preflight.sh
# carries the array twice - the void path and the verdict-bearing path - and
# reading only the first left a rename in the second one invisible while every
# receipt it produced went untrusted. That is the exact bug this guard exists
# to catch, so the guard must not contain it.
copies = re.findall(r"REQUIRED_SCOPE_NAMES=\(([^)]*)\)", preflight)
if not copies:
    die("REQUIRED_SCOPE_NAMES not found in scripts/ci/preflight.sh")
    sys.exit(fail)
sets = [set(c.split()) for c in copies]
declared = sets[0]
copies_agree = True
for i, other in enumerate(sets[1:], 2):
    if other != declared:
        die(f"preflight.sh REQUIRED_SCOPE_NAMES copy {i} disagrees with copy 1: "
            f"missing {sorted(declared - other) or 'nothing'}, "
            f"extra {sorted(other - declared) or 'nothing'}")
        copies_agree = False
if copies_agree:
    print(f"  ok: {len(copies)} REQUIRED_SCOPE_NAMES copies in preflight.sh, identical")

py = open("cli/src/fno/pr/_preflight.py").read()
rs = open("crates/fno-agents/src/verify_evidence.rs").read()


def literals(text, pattern):
    m = re.search(pattern, text, re.S)
    return set(re.findall(r'"([^"]+)"', m.group(1))) if m else set()


PY_FILE = "cli/src/fno/pr/_preflight.py"
RS_FILE = "crates/fno-agents/src/verify_evidence.rs"

# The OPTIONAL set is checked too, and it is not a lesser case. Both eligibility
# tests reject an UNKNOWN leg exactly as hard as they reject a missing required
# one, so a leg added to one side's optional list and not the other's still
# untrusts every receipt - and the historical break this guard's header cites,
# `tracker-gates:fno`, was an optional leg.
groups = [
    ("required", declared,
     literals(py, r"_PREFLIGHT_BASE_SCOPE = frozenset\(\s*\{(.*?)\}"),
     literals(rs, r"const PREFLIGHT_BASE_SCOPE: \[&str; \d+\] = \[(.*?)\];")),
    ("optional", None,
     literals(py, r"_PREFLIGHT_OPTIONAL_SCOPE = frozenset\(\s*\{(.*?)\}"),
     literals(rs, r"const PREFLIGHT_OPTIONAL_SCOPE: \[&str; \d+\] = \[(.*?)\];")),
]

if not declared:
    die("read 0 required scope names from preflight.sh")

for kind, from_sh, py_names, rs_names in groups:
    if not py_names:
        die(f"{PY_FILE}: could not read the {kind} scope list")
        continue
    if not rs_names:
        die(f"{RS_FILE}: could not read the {kind} scope list")
        continue
    # preflight.sh declares the required set only; the optional legs are named
    # at their record_leg call sites, so the two language copies are compared
    # against each other there.
    pairs = [(PY_FILE, py_names), (RS_FILE, rs_names)] if from_sh is not None else []
    reference = from_sh if from_sh is not None else py_names
    ref_label = "preflight.sh" if from_sh is not None else PY_FILE
    if not pairs:
        pairs = [(RS_FILE, rs_names)]
    ok = True
    for label, names in pairs:
        if names != reference:
            die(f"{label} {kind} scope disagrees with {ref_label}: "
                f"missing {sorted(reference - names) or 'nothing'}, "
                f"extra {sorted(names - reference) or 'nothing'}")
            ok = False
    if ok:
        print(f"  ok: {len(reference)} {kind} legs, identical across the files that carry them")

sys.exit(fail)
PY
rc=$?
echo ""
if ((rc == 0)); then
    echo "test_preflight_parity: ALL PASS"
else
    echo "test_preflight_parity: FAILED"
fi
exit $rc
