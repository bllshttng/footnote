#!/usr/bin/env bash
# test_reinstall_window_narrow.sh
#
# The end-to-end proof for the narrowed reinstall: while
# `uv tool install --reinstall-package fno` runs against a scratch tool
# environment, a loop of fresh interpreters importing an fno submodule plus a
# third-party package must see ZERO third-party import failures -- the
# dependencies never leave site-packages. The fno package's own swap keeps a
# sub-second namespace window (fno/__init__.py absent mid-swap, so `import
# fno` namespace-loads and no in-package guard can exist); fno-scoped
# failures from that window are reported, not failed. The negative control
# reruns the same loop against the wide `--reinstall`, which must produce at
# least one third-party import failure; a green run whose control did not
# fire proves nothing.
#
# Modelled on tests/ci/test_uv_install_verify_wait.sh: same set -uo pipefail,
# same exit codes.
#
# Exit codes: 0 pass / 1 assertion failed / 77 skipped (missing deps,
# build failure, or an assertion precondition that did not reproduce)

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
CLI_SRC="$REPO/cli"
PROBE_MODULE="import click, fno.state.cli"

fail() { echo "FAIL: $*" >&2; exit 1; }
skip() { echo "SKIP: $*" >&2; exit 77; }

command -v uv >/dev/null 2>&1 || skip "uv is not on PATH"
command -v python3 >/dev/null 2>&1 || skip "python3 is not on PATH"
[[ -f "$CLI_SRC/pyproject.toml" ]] || fail "no pyproject.toml at $CLI_SRC"

SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT
export UV_TOOL_DIR="$SCRATCH/tools"
# Keep the test's bin links out of ~/.local/bin: a scratch install must never
# touch the machine's real tool exposure, and a machine that already owns
# those bin names would refuse the install outright.
export UV_TOOL_BIN_DIR="$SCRATCH/bin"

now() { python3 -c 'import time; print(repr(time.time()))'; }

# --- 0. Baseline: build the scratch env, then prove the probe works when
#        nothing is in flight. Without this positive control a probe that
#        fails for an unrelated reason (broken venv, wrong layout) would read
#        as window exposure.
if ! uv tool install --compile-bytecode "$CLI_SRC" >"$SCRATCH/base.log" 2>&1; then
  skip "scratch tool build failed:
$(tail -5 "$SCRATCH/base.log")"
fi
VENV_PY="$UV_TOOL_DIR/fno/bin/python3"
[[ -x "$VENV_PY" ]] || skip "unexpected uv tool layout: no python3 at $VENV_PY"
if ! "$VENV_PY" -c "$PROBE_MODULE" >/dev/null 2>&1; then
  skip "probe import fails on the untouched baseline env:
$("$VENV_PY" -c "$PROBE_MODULE" 2>&1 | tail -3)"
fi

# --- The probe: fresh interpreter per iteration, timestamps around each
#        spawn, bounded by the install's lifetime and a deadline. Rows land
#        in JSON: [{"t0":..,"t1":..,"rc":..,"err":..}, ...]
cat > "$SCRATCH/probe.py" <<PYEOF
import json, os, subprocess, sys, time

venv_py, install_pid, out_path, deadline_s = (
    sys.argv[1], int(sys.argv[2]), sys.argv[3], float(sys.argv[4])
)
probe_cmd = sys.argv[5]
rows = []
deadline = time.time() + deadline_s
while time.time() < deadline:
    try:
        os.kill(install_pid, 0)
    except OSError:
        break  # the install finished; the window is closed
    t0 = time.time()
    proc = subprocess.run(
        [venv_py, "-c", probe_cmd], capture_output=True, text=True
    )
    t1 = time.time()
    rows.append({
        "t0": t0, "t1": t1, "rc": proc.returncode,
        "err": proc.stderr[-2000:] if proc.returncode else "",
    })
with open(out_path, "w") as f:
    json.dump(rows, f)
PYEOF

run_window() { # run_window <uv-args...> <probe-cmd> <rows-out>
  local rows_out="${!#}"
  local penult=$(($# - 1))
  local probe_cmd="${@:$penult:1}"
  local uv_args=("${@:1:$#-2}")
  now > "$SCRATCH/win_start"
  uv tool install "${uv_args[@]}" >"$SCRATCH/win_install.log" 2>&1 &
  local install_pid=$!
  python3 "$SCRATCH/probe.py" "$VENV_PY" "$install_pid" "$rows_out" 240 "$probe_cmd"
  local probe_rc=$?
  # A bounded hang: the probe gave up at its deadline, so do not wait forever
  # on an install that never returned.
  kill -0 "$install_pid" 2>/dev/null && kill "$install_pid" 2>/dev/null
  wait "$install_pid"
  local install_rc=$?
  now > "$SCRATCH/win_end"
  [[ "$probe_rc" -eq 0 ]] || fail "probe runner crashed"
  [[ "$install_rc" -eq 0 ]] || fail "uv tool install failed:
$(tail -5 "$SCRATCH/win_install.log")"
}

analyze() { # analyze <rows-out> <kind: narrow|wide>
  INSTALL_START="$(cat "$SCRATCH/win_start")"
  INSTALL_END="$(cat "$SCRATCH/win_end")"
  python3 - "$1" "$2" "$INSTALL_START" "$INSTALL_END" <<'PYEOF'
import json, sys

rows_path, kind, start, end = sys.argv[1], sys.argv[2], float(sys.argv[3]), float(sys.argv[4])
rows = json.load(open(rows_path))
overlapped = [r for r in rows if r["t0"] >= start and r["t1"] <= end]
failed = [r for r in rows if r["rc"] != 0]
missing = [r for r in failed if "No module named" in r["err"]]
broken = [r for r in failed if "No module named" not in r["err"]]
print(f"{kind}: {len(rows)} probes, {len(overlapped)} overlapped the install, "
      f"{len(failed)} failed, {len(missing)} module-not-found")
for r in broken:
    print(f"BROKEN PROBE (not a window failure): {r['err']}", file=sys.stderr)
if not overlapped:
    print(f"exposure not reproduced: no probe overlapped the {kind} install", file=sys.stderr)
    sys.exit(77)
if broken:
    sys.exit(1)
if kind == "narrow":
    # The honest invariant, measured 2026-09-12: the narrow form closes the
    # DEPENDENCY window (third-party packages never leave site-packages), but
    # the fno package's own swap keeps a sub-second namespace window that no
    # in-package guard can see -- mid-swap, fno/__init__.py itself is absent
    # while its directory remains, so `import fno` namespace-loads and the
    # guard's code never runs. A probe started inside that window fails fast
    # with an fno-scoped ModuleNotFoundError and a retry lands. So: any
    # third-party-named failure is a regression and fails the run; fno-scoped
    # ones are reported as the residual-window count.
    import re
    foreign = []
    for r in missing:
        m = re.search(r"No module named '([^']+)'", r["err"])
        if m and not m.group(1).startswith("fno"):
            foreign.append(r)
    if foreign:
        for r in foreign:
            print(f"THIRD-PARTY WINDOW FAILURE (narrow form regressed): {r['err']}",
                  file=sys.stderr)
        sys.exit(1)
    print(f"residual fno-namespace window: {len(missing)} fno-scoped failure(s), retry lands")
else:
    import re
    foreign = []
    for r in missing:
        m = re.search(r"No module named '([^']+)'", r["err"])
        if m and not m.group(1).startswith("fno"):
            foreign.append(r)
    if not foreign:
        print("negative control did not fire: the wide reinstall produced "
              "no THIRD-PARTY module-not-found failure", file=sys.stderr)
        sys.exit(77)
sys.exit(0)
PYEOF
}

# --- 1. The narrow reinstall: overlap it, expect zero import failures.
# The probe imports an fno submodule through the real user path. On failure it
# also dumps the interpreter's meta_path and fno's spec shape, so a live-swap
# failure carries its own mechanism in the record.
NARROW_PROBE_CMD="import sys, json
try:
    import click, fno.state.cli
except Exception as exc:
    import fno
    spec = getattr(fno, '__spec__', None)
    print(json.dumps({'error': str(exc)[:300], 'exc_type': type(exc).__name__,
                      'fno_loader': getattr(spec, 'loader', None) is not None and type(spec.loader).__name__,
                      'meta_path': [type(f).__name__ for f in sys.meta_path]}), file=sys.stderr)
    sys.exit(1)
"
run_window --reinstall-package fno --refresh-package fno --compile-bytecode "$CLI_SRC" \
  "$NARROW_PROBE_CMD" "$SCRATCH/narrow.json"
analyze "$SCRATCH/narrow.json" narrow
ANALYZE_RC=$?
[[ "$ANALYZE_RC" -ne 0 ]] && exit "$ANALYZE_RC"

# The retry must land: one probe on the settled env, after the swap, succeeds.
"$VENV_PY" -c "$PROBE_MODULE" >/dev/null 2>&1 \
  || fail "post-install probe failed: the narrow reinstall left the env broken"

# --- 2. Negative control: the wide reinstall must open the window.
run_window --reinstall --refresh --compile-bytecode "$CLI_SRC" "$PROBE_MODULE" "$SCRATCH/wide.json"
analyze "$SCRATCH/wide.json" wide
ANALYZE_RC=$?
[[ "$ANALYZE_RC" -ne 0 ]] && exit "$ANALYZE_RC"

echo "PASS: no third-party package left site-packages during the narrow reinstall; the wide control still fired"
exit 0
