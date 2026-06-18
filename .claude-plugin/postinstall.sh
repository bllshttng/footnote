#!/usr/bin/env bash
# Post-install hook for the fno Claude Code plugin.
#
# Lands a complete `fno` (CLI + all three Rust binaries) on PATH. Preference
# order (US7, ab-18563bcc):
#
#   1. `uv tool install fno` BY NAME - the published PyPI platform wheel, which
#      is binary-complete in one step (no separate `fno update --rust`). Guarded
#      for name-collision safety (AC7-FR): we verify the installed package is
#      OURS (its version matches this plugin's bundled cli/ source) and fall
#      back to the source build on any mismatch, so the reserved 0.0.0
#      placeholder or a squatted `fno` never runs in place of ours.
#   2. `uv tool install` from the bundled cli/ source (Python-only; the Rust
#      binaries then need a later `fno update --rust`) when the PyPI wheel is
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

install_source_via_uv() {
  log "installing from $CLI_DIR via uv tool install (source build; Python-only)..."
  if uv tool install --force "$CLI_DIR"; then
    log "installed Python-only fno from source. The Rust binaries are NOT included -"
    log "run 'fno update --rust' for the daemon-backed verbs (or install a published PyPI wheel)."
    log "restart your shell (or source your env) to pick up PATH."
    return 0
  fi
  return 1
}

if command -v uv >/dev/null 2>&1; then
  SRC_VERSION="$(src_version)"

  # Idempotent: already binary-complete at our version -> nothing to do. Require
  # ALL THREE binaries, not just the client: a same-version single-binary install
  # (e.g. a pre-G2 wheel) must NOT take this skip, or the daemon/worker stay
  # missing - the exact incomplete state this postinstall repairs.
  if [[ -n "$SRC_VERSION" && "$(uv_installed_fno_version)" == "$SRC_VERSION" ]] \
     && command -v fno-agents >/dev/null 2>&1 \
     && command -v fno-agents-daemon >/dev/null 2>&1 \
     && command -v fno-agents-worker >/dev/null 2>&1; then
    log "fno $SRC_VERSION already installed (binary-complete); skipping."
    exit 0
  fi

  log "preferring the published PyPI wheel: uv tool install fno (by name)..."
  if uv tool install --force fno >/dev/null 2>&1; then
    INSTALLED="$(uv_installed_fno_version)"
    if [[ -n "$SRC_VERSION" && "$INSTALLED" == "$SRC_VERSION" ]]; then
      log "installed binary-complete fno $INSTALLED from PyPI (CLI + all three Rust binaries on PATH)."
      log "restart your shell (or source your env) to pick up PATH."
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
    log "installed Python-only fno via pip --user. Run 'fno update --rust' for the Rust binaries."
    log "ensure ~/.local/bin (or your user site-scripts dir) is on PATH."
    exit 0
  else
    err "pip install --user failed."
  fi
fi

err "fno CLI requires Python with uv or pip; install Python from https://python.org and re-run /plugin install fno"
exit 1
