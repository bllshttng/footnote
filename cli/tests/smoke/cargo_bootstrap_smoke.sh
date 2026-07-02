#!/usr/bin/env bash
# ab-4040eee8 US6: clean-machine smoke for the cargo install channel.
#
# Given a built, binary-complete platform wheel, exercise the cargo front door
# end to end:
#
#   1. `cargo install --path crates/fno` lands the `fno` shim.
#   2. The first `fno` run self-bootstraps: it provisions the wheel via uv
#      (from FNO_BOOTSTRAP_WHEEL - the local wheel, since the by-name PyPI
#      publish is the launch gate, Open Q3) and runs the command (AC6-HP).
#   3. All three `fno-agents*` binaries land on the uv tool bin (US2/AC2-HP):
#      proof that uv tool install surfaces the wheel's shared_scripts on PATH.
#   4. A second run forwards via the sentinel with no re-provision (AC4-HP).
#
# Every verb runs from a cwd OUTSIDE any git repo (AC6-EDGE). The whole run is
# isolated into a temp HOME + UV_TOOL_DIR + cargo root, so it never mutates the
# runner's real tools and leaves no /tmp leak. Prints pass/fail per check and
# exits non-zero on any miss (AC6-ERR/UI), so a broken bootstrap fails the
# release rather than going silently green.
#
# Usage: cargo_bootstrap_smoke.sh <path-to-binary-complete-wheel>
#
# Note: the by-name `cargo install fno` + `uv tool install fno` variant can only
# go green AFTER the PyPI + crates.io publishes (foundation launch #jc); that is
# the launch-gated required check. This smoke proves the mechanism now.
set -uo pipefail

WHEEL="${1:?usage: cargo_bootstrap_smoke.sh <binary-complete-wheel>}"
WHEEL="$(cd "$(dirname "$WHEEL")" && pwd)/$(basename "$WHEEL")"
[ -f "$WHEEL" ] || { echo "FAIL[input] wheel not found: $WHEEL"; exit 1; }

# Repo root from this script's location (cli/tests/smoke/ -> repo root).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
CRATE_DIR="$REPO_ROOT/crates/fno"
[ -f "$CRATE_DIR/Cargo.toml" ] || { echo "FAIL[input] bootstrapper crate not found at $CRATE_DIR"; exit 1; }

command -v cargo >/dev/null 2>&1 || { echo "FAIL[env] cargo not on PATH (the cargo channel needs a Rust toolchain)"; exit 1; }

fail=0
pass() { printf 'PASS[%s] %s\n' "$1" "$2"; }
miss() { printf 'FAIL[%s] %s\n' "$1" "$2"; fail=1; }

# One base temp dir for the cargo root, isolated uv tool dir/bin, sentinel cache,
# pristine HOME, and a repo-less work dir, all cleaned on exit.
BASE_TMP="$(mktemp -d)"
BASE_TMP="$(cd "$BASE_TMP" && pwd -P)"
trap 'rm -rf "$BASE_TMP"' EXIT
CARGO_ROOT="$BASE_TMP/cargo"
UV_TOOLS="$BASE_TMP/uv-tools"
UV_BIN="$BASE_TMP/uv-bin"
CACHE="$BASE_TMP/cache"
WORK="$BASE_TMP/work"
mkdir -p "$CARGO_ROOT" "$UV_TOOLS" "$UV_BIN" "$CACHE" "$WORK"

# --- install the shim via the cargo front door ---
if ! cargo install --path "$CRATE_DIR" --root "$CARGO_ROOT" --quiet 2>&1; then
  echo "FAIL[install] cargo install --path crates/fno failed"
  exit 1
fi
SHIM="$CARGO_ROOT/bin/fno"
[ -x "$SHIM" ] || { echo "FAIL[install] cargo did not produce $SHIM"; exit 1; }

# Isolated provisioning env: the shim's first run installs the wheel HERE, never
# into the runner's real ~/.local. FNO_BOOTSTRAP_WHEEL is the local wheel (the
# by-name PyPI path is the launch gate). HOME pristine so nothing resolves back
# to a real install. cwd is repo-less (AC6-EDGE).
cd "$WORK" || { echo "FAIL[env] cd to $WORK failed"; exit 1; }
export UV_TOOL_DIR="$UV_TOOLS" UV_TOOL_BIN_DIR="$UV_BIN" \
       XDG_CACHE_HOME="$CACHE" HOME="$BASE_TMP/home" \
       FNO_BOOTSTRAP_WHEEL="$WHEEL"
mkdir -p "$HOME"

run_capture() { OUT="$("$@" 2>&1)"; RC=$?; }

# --- check 1: first-run provision + version (AC6-HP / AC1-HP) ---
run_capture "$SHIM" --version
if [ "$RC" -eq 0 ] && printf '%s' "$OUT" | grep -qE 'fno[[:space:]]+[0-9]+\.[0-9]+'; then
  pass "provision" "first run provisioned + ran fno --version (rc=0)"
else
  miss "provision" "rc=$RC out: $(printf '%s' "$OUT" | tail -1)"
fi

# --- check 2: the verify-ours verdict was reported (AC3-UI) ---
if printf '%s' "$OUT" | grep -qi 'verified fno'; then
  pass "verify-reported" "identity verdict logged before exec"
else
  miss "verify-reported" "no 'verified fno' line on first run"
fi

# --- check 3: all three binaries on the uv tool bin (US2 / AC2-HP / AC6-UI) ---
for b in fno-agents fno-agents-daemon fno-agents-worker; do
  if [ -x "$UV_BIN/$b" ]; then pass "binary:$b" "present on the uv tool bin"
  else miss "binary:$b" "absent (uv tool install must surface the wheel's shared_scripts)"; fi
done

# --- check 4: sentinel fast-path on re-run, no re-provision (AC4-HP) ---
run_capture "$SHIM" --version
if [ "$RC" -eq 0 ] && ! printf '%s' "$OUT" | grep -qi 'first run'; then
  pass "idempotent" "second run forwarded via sentinel, no re-provision (rc=0)"
else
  miss "idempotent" "rc=$RC unexpectedly re-provisioned or failed: $(printf '%s' "$OUT" | head -1)"
fi

echo "---"
if [ "$fail" -ne 0 ]; then echo "cargo-bootstrap smoke: FAILED"; exit 1; fi
echo "cargo-bootstrap smoke: all checks passed"
