#!/usr/bin/env bash
# Post-install hook for the fno Claude Code plugin.
#
# Lands a complete `fno` (CLI + all three Rust binaries) on PATH. Preference
# order (US7, ab-18563bcc):
#
#   1. `uv tool install fno` BY NAME - the published PyPI platform wheel, which
#      is binary-complete in one step (no separate `fno doctor update --rust`). Guarded
#      for name-collision safety (AC7-FR): we verify the installed package is
#      OURS (its version matches this plugin's bundled cli/ source) and fall
#      back to the source build on any mismatch, so the reserved 0.0.0
#      placeholder or a squatted `fno` never runs in place of ours.
#   2. `uv tool install` from the bundled cli/ source (Python-only; the Rust
#      binaries then need a later `fno doctor update --rust`) when the PyPI wheel is
#      unavailable, not yet published, or not ours (AC7-ERR).
#   3. `pip install --user` from cli/ source.
#   4. an actionable error if neither uv nor pip is present (AC7-EDGE, unchanged).
#
# Every path logs which one it took (AC7-UI), so the user knows whether the
# daemon-backed verbs will work without a second step.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLI_DIR="$(dirname "$SCRIPT_DIR")/cli"

log() { printf "[fno postinstall] %s\n" "$*"; }
err() { printf "[fno postinstall] ERROR: %s\n" "$*" >&2; }
# Printed after a successful install so a new user knows the one configuration
# step exists (install never prompts). Optional: defaults work without it.
next_steps() { log "Next: run 'fno setup wizard' to configure (optional; defaults work)."; }

if [[ ! -f "$CLI_DIR/pyproject.toml" ]]; then
  err "expected cli/ at $CLI_DIR but pyproject.toml is missing."
  exit 1
fi

# Version this plugin's bundled source declares. A by-name PyPI install must
# match it to count as "ours" (the name-collision / placeholder guard).
src_version() {
  local v
  # sed -n ... p: print ONLY a matched version; emit nothing (not the whole
  # line) if the format is unexpected, so SRC_VERSION is either clean or empty -
  # an empty SRC_VERSION fails the guard closed (falls back to source).
  v="$(grep -E '^__version__' "$CLI_DIR/src/fno/__init__.py" 2>/dev/null \
        | head -1 | sed -n -E 's/^__version__[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p')" || true
  printf '%s' "$v"
}

# Version uv reports for an installed `fno` tool, normalized (no leading v).
uv_installed_fno_version() {
  local v
  v="$(uv tool list 2>/dev/null | awk '$1=="fno"{print $2; exit}' | sed -E 's/^v//')" || true
  printf '%s' "$v"
}

# Positive marker for a provisioned install: the console script exists AND the
# venv ships compiled bytecode. A zero exit from uv proves nothing about what
# landed, so every install path verifies before trusting itself. Deliberately
# NOT the front-door check below: a Python-only source install must also pass
# here, and it carries no Rust binaries.
uv_install_verifies() {
  local td
  td="$(NO_COLOR=1 UV_NO_COLOR=1 uv tool dir 2>/dev/null)" || return 1
  [[ -x "$td/fno/bin/fno-py" ]] || return 1
  [[ -n "$(find "$td/fno/lib" -name '*.pyc' -print -quit 2>/dev/null)" ]]
}

# The complete-install marker (x-538e): the Rust `fno` front door rides in the
# release wheel as a shared_script, so a binary-complete install lands it in
# the tool venv bin beside fno-py. Checked only on the wheel paths - a source
# build has none and is reported as Python-only instead (AC2-EDGE).
frontdoor_installed() {
  local td
  td="$(NO_COLOR=1 UV_NO_COLOR=1 uv tool dir 2>/dev/null)" || return 1
  [[ -x "$td/fno/bin/fno" ]]
}

# The install receipt (x-538e, AC2-HP/AC2-EDGE): name the actual front-door
# path and PROVE both command families answer through it - `mux ls` is native
# Rust (no Python), `--version` forwards to fno-py. A missing or shadowed
# front door is a named incomplete install with the supported repair, never a
# success inferred from uv's exit code.
verify_frontdoor() {
  local td winner out
  td="$(NO_COLOR=1 UV_NO_COLOR=1 uv tool dir 2>/dev/null)" || {
    err "uv tool dir unreadable; cannot locate the installed fno front door."; return 1; }
  local fno_bin="$td/fno/bin/fno"
  if [[ ! -x "$fno_bin" ]]; then
    err "incomplete install: no fno front door at $fno_bin. The installed wheel predates the complete payload; update to a release wheel that carries it, or run 'cargo install fno'."
    return 1
  fi
  winner="$(command -v fno 2>/dev/null || true)"
  if [[ -n "$winner" && "$winner" != "$fno_bin" ]]; then
    log "note: 'fno' on PATH resolves to $winner; the mux forwards to the same fno-py, so both work."
  fi
  log "fno front door: $fno_bin"
  if out="$("$fno_bin" mux ls 2>&1)"; then
    log "fno mux answers (mux ls rc=0)."
  else
    err "incomplete install: 'fno mux ls' failed at $fno_bin: $(printf '%s' "$out" | head -1)"
    return 1
  fi
  if out="$("$fno_bin" --version 2>&1)"; then
    log "fno --version forwarded to the Python CLI ($out)."
  else
    err "incomplete install: 'fno --version' did not forward at $fno_bin: $(printf '%s' "$out" | head -1)"
    return 1
  fi
  return 0
}

# `uv_install_verifies`, RE-CHECKED until it passes or the budget runs out.
#
# uv exits before its own artifacts settle: the console script is deleted and
# recreated across an install and is absent for ~490ms, a gap that closed only
# ~40ms before uv exited in an idle measurement (see
# docs/architecture/cli-lazy-imports.md). A verify firing the instant uv returns
# therefore races the install it is verifying and refuses a good tree.
#
# One of FOUR provisioning paths carrying this wait. The others are
# `__fno_verify_within` inside `_uv_retry_sh` in cli/src/fno/update.py,
# `install_verified_within` in crates/fno/src/bootstrap.rs, and
# `uv_install_verifies_within` in scripts/install/fno.sh. All four spend the same
# 15 * 0.2s = 3s and all four RE-CHECK rather than sleeping blind, so a genuinely
# broken install still fails with the same message.
# tests/ci/test_uv_install_verify_wait.sh asserts the four budgets match; change
# one and change all four.
uv_install_verifies_within() {
  local n=0
  while :; do
    uv_install_verifies && return 0
    n=$((n + 1))
    [[ "$n" -gt 15 ]] && return 1
    sleep 0.2
  done
}

# `uv tool install --force --compile-bytecode "$@"` with ONE retried failure:
# the ENOTEMPTY signature, uv's removal walk racing a concurrent importer's
# bytecode rewrite (docs/architecture/cli-lazy-imports.md). Any other failure
# returns non-zero immediately, uv's error already printed verbatim above.
# Bounded at three attempts; success is accepted only via uv_install_verifies.
uv_tool_install_retry() {
  local attempts=0 err
  err="$(mktemp)" || { err "cannot create a temp file to capture uv's error."; return 1; }
  while :; do
    attempts=$((attempts + 1))
    if uv tool install --force --compile-bytecode "$@" 2>"$err"; then
      rm -f "$err"
      uv_install_verifies_within || { err "uv exited 0 but the install does not verify after waiting 3s: no fno-py script or no shipped bytecode under the tool venv. Inspect it with 'uv tool dir'."; return 1; }
      return 0
    fi
    cat "$err" >&2
    if ! grep -q "Directory not empty" "$err" || ! grep -q "os error 66" "$err"; then
      rm -f "$err"
      return 1
    fi
    rm -f "$err"
    if [[ "$attempts" -ge 3 ]]; then
      err "uv tool install hit the directory race (os error 66) three times. A concurrent fno process is rewriting bytecode into the venv mid-removal. Stop fno processes and re-run."
      return 1
    fi
    sleep 1
  done
}

install_source_via_uv() {
  log "installing from $CLI_DIR via uv tool install (source build; Python-only)..."
  if uv_tool_install_retry "$CLI_DIR"; then
    log "installed Python-only fno from source. INCOMPLETE install: no 'fno' front door and no Rust binaries -"
    log "run 'fno doctor update --rust' for the daemon-backed verbs, or install a published PyPI wheel for the advertised 'fno' command."
    log "restart your shell (or source your env) to pick up PATH."
    next_steps
    return 0
  fi
  return 1
}

if command -v uv >/dev/null 2>&1; then
  SRC_VERSION="$(src_version)"

  # Idempotent: already binary-complete at our version -> nothing to do. Require
  # the front door and ALL THREE agent binaries, not just the client: a
  # same-version install missing the mux (e.g. a pre-x-538e wheel) must NOT take
  # this skip, or the advertised `fno` command stays missing - the exact
  # incomplete state this postinstall repairs.
  if [[ -n "$SRC_VERSION" && "$(uv_installed_fno_version)" == "$SRC_VERSION" ]] \
     && command -v fno >/dev/null 2>&1 \
     && command -v fno-agents >/dev/null 2>&1 \
     && command -v fno-agents-daemon >/dev/null 2>&1 \
     && command -v fno-agents-worker >/dev/null 2>&1; then
    log "fno $SRC_VERSION already installed (binary-complete); skipping."
    exit 0
  fi

  log "preferring the published PyPI wheel: uv tool install fno (by name)..."
  # stdout only is silenced: the retry wrapper's stderr (uv's verbatim error,
  # the verify refusal, the three-attempts race message) is the diagnostic
  # surface and must reach the user.
  if uv_tool_install_retry fno >/dev/null; then
    INSTALLED="$(uv_installed_fno_version)"
    if [[ -n "$SRC_VERSION" && "$INSTALLED" == "$SRC_VERSION" ]]; then
      log "installed fno $INSTALLED from PyPI (front door + CLI + agent binaries on PATH)."
      # The receipt proves the advertised command, not uv's exit code. A wheel
      # that predates the complete payload stays installed (the Python CLI
      # works) but the missing front door is named with its repair (AC2-EDGE).
      verify_frontdoor || true
      log "restart your shell (or source your env) to pick up PATH."
      next_steps
      exit 0
    fi
    # Not ours: the reserved 0.0.0 placeholder, a name collision, or a version
    # that does not match this plugin's bundled source. Remove it and build from
    # source rather than run a foreign/empty fno (AC7-FR).
    log "PyPI 'fno' is ${INSTALLED:-unresolved}, not this plugin's ${SRC_VERSION:-version} - using the bundled source instead."
    uv tool uninstall fno >/dev/null 2>&1 || true
  else
    log "PyPI 'fno' unavailable (offline or not yet published) - using the bundled source."
  fi

  if install_source_via_uv; then
    exit 0
  fi
  err "uv tool install failed; falling through to pip fallback."
fi

if command -v pip >/dev/null 2>&1 || command -v pip3 >/dev/null 2>&1; then
  PIP="$(command -v pip || command -v pip3)"
  log "uv unavailable; falling back to $PIP install --user from $CLI_DIR (Python-only)..."
  if "$PIP" install --user "$CLI_DIR"; then
    log "installed Python-only fno via pip --user. INCOMPLETE install: no 'fno' front door - run 'fno doctor update --rust' for the Rust binaries, or install a published PyPI wheel for the advertised command."
    log "ensure ~/.local/bin (or your user site-scripts dir) is on PATH."
    next_steps
    exit 0
  else
    err "pip install --user failed."
  fi
fi

err "fno CLI requires Python with uv or pip; install Python from https://python.org and re-run /plugin install fno"
exit 1
