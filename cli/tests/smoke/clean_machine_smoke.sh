#!/usr/bin/env bash
# ab-18563bcc US5/US6: clean-machine smoke for a release wheel.
#
# Given a built platform wheel, install it into a FRESH venv with cwd OUTSIDE
# any git repo, then assert the contract that distinguishes a regression from
# an intended degrade (AC5-FR):
#
#   1. binary-complete (US6, AC6-UI): the `fno` front door and all three agent
#      binaries on PATH.
#   2. the ADVERTISED ENTRYPOINT (x-538e): the first verb runs through `fno`,
#      not fno-py - `mux ls --json` proves the native mux, and `--version`
#      forwards to the ADJACENT fno-py (verified via its console-script
#      shebang), the packaged-complete path a wheel install actually takes.
#   3. internalized/folded verbs run from in-package code (US1/US2): each
#      invocation must NOT 127 / "script not found" / traceback. A `--help`
#      probe must exit 0 (proves the in-package module shipped + imports); a
#      real run (notify) may fail on its own merits but never via a missing
#      shell-out script.
#   4. clone-only verbs degrade (US3): `fno do target init` / `fno doctor bundle` exit
#      non-zero with the install-the-plugin message, never a 127 or a traceback.
#
# Prints pass/fail per check and exits non-zero on any miss. Runs on the
# release-wheels matrix (Linux/macOS) against the freshly built wheel, so a
# stale-binary or staging regression fails the release before it reaches PyPI
# (AC1-FR / AC5-FR run against the wheel's bundled binary, not an installed one).
#
# Usage: clean_machine_smoke.sh <path-to-wheel>
set -uo pipefail

# Scrubbed FIRST, before anything is installed or run: every check below reads
# the INSTALLED wheel, and an inherited PYTHONPATH pointing at cli/src puts the
# source tree ahead of it on sys.path, so this smoke would pass on a wheel that
# shipped nothing. scripts/ci/preflight.sh exports exactly that, and it already
# cost test_build.sh two false wheel-defect reports before it got this same line.
unset PYTHONPATH
[ -z "${PYTHONPATH:-}" ] || { echo "FAIL[env] PYTHONPATH survived unset (readonly?); refusing to smoke against a contaminated sys.path"; exit 1; }

WHEEL="${1:?usage: clean_machine_smoke.sh <wheel>}"
# Absolutise before we cd away to a repo-less working dir.
WHEEL="$(cd "$(dirname "$WHEEL")" && pwd)/$(basename "$WHEEL")"
[ -f "$WHEEL" ] || { echo "FAIL[input] wheel not found: $WHEEL"; exit 1; }

fail=0
pass() { printf 'PASS[%s] %s\n' "$1" "$2"; }
miss() { printf 'FAIL[%s] %s\n' "$1" "$2"; fail=1; }

# --- fresh venv, install the wheel ---
# One base temp dir for the venv, work dir, and pristine HOME, cleaned on exit
# (no /tmp leak per run). pwd -P resolves the physical path so symlinked roots
# (macOS /tmp -> /private/tmp) don't cause path-comparison surprises.
BASE_TMP="$(mktemp -d)"
BASE_TMP="$(cd "$BASE_TMP" && pwd -P)"
trap 'rm -rf "$BASE_TMP"' EXIT
VENV="$BASE_TMP/venv"
python3 -m venv "$VENV"
"$VENV/bin/pip" install --quiet --upgrade pip >/dev/null 2>&1 || true
if ! "$VENV/bin/pip" install --quiet "$WHEEL"; then
  echo "FAIL[install] pip install of $(basename "$WHEEL") failed"
  exit 1
fi
BIN="$VENV/bin"
# The wheel's Python CLI console script is `fno-py` (a component name); the
# advertised entrypoint is the Rust `fno` front door the wheel now carries
# (x-538e). Every verb below runs through it.
FNO="$BIN/fno"

# Run every verb from a dir OUTSIDE any git repo, with repo/plugin env unset, so
# only the bare-install path is exercised (AC5-EDGE: no RuntimeError from
# repo-root resolution; AC3: degrade fires because the scripts are genuinely
# absent, not because an env var points back at a checkout).
WORK="$BASE_TMP/work"
mkdir -p "$WORK"
cd "$WORK"
export PATH="$BIN:$PATH"
unset FNO_REPO_ROOT CLAUDE_PLUGIN_ROOT CODEX_PLUGIN_ROOT EVENTS_SCHEMA_PATH 2>/dev/null || true
# Pristine HOME so ~/.fno/plugin-root (a dev / CI-runner artifact) cannot resolve
# the clone-only verbs back to a real checkout - the degrade (class 3) must fire
# because the plugin is genuinely absent, making the smoke hermetic everywhere.
export HOME="$BASE_TMP/home"
mkdir -p "$HOME"

# Signals that a shell-out was NOT eliminated, or an internalized module / the
# bundled binary is missing - the failures this smoke exists to catch.
REGRESSION='command not found|No such file or directory|Traceback \(most recent call last\)|scripts/.*\.sh'

run_capture() {  # sets RC and OUT
  OUT="$("$@" 2>&1)"; RC=$?
}

# --- class 1: binary-complete (US6, AC6-UI) ---
for b in fno fno-py fno-agents fno-agents-daemon fno-agents-worker; do
  if [ -x "$BIN/$b" ]; then pass "binary:$b" "present on PATH"
  else miss "binary:$b" "absent (a release wheel must carry the front door + component + agent triad)"; fi
done

# --- class 2: the advertised entrypoint routes (x-538e) ---
# `mux ls --json` is native Rust: it answers with no Python and no
# provisioning, and an empty session list is exactly `[]` - a miss here means
# the front door itself is broken, not the Python side.
run_capture "$FNO" mux ls --json
if [ "$RC" -eq 0 ] && printf '%s' "$OUT" | grep -qF '[]'; then
  pass "mux" "fno mux ls --json answers natively (rc=0, empty list)"
else
  miss "mux" "rc=$RC out: $(printf '%s' "$OUT" | head -1)"
fi

# The Python forwarding: the front door verifies the ADJACENT fno-py (the
# console script's shebang names this venv's python) and execs it - no uv, no
# second copy of the wheel, no network. A 127 here would mean the sibling
# resolution arm is broken; a traceback, a shipped module missing.
run_capture "$FNO" --version
if [ "$RC" -eq 0 ] && printf '%s' "$OUT" | grep -qE 'fno[[:space:]]+[0-9]+\.[0-9]+'; then
  pass "frontdoor" "fno --version verified and forwarded to the adjacent CLI (rc=0)"
else
  miss "frontdoor" "rc=$RC out: $(printf '%s' "$OUT" | tail -1)"
fi

# --- class 3: internalized / folded verbs run from in-package code (AC5-FR) ---
# `--help` proves the lazy in-package module shipped and imports: a missing
# module surfaces as a non-zero exit here, not silently. The REGRESSION grep is
# NOT applied to --help output - help text legitimately documents the script a
# verb replaced (e.g. phase kill-check), which is not a runtime failure.
check_help() {  # name + argv: must exit 0
  local name="$1"; shift
  run_capture "$@"
  if [ "$RC" -eq 0 ]; then
    pass "internal:$name" "in-package module imports (rc=0)"
  else
    miss "internal:$name" "rc=$RC (module missing or not in-package): $(printf '%s' "$OUT" | head -1)"
  fi
}
check_help plan "$FNO" plan stamp --help
check_help phase-kill-check "$FNO" phase kill-check --help
check_help cost "$FNO" cost --help

# A real run: notify must fail (or succeed) on its own merits - on a headless
# CI box with no notify-send it returns a clean non-zero, never a 127/traceback.
run_capture "$FNO" notify "smoke" "clean-machine"
if printf '%s' "$OUT" | grep -qiE "$REGRESSION"; then
  miss "internal:notify" "rc=$RC shell-out/crash signal: $(printf '%s' "$OUT" | head -1)"
else
  pass "internal:notify" "ran in-package (rc=$RC, on its own merits)"
fi

# `fno cost --help` (above) proves the verb is wired; also assert all three cost
# MODULES ship + import on a bare install - _register / cost_tracker are not
# imported by the --help path, so this covers the AC2-EDGE sibling-travels case.
run_capture "$BIN/python" -c "import fno.cost._session_cost, fno.cost._register, fno.cost.cost_tracker"
if [ "$RC" -eq 0 ]; then pass "internal:cost-modules" "import OK on bare install"
else miss "internal:cost-modules" "rc=$RC: $(printf '%s' "$OUT" | head -1)"; fi

# --- class 4: clone-only verbs degrade loudly (US3, AC5-FR) ---
check_degrade() {  # name + argv: must exit !=0 with the plugin message, no 127/traceback
  local name="$1"; shift
  run_capture "$@"
  if [ "$RC" -eq 0 ]; then
    miss "degrade:$name" "exited 0 on a bare install (expected a non-zero degrade)"
  elif [ "$RC" -eq 127 ] || printf '%s' "$OUT" | grep -qiE 'Traceback \(most recent call last\)'; then
    miss "degrade:$name" "rc=$RC crashed instead of degrading: $(printf '%s' "$OUT" | head -1)"
  elif printf '%s' "$OUT" | grep -qi 'plugin'; then
    pass "degrade:$name" "non-zero with install-the-plugin message (rc=$RC)"
  else
    miss "degrade:$name" "rc=$RC non-zero but no plugin guidance: $(printf '%s' "$OUT" | head -1)"
  fi
}
check_degrade target-init "$FNO" target init --input smoke
check_degrade bundle "$FNO" bundle check

echo "---"
if [ "$fail" -ne 0 ]; then echo "clean-machine smoke: FAILED"; exit 1; fi
echo "clean-machine smoke: all checks passed"
