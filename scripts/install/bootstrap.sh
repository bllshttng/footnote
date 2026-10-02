#!/usr/bin/env bash
# bootstrap.sh - the zero-binary install doctor for the fno plugin.
#
# The plugin tree is the only thing guaranteed present after `/plugin install`,
# and the repair verb (`fno doctor update`) refuses to run until `fno-agents`
# exists. This script closes that gap: it needs no fno binary, no jq, no
# python3 - only bash and the standard POSIX tools every harness host has.
#
# An agent in a session where `fno` is missing runs it to learn, as JSON, the
# state of every prerequisite and the exact command that fixes each one, with
# absolute paths (a running session keeps its old PATH, so resolved paths are
# the only directly usable ones):
#
#   scripts/install/bootstrap.sh                    # report; exit 0 = ready
#   scripts/install/bootstrap.sh --repair           # run the plugin's own
#                                                   # installer, then re-report
#   scripts/install/bootstrap.sh --from-source DIR  # build+install from a
#                                                   # checkout (the hand-run
#                                                   # replacement)
#
# The report is one JSON object on stdout; human progress goes to stderr. Exit
# codes: 0 every required prerequisite is ok, 1 not ready (JSON still printed),
# 2 usage error.
#
# Non-interactive and idempotent. Every install step is verified by outcome
# (the binary exists and answers), never by exit code alone. It NEVER edits a
# shell rc file or harness config: repair sets FNO_NO_MODIFY_PATH so the
# delegated installer cannot either, and the report names the profile command
# for the user to run themselves.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# The one wall-clock bound for probes. Sourced from the same plugin tree; if
# this copy is broken, probes run unbounded but the script still reports.
HAVE_TIMEOUT_LIB=
# shellcheck source=../lib/with-timeout.sh
if [[ -r "$SCRIPT_DIR/../lib/with-timeout.sh" ]]; then
  # shellcheck disable=SC1091
  source "$SCRIPT_DIR/../lib/with-timeout.sh" && HAVE_TIMEOUT_LIB=1
fi

say() { printf '[fno bootstrap] %s\n' "$*" >&2; }
die() { printf '[fno bootstrap] ERROR: %s\n' "$*" >&2; exit "${2:-2}"; }

# with_timeout SECS then CMD...; unbounded fallback when the lib is absent.
run_bounded() {
  local secs="$1"; shift
  if [[ -n "$HAVE_TIMEOUT_LIB" ]]; then
    with_timeout "$secs" "$@"
  else
    "$@"
  fi
}

# --- JSON emission ----------------------------------------------------------
# A fixed set of keys we control (paths, versions, command strings), escaped
# for the four characters that can break a JSON string. No jq, no python3.
json_escape() {
  local s="$1"
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  s=${s//$'\n'/\\n}
  s=${s//$'\r'/\\r}
  s=${s//$'\t'/\\t}
  printf '%s' "$s"
}

CHECKS_JSON=""
READY=1
emit_check() { # emit_check <name> <state> <required 0|1> <path> <detail> <fix>
  local name="$1" state="$2" required="$3" path="$4" detail="$5" fix="$6"
  if [[ "$required" == 1 && "$state" != "ok" ]]; then
    READY=0
  fi
  local required_json="false"
  [[ "$required" == 1 ]] && required_json="true"
  local c
  c=$(printf '{"name":"%s","state":"%s","required":%s,"path":"%s","detail":"%s","fix":"%s"}' \
    "$(json_escape "$name")" "$(json_escape "$state")" "$required_json" \
    "$(json_escape "$path")" "$(json_escape "$detail")" "$(json_escape "$fix")")
  CHECKS_JSON="${CHECKS_JSON:+$CHECKS_JSON,}$c"
}

# --- shared resolution ------------------------------------------------------
# Locate uv: on PATH, else the well-known dirs Astral's installer uses (the
# same list scripts/install/fno.sh carries). UV_CALL is empty when absent.
UV_CALL=
resolve_uv() {
  local p
  if p="$(command -v uv 2>/dev/null)" && [[ -n "$p" ]]; then
    UV_CALL="$p"; return 0
  fi
  for p in "${HOME:-}/.local/bin/uv" "${XDG_BIN_HOME:-}/uv" "${HOME:-}/.cargo/bin/uv"; do
    if [[ -n "$p" && -x "$p" ]]; then UV_CALL="$p"; return 0; fi
  done
  return 1
}

# uv colorizes `tool dir` on a TTY; strip ANSI defensively (BSD sed cannot
# interpret \033, so the ESC byte is built with printf - same as fno.sh).
strip_ansi() {
  local esc
  esc="$(printf '\033')"
  printf '%s' "$1" | sed -e "s/${esc}\[[0-9;]*[@-~]//g" -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//'
}

TOOL_DIR=
TOOL_BIN=
resolve_tool_dirs() {
  [[ -n "$UV_CALL" ]] || return 1
  local d
  d="$(NO_COLOR=1 UV_NO_COLOR=1 "$UV_CALL" tool dir 2>/dev/null)" || return 1
  d="$(strip_ansi "$d")"
  [[ -n "$d" ]] || return 1
  TOOL_DIR="$d"
  d="$(NO_COLOR=1 UV_NO_COLOR=1 "$UV_CALL" tool dir --bin 2>/dev/null)" || d=""
  d="$(strip_ansi "$d")"
  TOOL_BIN="${d:-${UV_TOOL_BIN_DIR:-${HOME:-}/.local/bin}}"
  return 0
}

# --- checks (each appends one JSON object; order is the report order) -------
check_uv() {
  if resolve_uv; then
    emit_check "uv" "ok" 1 "$UV_CALL" "uv is available." ""
  else
    emit_check "uv" "missing" 1 "" "no uv on PATH and none in ~/.local/bin or ~/.cargo/bin." \
      "curl -fsSL https://astral.sh/uv/install.sh | sh"
  fi
}

check_wheel() {
  local cand version out rc
  if resolve_tool_dirs; then
    cand="$TOOL_DIR/fno/bin/fno-py"
  else
    cand=""
  fi
  if [[ -z "$cand" || ! -x "$cand" ]]; then
    cand="$(command -v fno-py 2>/dev/null || true)"
  fi
  if [[ -z "$cand" ]]; then
    emit_check "wheel" "missing" 1 "" "the Python CLI (fno-py) is not installed." \
      "run this script with --repair, or: uv tool install --force --compile-bytecode fno"
    return 0
  fi
  out="$(run_bounded 5 "$cand" --version 2>&1)"; rc=$?
  if [[ $rc -eq 0 && -n "$out" ]]; then
    version="$(printf '%s' "$out" | head -1)"
    emit_check "wheel" "ok" 1 "$cand" "Python CLI answers: $version" ""
  elif [[ -e "$cand" ]]; then
    emit_check "wheel" "broken" 1 "$cand" "fno-py exists but --version failed (rc=$rc)." \
      "run this script with --repair to reinstall the wheel"
  else
    emit_check "wheel" "missing" 1 "" "the Python CLI (fno-py) is not installed." \
      "run this script with --repair, or: uv tool install --force --compile-bytecode fno"
  fi
}

check_frontdoor() {
  local cand out rc
  cand=""
  if [[ -n "$TOOL_DIR" && -x "$TOOL_DIR/fno/bin/fno" ]]; then
    cand="$TOOL_DIR/fno/bin/fno"
  elif [[ -n "$TOOL_BIN" && -x "$TOOL_BIN/fno" ]]; then
    cand="$TOOL_BIN/fno"
  else
    cand="$(command -v fno 2>/dev/null || true)"
  fi
  if [[ -z "$cand" ]]; then
    emit_check "frontdoor" "missing" 1 "" "the Rust front door (the 'fno' mux binary) is not installed; the wheel that carries it is the repair." \
      "run this script with --repair"
    return 0
  fi
  # `mux ls` is the discriminator: the Python CLI has no mux verb and fails
  # fast, so rc 0 (answered) or 124 (our bound fired on a wedged socket - it
  # still proves the Rust front door is present, same reading as the
  # session-start hook).
  run_bounded 3 "$cand" mux ls --json >/dev/null 2>&1; rc=$?
  if [[ $rc -ne 0 && $rc -ne 124 ]]; then
    emit_check "frontdoor" "broken" 1 "$cand" "exists but 'mux ls --json' failed (rc=$rc); a non-mux 'fno' may shadow the front door." \
      "run this script with --repair, or: cargo install fno"
    return 0
  fi
  out="$(run_bounded 5 "$cand" --version 2>&1)"; rc=$?
  if [[ $rc -eq 0 && -n "$out" ]]; then
    emit_check "frontdoor" "ok" 1 "$cand" "front door answers mux ls and forwards --version: $(printf '%s' "$out" | head -1)" ""
  else
    emit_check "frontdoor" "broken" 1 "$cand" "mux answered but --version did not forward (rc=$rc)." \
      "run this script with --repair to reinstall the wheel"
  fi
}

check_agents() {
  local bin missing="" found_first="" p
  for bin in fno-agents fno-agents-daemon fno-agents-worker; do
    p=""
    if [[ -n "$TOOL_BIN" && -x "$TOOL_BIN/$bin" ]]; then
      p="$TOOL_BIN/$bin"
    elif [[ -n "${HOME:-}" && -x "$HOME/.cargo/bin/$bin" ]]; then
      p="$HOME/.cargo/bin/$bin"
    fi
    if [[ -z "$p" ]]; then
      missing="${missing:+$missing, }$bin"
    elif [[ -z "$found_first" ]]; then
      found_first="$p"
    fi
  done
  if [[ -z "$missing" ]]; then
    emit_check "fno-agents" "ok" 1 "${found_first%/*}" "all three agent binaries resolve." ""
  else
    emit_check "fno-agents" "missing" 1 "" "missing: $missing. The wheel bundles them; a Python-only source install does not." \
      "run this script with --repair"
  fi
}

PATH_STATE="unknown"
check_path() {
  local fix="" detail=""
  if [[ -z "$TOOL_BIN" ]]; then
    emit_check "path" "unknown" 0 "" "no uv tool bin dir resolved; PATH state unknowable." ""
    PATH_STATE="unknown"
    return 0
  fi
  case ":${PATH:-}:" in
    *":$TOOL_BIN:"*)
      emit_check "path" "ok" 0 "$TOOL_BIN" "the tool bin dir is on PATH." ""
      PATH_STATE="ok"
      return 0
      ;;
  esac
  fix="export PATH=\"$TOOL_BIN:\$PATH\""
  detail="for this session, or run 'uv tool update-shell' yourself to edit the shell profile (this script never edits rc files)."
  emit_check "path" "missing" 0 "$TOOL_BIN" "$detail" "$fix"
  PATH_STATE="missing"
}

check_plugin_root() {
  local manifest version
  manifest="$PLUGIN_ROOT/.claude-plugin/plugin.json"
  if [[ ! -r "$manifest" ]]; then
    emit_check "plugin_root" "missing" 1 "$PLUGIN_ROOT" "no .claude-plugin/plugin.json above this script; run it from a plugin tree or checkout." \
      "reinstall the plugin (/plugin install fno@footnote)"
    return 0
  fi
  version="$(sed -n -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' "$manifest" 2>/dev/null | head -1)"
  if [[ -n "$version" ]]; then
    emit_check "plugin_root" "ok" 1 "$PLUGIN_ROOT" "plugin tree at version $version." ""
  else
    emit_check "plugin_root" "broken" 1 "$PLUGIN_ROOT" "plugin.json exists but its version is unreadable." \
      "reinstall the plugin (/plugin install fno@footnote)"
  fi
}

check_rust() {
  local out rc p
  p="$(command -v cargo 2>/dev/null || true)"
  if [[ -z "$p" ]]; then
    emit_check "rust" "optional_missing" 0 "" "no cargo; only the --from-source route needs it (the wheel ships the binaries)." \
      "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    return 0
  fi
  out="$(run_bounded 5 "$p" --version 2>&1)"; rc=$?
  if [[ $rc -eq 0 && -n "$out" ]]; then
    emit_check "rust" "ok" 0 "$p" "$(printf '%s' "$out" | head -1)" ""
  else
    emit_check "rust" "broken" 0 "$p" "cargo exists but --version failed (rc=$rc)." \
      "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  fi
}

run_all_checks() {
  CHECKS_JSON=""
  READY=1
  TOOL_DIR=""; TOOL_BIN=""
  check_uv
  check_wheel      # resolves TOOL_DIR/TOOL_BIN as a side effect
  check_frontdoor
  check_agents
  check_path
  check_plugin_root
  check_rust
}

emit_report() { # emit_report <mode> <restart 0|1> [extra-json-members as preformatted "k":v]
  local mode="$1"; shift
  local restart="$1"; shift
  local ready_json="false" restart_json="false"
  [[ "$READY" == 1 ]] && ready_json="true"
  [[ "$restart" == 1 ]] && restart_json="true"
  printf '{"schema":"fno.bootstrap.v1","mode":"%s","ready":%s,"restart_needed":%s,"plugin_root":"%s"%s,"checks":[%s]}\n' \
    "$(json_escape "$mode")" "$ready_json" "$restart_json" "$(json_escape "$PLUGIN_ROOT")" \
    "${1:-}" "$CHECKS_JSON"
}

usage() {
  cat >&2 <<'EOF'
usage: scripts/install/bootstrap.sh [--repair | --from-source DIR | --help]

  (no flag)        report every prerequisite as JSON on stdout; exit 0 = ready
  --repair         run the plugin's own installer (.claude-plugin/postinstall.sh),
                   then re-report. Never edits shell rc files.
  --from-source DIR  build+install from a checkout: uv tool install from DIR/cli,
                   then cargo install of DIR/crates/fno and DIR/crates/fno-agents.
                   Prints the plugin-registration step; never runs it.
EOF
}

# --- modes ------------------------------------------------------------------
MODE="report"
RESTART_NEEDED=0
EXTRA_JSON=""

mode_repair() {
  local installer="$PLUGIN_ROOT/.claude-plugin/postinstall.sh" rc
  MODE="repair"
  if [[ ! -f "$installer" ]]; then
    say "repair: no postinstall.sh at $installer; reporting only."
    EXTRA_JSON=',"repair":{"ran":false,"installer_exit":127}'
    return 0
  fi
  # FNO_NO_MODIFY_PATH travels through to fno.sh (postinstall's no-uv
  # delegation) so a repair can never edit a shell profile behind the user's
  # back. The profile command is reported, not run.
  FNO_NO_MODIFY_PATH=1
  export FNO_NO_MODIFY_PATH
  say "repair: running the plugin installer (bounded at 900s); its log follows on stderr..."
  if [[ -n "$HAVE_TIMEOUT_LIB" ]]; then
    with_timeout 900 bash "$installer" >&2
  else
    bash "$installer" >&2
  fi
  rc=$?
  say "repair: installer exit $rc."
  EXTRA_JSON=",\"repair\":{\"ran\":true,\"installer_exit\":$rc}"
  RESTART_NEEDED=1
  return 0
}

mode_from_source() {
  local checkout="$1" rc
  MODE="from-source"
  for f in cli/pyproject.toml crates/fno/Cargo.toml crates/fno-agents/Cargo.toml; do
    if [[ ! -f "$checkout/$f" ]]; then
      die "--from-source: $checkout does not look like an fno checkout (missing $f)."
    fi
  done
  if ! resolve_uv; then
    say "from-source needs uv; none found."
    EXTRA_JSON=',"from_source":{"ran":false,"reason":"uv missing"}'
    return 0
  fi
  local actions=""
  say "from-source: installing the Python CLI from $checkout/cli ..."
  if run_bounded 600 "$UV_CALL" tool install --force --compile-bytecode "$checkout/cli" >&2; then
    actions="$actions\"uv tool install from cli\""
  else
    say "from-source: uv tool install failed (rc=$?)."
  fi
  if command -v cargo >/dev/null 2>&1; then
    say "from-source: cargo install crates/fno (the front door; bounded at 900s) ..."
    if run_bounded 900 cargo install --locked --path "$checkout/crates/fno" >&2; then
      actions="${actions:+$actions,}\"cargo install crates/fno\""
    else
      say "from-source: cargo install crates/fno failed."
    fi
    say "from-source: cargo install crates/fno-agents (three agent binaries) ..."
    if run_bounded 900 cargo install --locked --path "$checkout/crates/fno-agents" >&2; then
      actions="${actions:+$actions,}\"cargo install crates/fno-agents\""
    else
      say "from-source: cargo install crates/fno-agents failed."
    fi
  else
    say "from-source: no cargo; the Rust binaries were not built. The Python CLI works as fno-py."
  fi
  # Registering the checkout as the harness's plugin edits harness config, so
  # it is printed for the user, never run here.
  say "from-source done. Register the plugin for your harness from $checkout yourself (Claude Code: /plugin marketplace add + /plugin install; see docs/getting-started.md)."
  EXTRA_JSON=",\"from_source\":{\"ran\":true,\"checkout\":\"$(json_escape "$checkout")\",\"actions\":[$actions]}"
  RESTART_NEEDED=1
  return 0
}

main() {
  local arg="${1:-}"
  case "$arg" in
    "") ;;
    --repair) shift; mode_repair "$@" ;;
    --from-source)
      [[ -n "${2:-}" ]] || die "--from-source needs a checkout directory."
      [[ -d "$2" ]] || die "--from-source: not a directory: $2"
      mode_from_source "$2"
      ;;
    --help|-h) usage; exit 0 ;;
    *) usage; die "unknown argument: $arg" ;;
  esac

  run_all_checks

  if [[ "$MODE" != "report" ]]; then
    RESTART_NEEDED=1   # any install pass leaves a running session's PATH stale
  fi
  if [[ "$PATH_STATE" == "missing" && "$READY" == 1 ]]; then
    RESTART_NEEDED=1   # binaries work via absolute paths; the bare 'fno' does not yet
  fi

  emit_report "$MODE" "$RESTART_NEEDED" "$EXTRA_JSON"
  [[ "$READY" == 1 ]] && exit 0
  exit 1
}

main "$@"
