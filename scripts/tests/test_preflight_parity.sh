#!/usr/bin/env bash
# Two lists in this repo are written by hand in more than one file, and both
# have already drifted silently once.
#
# 1. The integration-test targets. `cargo test --all-targets` used to name them
#    for us. The serialized legs replaced it with explicit `--test` lists in
#    scripts/ci/preflight.sh and .github/workflows/rust-ci.yml, and a target
#    absent from a list runs NOWHERE while every job stays green. Three had
#    already fallen out when this guard was written.
#
# 2. The preflight receipt scope. scripts/ci/preflight.sh declares the legs a
#    full run must record; cli/src/fno/pr/_preflight.py and
#    crates/fno-agents/src/verify_evidence.rs each decide whether a receipt is
#    trustworthy by comparing against their own copy. A rename on one side makes
#    every green preflight untrusted, and the discard is SILENT.
#
# Both checks report the size of the set they compared. A count is the positive
# marker that the parse found anything at all: a regex that matches nothing
# reports "0 targets" rather than passing quietly.
set -uo pipefail

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || {
    echo "parity=unavailable reason=not-a-git-checkout" >&2
    exit 2
}
cd "$repo_root" || exit 2

python3 - <<'PY'
import os
import re
import sys

fail = 0


def die(msg):
    global fail
    print(f"  FAIL: {msg}")
    fail = 1


preflight = open("scripts/ci/preflight.sh").read()
workflow = open(".github/workflows/rust-ci.yml").read()


def var(name):
    m = re.search(rf'{name}="([^"]*)"', preflight)
    if not m:
        die(f"{name} not found in scripts/ci/preflight.sh")
        return set()
    return set(re.findall(r"--test (\w+)", m.group(1)))


def on_disk(crate):
    d = f"crates/{crate}/tests"
    return {f[:-3] for f in os.listdir(d) if f.endswith(".rs")}


print("== every integration target is named by preflight and by rust-ci ==")
blocks = re.findall(r"run: >-\n((?:\s+cargo test\n)(?:\s+--test \w+\n)+)", workflow)
if len(blocks) != 2:
    die(f"expected 2 explicit --test blocks in rust-ci.yml, found {len(blocks)}")
    blocks = ["", ""]
workflow_sets = [set(re.findall(r"--test (\w+)", b)) for b in blocks]

for crate, pf_var in (("fno-agents", "FNO_AGENTS_INTEGRATION_TARGETS"),
                      ("fno", "FNO_INTEGRATION_TARGETS")):
    disk = on_disk(crate)
    if not disk:
        die(f"{crate}: read 0 integration targets from disk")
        continue
    listed = var(pf_var)
    # Pick the workflow block that names this crate's targets. Matching by
    # overlap, not by position, so reordering the jobs cannot silently compare
    # one crate's list against the other's.
    yml = max(workflow_sets, key=lambda s: len(s & disk))
    crate_ok = True
    for label, names in (("preflight", listed), ("rust-ci", yml)):
        missing = sorted(disk - names)
        stale = sorted(names - disk)
        if missing:
            die(f"{crate}: {label} never runs {', '.join(missing)}")
            crate_ok = False
        if stale:
            die(f"{crate}: {label} names a target that does not exist: {', '.join(stale)}")
            crate_ok = False
    if crate_ok:
        print(f"  ok: {crate} - {len(disk)} targets, all named by preflight and rust-ci")

print("")
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
m = re.search(r"_PREFLIGHT_BASE_SCOPE = frozenset\(\s*\{(.*?)\}", py, re.S)
python_scope = set(re.findall(r'"([^"]+)"', m.group(1))) if m else set()

rs = open("crates/fno-agents/src/verify_evidence.rs").read()
m = re.search(r"const PREFLIGHT_BASE_SCOPE: \[&str; \d+\] = \[(.*?)\];", rs, re.S)
rust_scope = set(re.findall(r'"([^"]+)"', m.group(1))) if m else set()

if not declared:
    die("read 0 required scope names from preflight.sh")
for label, names in (("cli/src/fno/pr/_preflight.py", python_scope),
                     ("crates/fno-agents/src/verify_evidence.rs", rust_scope)):
    if not names:
        die(f"{label}: could not read a base scope list")
        continue
    if names != declared:
        die(f"{label} disagrees with preflight.sh: "
            f"missing {sorted(declared - names) or 'nothing'}, "
            f"extra {sorted(names - declared) or 'nothing'}")
if not fail:
    print(f"  ok: {len(declared)} required legs, identical in all three files")

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
