#!/usr/bin/env bash
# ab-f49b54c1 US6: clean-machine smoke for the fno.sh curl-one-liner channel.
#
# Given a built, binary-complete platform wheel, exercise the fno.sh front door
# end to end against the REPO COPY of the installer (the deployed CF script is
# byte-identical to it, AC6-EDGE; the by-name PyPI publish is the launch gate):
#
#   1. `sh scripts/install/fno.sh` provisions the wheel via uv (from
#      FNO_INSTALL_WHEEL - the local wheel, since by-name PyPI is the launch
#      gate, Open Q3) and reports success (AC1-HP/AC6-HP).
#   2. The verify-ours identity verdict is logged before success (AC5-UI).
#   3. All three fno-agents* binaries land on the uv tool bin (US3/AC3-HP):
#      proof that `uv tool install` surfaces the wheel's shared_scripts on PATH.
#   4. The provisioned `fno --version` runs from the tool bin (AC3-ERR: no 127).
#   5. A second run is a clean no-op - no re-provision (US4/AC4-HP).
#   6. The uv-tool-bin-not-on-PATH hint is surfaced (AC3-UI).
#
# Every verb runs from a cwd OUTSIDE any git repo (AC6-EDGE). The whole run is
# isolated into a temp HOME + UV_TOOL_DIR + UV_TOOL_BIN_DIR, so it never mutates
# the runner's real tools and leaves no /tmp leak. Per-check pass/fail; exits
# non-zero on any miss (AC6-ERR/UI) so a broken installer fails the release
# rather than going silently green.
#
# Scope note: this exercises the uv-PRESENT provisioning path (uv on the runner,
# or the well-known fallback path). The truly-no-uv Astral-chain (US2 "install
# uv when absent") needs network to astral.sh and a uv-less image; that is the
# full no-prereq container case, covered separately. This smoke proves the
# provision + binaries + verify + idempotency mechanism now.
#
# Usage: fno_sh_smoke.sh <path-to-binary-complete-wheel>
set -uo pipefail

WHEEL="${1:?usage: fno_sh_smoke.sh <binary-complete-wheel>}"
WHEEL="$(cd "$(dirname "$WHEEL")" && pwd)/$(basename "$WHEEL")"
[ -f "$WHEEL" ] || { echo "FAIL[input] wheel not found: $WHEEL"; exit 1; }

# Repo root from this script's location (cli/tests/smoke/ -> repo root).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
INSTALLER="$REPO_ROOT/scripts/install/fno.sh"
[ -f "$INSTALLER" ] || { echo "FAIL[input] installer not found at $INSTALLER"; exit 1; }

command -v uv >/dev/null 2>&1 || { echo "FAIL[env] uv not on PATH (this smoke exercises the uv-present provision path)"; exit 1; }

fail=0
pass() { printf 'PASS[%s] %s\n' "$1" "$2"; }
miss() { printf 'FAIL[%s] %s\n' "$1" "$2"; fail=1; }

# One base temp dir for the isolated uv tool dir/bin, pristine HOME, and a
# repo-less work dir, all cleaned on exit.
BASE_TMP="$(mktemp -d)"
BASE_TMP="$(cd "$BASE_TMP" && pwd -P)"
trap 'rm -rf "$BASE_TMP"' EXIT
UV_TOOLS="$BASE_TMP/uv-tools"
UV_BIN="$BASE_TMP/uv-bin"
CACHE="$BASE_TMP/cache"
WORK="$BASE_TMP/work"
mkdir -p "$UV_TOOLS" "$UV_BIN" "$CACHE" "$WORK" "$BASE_TMP/home"

# Isolated provisioning env: the installer lands the wheel HERE, never into the
# runner's real ~/.local. FNO_INSTALL_WHEEL is the local wheel (the by-name PyPI
# path is the launch gate). HOME pristine so nothing resolves back to a real
# install. cwd is repo-less (AC6-EDGE).
cd "$WORK" || { echo "FAIL[env] cd to $WORK failed"; exit 1; }
export UV_TOOL_DIR="$UV_TOOLS" UV_TOOL_BIN_DIR="$UV_BIN" \
       XDG_CACHE_HOME="$CACHE" HOME="$BASE_TMP/home" \
       FNO_INSTALL_WHEEL="$WHEEL"

run_capture() { OUT="$("$@" 2>&1)"; RC=$?; }

# --- check 1: first-run provision succeeds (AC1-HP / AC6-HP) ---
run_capture sh "$INSTALLER"
if [ "$RC" -eq 0 ]; then
  pass "provision" "fno.sh provisioned the wheel (rc=0)"
else
  miss "provision" "rc=$RC out: $(printf '%s' "$OUT" | tail -2)"
fi

# --- check 2: the verify-ours verdict was reported (AC5-UI) ---
if printf '%s' "$OUT" | grep -qi 'verified fno'; then
  pass "verify-reported" "identity verdict logged before success"
else
  miss "verify-reported" "no 'verified fno' line on first run"
fi

# --- check 3: the uv-bin-not-on-PATH hint was surfaced (AC3-UI) ---
if printf '%s' "$OUT" | grep -qiE 'not on your PATH|export PATH'; then
  pass "path-hint" "PATH-fix hint surfaced (tool bin not yet on PATH)"
else
  miss "path-hint" "no PATH hint though the tool bin is off PATH"
fi

# --- check 4: all three binaries on the uv tool bin (US3 / AC3-HP / AC6-UI) ---
for b in fno-agents fno-agents-daemon fno-agents-worker; do
  if [ -x "$UV_BIN/$b" ]; then pass "binary:$b" "present on the uv tool bin"
  else miss "binary:$b" "absent (uv tool install must surface the wheel's shared_scripts)"; fi
done

# --- check 5: the provisioned fno --version runs (AC3-ERR: no 127) ---
FNO_BIN="$UV_BIN/fno"
if [ -x "$FNO_BIN" ]; then
  run_capture "$FNO_BIN" --version
  if [ "$RC" -eq 0 ] && printf '%s' "$OUT" | grep -qE 'fno[[:space:]]+[0-9]+\.[0-9]+'; then
    pass "version" "fno --version runs from the tool bin (rc=0)"
  else
    miss "version" "rc=$RC out: $(printf '%s' "$OUT" | tail -1)"
  fi
else
  miss "version" "fno not found on the tool bin at $FNO_BIN"
fi

# --- check 6: re-run is a clean no-op, no re-provision (US4 / AC4-HP) ---
# The real re-run is the bare `curl | sh` (no override), so drop FNO_INSTALL_WHEEL
# here: the installer must detect the already-provisioned, verified fno and no-op
# WITHOUT touching uv/PyPI. An explicit override would (by design) force a
# reinstall, which is not the idempotency path under test.
run_capture env -u FNO_INSTALL_WHEEL sh "$INSTALLER"
if [ "$RC" -eq 0 ] && printf '%s' "$OUT" | grep -qi 'already installed' \
   && ! printf '%s' "$OUT" | grep -qi 'provisioning the fno CLI'; then
  pass "idempotent" "second run detected the install and did not re-provision (rc=0)"
else
  miss "idempotent" "rc=$RC unexpectedly re-provisioned or failed: $(printf '%s' "$OUT" | tail -2)"
fi

echo "---"
if [ "$fail" -ne 0 ]; then echo "fno.sh smoke: FAILED"; exit 1; fi
echo "fno.sh smoke: all checks passed"
