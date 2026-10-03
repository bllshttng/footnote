#!/usr/bin/env bash
# Install-channel matrix driver: one row per invocation; the row list lives in
# .github/workflows/install-channels.yml.
#
# Each row installs footnote the way that channel's user would, on a clean
# machine (fresh HOME, reduced PATH, no XDG_* / FNO_* inheritance), then runs
# the shared smoke: fno --version, fno-agents --version, fno mux ls --json,
# and a backlog idea -> get round trip in a scratch git repo. Exit 0 = the
# channel works; exit 1 = it does not; exit 42 = the runner was dirty (uv, fno
# or fno-agents resolvable before install) and the row is not scoreable; exit
# 3 = the driver and the workflow disagree about the row itself.
#
# A row may legitimately fail: the workflow's expect field says which rows are
# supposed to fail today and names the node whose work flips them. This driver
# only reports what it observed.
set -uo pipefail

ROW="${1:?usage: channel_matrix_smoke.sh <row-id>}"
# macOS legs of a row share the row's body; the workflow suffixes their ids.
ROW_KEY="${ROW%-macos}"

fail=0
pass() {
  printf 'PASS[%s] %s\n' "$1" "$2"
}
miss() {
  printf 'FAIL[%s] %s\n' "$1" "$2"
  fail=1
}
run_capture() {
  OUT="$("$@" 2>&1)"
  RC=$?
}

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"

# --- clean machine -----------------------------------------------------------
# Fresh HOME, pristine SHELL (uv's profile edit refuses without one), no
# inherited XDG/FNO state. Runner tool locations are captured while the full
# PATH is still live; rows re-add only their own prerequisites.
BASE="$(mktemp -d)"
BASE="$(cd "$BASE" && pwd -P)"
trap 'rm -rf "$BASE"' EXIT
mkdir -p "$BASE/home" "$BASE/work"

RUNNER_NODE_BIN=""
if command -v node >/dev/null 2>&1; then
  RUNNER_NODE_BIN="$(dirname "$(command -v node)")"
fi
RUNNER_CARGO_BIN=""
if [ -x "$HOME/.cargo/bin/cargo" ]; then
  RUNNER_CARGO_BIN="$HOME/.cargo/bin"
fi

export HOME="$BASE/home"
export SHELL="${SHELL:-/bin/bash}"
# The reduced PATH every row starts from; rows re-add only their own
# prerequisites, so the row scores the channel, not the runner image.
export PATH="/usr/bin:/bin:/usr/sbin:/sbin"
unset PYTHONPATH
unset XDG_CACHE_HOME XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_BIN_HOME
unset FNO_VERSION FNO_INSTALL_WHEEL FNO_INSTALL_DIR FNO_NO_MODIFY_PATH
unset CLAUDE_CONFIG_DIR CLAUDE_PLUGIN_DATA CODEX_HOME CODEX_PLUGIN_CACHE
unset UV_TOOL_DIR UV_TOOL_BIN_DIR
# The scrub contract: prove the unsets took. An inherited PYTHONPATH would put
# the source tree ahead of the installed artifact on sys.path.
[ -z "${PYTHONPATH:-}" ] || { echo "PYTHONPATH survived the clean-machine unset"; exit 1; }

# Refuse a dirty machine: uv, fno or fno-agents resolvable before install means
# the row would score the runner, not the channel. Every row calls this FIRST,
# before it installs any prerequisite of its own.
assert_clean_machine() {
  dirty=""
  for b in uv fno fno-agents; do
    command -v "$b" >/dev/null 2>&1 && dirty="$dirty $b"
  done
  if [ -n "$dirty" ]; then
    printf 'FAIL[env]%s resolvable on the reduced PATH before install\n' "$dirty"
    exit 42
  fi
}

# newest_pypi_version: highest fno version on PyPI, pre-releases included, so
# the pinned rows follow the newest rc with no edit here.
newest_pypi_version() {
  curl -fsSL https://pypi.org/pypi/fno/json | python3 -c '
import json, re, sys
data = json.load(sys.stdin)
def key(v):
    return tuple((0, int(t)) if t.isdigit() else (1, t) for t in re.findall(r"\d+|\D+", v))
print(sorted(data["releases"], key=key)[-1])
'
}

# --- shared smoke ------------------------------------------------------------
shared_smoke() {
  # $@ = candidate directories holding the installed fno / fno-agents binaries;
  # the first one carrying an fno binary wins (channels disagree about where
  # the tool bin lands, and a macOS pip --user fallback lands elsewhere still).
  local bin_dir="" agents_bin="" d
  for d in "$@"; do
    if [ -x "$d/fno" ]; then
      bin_dir="$d"
      break
    fi
  done
  if [ -z "$bin_dir" ]; then
    miss "install" "no fno binary in any of: $*"
    return 1
  fi
  export PATH="$bin_dir:$PATH"

  run_capture "$bin_dir/fno" --version
  if [ "$RC" -eq 0 ] && printf '%s' "$OUT" | grep -qE 'fno[[:space:]]+[0-9]+\.[0-9]+'; then
    pass "version" "fno --version answers"
  else
    miss "version" "rc=$RC out: $(printf '%s' "$OUT" | tail -1)"
  fi

  # The agents binary need not sit beside the front door: the cargo channel
  # lands it in the uv tool bin instead. Search every candidate; fall back to
  # beside-the-front-door so the miss message names the expected spot.
  for d in "$@"; do
    if [ -x "$d/fno-agents" ]; then
      agents_bin="$d/fno-agents"
      break
    fi
  done
  agents_bin="${agents_bin:-$bin_dir/fno-agents}"
  run_capture "$agents_bin" --version
  if [ "$RC" -eq 0 ]; then
    pass "agents-version" "fno-agents --version answers"
  else
    miss "agents-version" "rc=$RC out: $(printf '%s' "$OUT" | tail -1)"
  fi

  run_capture "$bin_dir/fno" mux ls --json
  if [ "$RC" -eq 0 ]; then
    pass "mux" "fno mux ls --json answers natively"
  else
    miss "mux" "rc=$RC out: $(printf '%s' "$OUT" | tail -1)"
  fi

  local repo="$BASE/work/repo"
  mkdir -p "$repo"
  git -C "$repo" init -q
  run_capture "$bin_dir/fno" backlog idea "channel smoke" --difficulty low --json
  if [ "$RC" -ne 0 ]; then
    miss "backlog-idea" "rc=$RC out: $(printf '%s' "$OUT" | tail -2)"
    return 1
  fi
  local node_id
  node_id="$(printf '%s' "$OUT" | python3 -c 'import json,sys
try:
    d = json.load(sys.stdin)
except Exception:
    print(""); raise SystemExit
print(d.get("id") or d.get("node_id") or "")' 2>/dev/null)"
  if [ -z "$node_id" ]; then
    node_id="$(printf '%s' "$OUT" | grep -oE '"id"[[:space:]]*:[[:space:]]*"[^"]+"' | head -1 | sed 's/.*: *"//; s/"$//')"
  fi
  if [ -z "$node_id" ]; then
    miss "backlog-idea" "created a node but no id in the output: $(printf '%s' "$OUT" | tail -2)"
    return 1
  fi
  run_capture "$bin_dir/fno" backlog get "$node_id"
  if [ "$RC" -eq 0 ]; then
    pass "backlog-get" "idea -> get round trip answered"
  else
    miss "backlog-get" "rc=$RC out: $(printf '%s' "$OUT" | tail -2)"
  fi
}

# wait_for_installer_exit: poll a postinstall log for its terminal
# "installer exit N" line. Prints N and returns 0 when seen; prints "no-log"
# and returns 2 when the log never appears within 60s (the installer never
# started); prints "timeout" and returns 1 when the bound fires first.
wait_for_installer_exit() {
  local log="$1"
  local max="${2:-600}"
  local waited=0
  local line
  while [ "$waited" -lt 60 ]; do
    [ -f "$log" ] && break
    sleep 5
    waited=$((waited + 5))
  done
  if [ ! -f "$log" ]; then
    printf 'no-log\n'
    return 2
  fi
  while [ "$waited" -lt "$max" ]; do
    line="$(grep -E '^installer exit [0-9]+$' "$log" | tail -1)"
    if [ -n "$line" ]; then
      printf '%s\n' "${line##* }"
      return 0
    fi
    sleep 5
    waited=$((waited + 5))
  done
  printf 'timeout\n'
  return 1
}

# score_plugin_session: shared scoring for the two plugin-session rows.
# $1 = postinstall log path, $2 = bin dir to smoke once installed.
score_plugin_session() {
  local code
  code="$(wait_for_installer_exit "$1")"
  case "$code" in
    no-log)
      miss "installer" "session start never started the installer (no log at $1)"
      ;;
    timeout)
      miss "installer" "no 'installer exit' line within the wait (log $1)"
      ;;
    0)
      pass "installer" "installer exit 0"
      shared_smoke "$2"
      ;;
    *)
      miss "installer" "installer exit $code: $(tail -2 "$1" 2>/dev/null | tr '\n' ' ')"
      ;;
  esac
}

# install_cli_via_npm: install one npm package into a private prefix and put
# its bin on PATH. $1 = package name.
install_cli_via_npm() {
  if [ -z "$RUNNER_NODE_BIN" ]; then
    miss "env" "node not found on the runner image"
    return 1
  fi
  local npm_prefix="$BASE/npm"
  export PATH="$RUNNER_NODE_BIN:$PATH"
  run_capture npm install -g --prefix "$npm_prefix" "$1"
  if [ "$RC" -ne 0 ]; then
    miss "cli-install" "npm install $1 failed rc=$RC: $(printf '%s' "$OUT" | tail -1)"
    return 1
  fi
  export PATH="$npm_prefix/bin:$PATH"
  return 0
}

install_uv_astral() {
  run_capture sh -c 'curl -fsSL https://astral.sh/uv/install.sh | sh'
  if [ "$RC" -ne 0 ] || [ ! -x "$HOME/.local/bin/uv" ]; then
    miss "uv-install" "Astral uv installer failed rc=$RC: $(printf '%s' "$OUT" | tail -1)"
    return 1
  fi
  export PATH="$HOME/.local/bin:$PATH"
  return 0
}

# --- rows --------------------------------------------------------------------
# Every row asserts a clean machine FIRST; only then does it add its own
# channel prerequisites and install.

row_claude_marketplace() {
  assert_clean_machine
  install_cli_via_npm @anthropic-ai/claude-code || return 0
  export CLAUDE_CONFIG_DIR="$BASE/claude-config"
  # The plugin install clones the marketplace repo through git; a fresh
  # machine has no SSH keys, so rewrite the remote to HTTPS for this row.
  git config --global url."https://github.com/".insteadOf "git@github.com:"
  run_capture claude plugin marketplace add bllshttng/footnote
  if [ "$RC" -ne 0 ]; then
    miss "marketplace-add" "rc=$RC: $(printf '%s' "$OUT" | tail -1)"
    return 0
  fi
  pass "marketplace-add" "marketplace added"
  run_capture claude plugin install fno@footnote
  if [ "$RC" -eq 0 ]; then
    pass "install" "plugin installed"
  else
    miss "install" "rc=$RC: $(printf '%s' "$OUT" | tail -1)"
  fi
}

# Copies the checked-out tree into scratch so the row never dirties the
# workspace checkout, and stamps plugin.json to the newest published version so
# the installer's channel math targets a real release.
copy_tree_to_scratch() {
  local dest="$1"
  mkdir -p "$dest"
  git archive HEAD | tar -x -C "$dest"
}

row_claude_plugin_session() {
  assert_clean_machine
  local tree="$BASE/tree"
  copy_tree_to_scratch "$tree"
  local version
  version="$(newest_pypi_version)" || { miss "pypi" "could not read the newest PyPI version"; return 0; }
  awk -v v="$version" '{
    gsub(/"version"[[:space:]]*:[[:space:]]*"[^"]*"/, "\"version\": \"" v "\"")
    print
  }' "$tree/.claude-plugin/plugin.json" > "$tree/.claude-plugin/plugin.json.new"
  mv "$tree/.claude-plugin/plugin.json.new" "$tree/.claude-plugin/plugin.json"
  export CLAUDE_PLUGIN_DATA="$BASE/plugin-data"
  bash "$tree/hooks/context-run.sh" claude-session-start >/dev/null 2>&1
  local py_bins="" d
  for d in "$HOME"/Library/Python/*/bin; do
    [ -d "$d" ] && py_bins="$py_bins $d"
  done
  # shellcheck disable=SC2086
  score_plugin_session "$CLAUDE_PLUGIN_DATA/postinstall.log" "$HOME/.local/bin" $py_bins
}

row_codex_plugin_session() {
  assert_clean_machine
  install_cli_via_npm @openai/codex || return 0
  # codex refuses a CODEX_HOME that does not exist yet.
  mkdir -p "$BASE/codex-home"
  export CODEX_HOME="$BASE/codex-home"
  # Same stamp as row_claude_plugin_session: the checkout's plugin.json can
  # lead the registry (main bumped to 0.5.0 before PyPI saw it), and a pinned
  # install of an unpublished version degrades to a source install that has
  # no `fno` front door. The channel math must target a real release.
  local tree="$BASE/codex-tree"
  copy_tree_to_scratch "$tree"
  local version
  version="$(newest_pypi_version)" || { miss "pypi" "could not read the newest PyPI version"; return 0; }
  awk -v v="$version" '{
    gsub(/"version"[[:space:]]*:[[:space:]]*"[^"]*"/, "\"version\": \"" v "\"")
    print
  }' "$tree/.claude-plugin/plugin.json" > "$tree/.claude-plugin/plugin.json.new"
  mv "$tree/.claude-plugin/plugin.json.new" "$tree/.claude-plugin/plugin.json"
  run_capture codex plugin marketplace add "$tree"
  if [ "$RC" -ne 0 ]; then
    miss "marketplace-add" "rc=$RC: $(printf '%s' "$OUT" | tail -1)"
    return 0
  fi
  run_capture codex plugin add fno@footnote
  if [ "$RC" -ne 0 ]; then
    miss "plugin-add" "rc=$RC: $(printf '%s' "$OUT" | tail -1)"
    return 0
  fi
  local installed_hook
  installed_hook="$(find "$CODEX_HOME" -name context-run.sh -path '*/hooks/*' 2>/dev/null | head -1)"
  if [ -z "$installed_hook" ]; then
    miss "plugin-tree" "installed plugin tree has no hooks/context-run.sh"
    return 0
  fi
  bash "$(dirname "$installed_hook")/context-run.sh" codex-session-start >/dev/null 2>&1
  local py_bins="" d
  for d in "$HOME"/Library/Python/*/bin; do
    [ -d "$d" ] && py_bins="$py_bins $d"
  done
  # shellcheck disable=SC2086
  score_plugin_session "$HOME/.local/state/fno/plugin-install/postinstall.log" "$HOME/.local/bin" $py_bins
}

row_fno_sh_served() {
  assert_clean_machine
  run_capture sh -c 'curl -fsSL https://fno.sh | sh'
  if [ "$RC" -ne 0 ]; then
    miss "install" "curl | sh rc=$RC: $(printf '%s' "$OUT" | tail -1)"
    return 0
  fi
  shared_smoke "$HOME/.local/bin"
}

row_fno_sh_head() {
  assert_clean_machine
  local version
  version="$(newest_pypi_version)" || { miss "pypi" "could not read the newest PyPI version"; return 0; }
  run_capture env FNO_VERSION="$version" sh "$REPO_ROOT/scripts/install/fno.sh"
  if [ "$RC" -ne 0 ]; then
    miss "install" "fno.sh rc=$RC: $(printf '%s' "$OUT" | tail -2 | tr '\n' ' ')"
    return 0
  fi
  shared_smoke "$HOME/.local/bin"
}

# The Worker pins a release tag, so the served bytes lag a release whenever
# the pin is not bumped. Compare against the newest stable tag; bytes-only,
# the served rows above already prove installability.
row_fno_sh_fresh() {
  local tag
  tag="$(git ls-remote --tags --refs https://github.com/bllshttng/footnote 'refs/tags/v*' \
    | awk -F'refs/tags/' '{print $2}' \
    | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -1)"
  if [ -z "$tag" ]; then
    miss "fresh" "could not read the newest stable tag from bllshttng/footnote"
    return 0
  fi
  curl -fsSL https://fno.sh > "$BASE/served.out" 2>/dev/null
  curl -fsSL "https://raw.githubusercontent.com/bllshttng/footnote/$tag/scripts/install/fno.sh" > "$BASE/released.out" 2>/dev/null
  if [ -s "$BASE/served.out" ] && cmp -s "$BASE/served.out" "$BASE/released.out"; then
    pass "fresh" "fno.sh serves the same bytes as $tag"
  else
    miss "fresh" "fno.sh is stale against $tag; bump the fno-web Worker INSTALL_SCRIPT_URL pin to $tag and redeploy"
  fi
}

row_install_sh_alias() {
  assert_clean_machine
  curl -fsSL https://fno.sh > "$BASE/root.out" 2>/dev/null
  curl -fsSL https://fno.sh/install.sh > "$BASE/alias.out" 2>/dev/null
  if cmp -s "$BASE/root.out" "$BASE/alias.out" && [ -s "$BASE/root.out" ]; then
    pass "alias" "install.sh serves the same bytes as the domain root"
  else
    miss "alias" "install.sh and the domain root diverge"
  fi
}

row_pypi_uv() {
  assert_clean_machine
  install_uv_astral || return 0
  run_capture uv tool install fno
  if [ "$RC" -ne 0 ]; then
    miss "uv-tool-install" "rc=$RC: $(printf '%s' "$OUT" | tail -1)"
    return 0
  fi
  shared_smoke "$HOME/.local/bin"
}

row_pypi_uv_pinned() {
  assert_clean_machine
  install_uv_astral || return 0
  local version
  version="$(newest_pypi_version)" || { miss "pypi" "could not read the newest PyPI version"; return 0; }
  run_capture uv tool install "fno==$version"
  if [ "$RC" -ne 0 ]; then
    miss "uv-tool-install" "rc=$RC: $(printf '%s' "$OUT" | tail -1)"
    return 0
  fi
  shared_smoke "$HOME/.local/bin"
}

row_brew() {
  assert_clean_machine
  export PATH="/opt/homebrew/bin:$PATH"
  run_capture brew install bllshttng/fno/fno
  if [ "$RC" -ne 0 ]; then
    miss "brew-install" "rc=$RC: $(printf '%s' "$OUT" | tail -2 | tr '\n' ' ')"
    return 0
  fi
  shared_smoke "/opt/homebrew/bin"
}

row_cargo() {
  assert_clean_machine
  if [ -n "$RUNNER_CARGO_BIN" ]; then
    # The image's rustup proxies live here; RUSTUP_HOME stays at its default.
    # Some images ship rustup with no default toolchain; pick one.
    export PATH="$RUNNER_CARGO_BIN:$PATH"
    rustup show active-toolchain >/dev/null 2>&1 || rustup default stable
  else
    # No rust on the image: install a private minimal toolchain.
    export RUSTUP_HOME="$BASE/rustup"
    export CARGO_HOME="$BASE/cargo-home"
    run_capture sh -c 'curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path'
    if [ "$RC" -ne 0 ] || [ ! -x "$CARGO_HOME/bin/cargo" ]; then
      miss "rustup" "rustup install rc=$RC: $(printf '%s' "$OUT" | tail -1)"
      return 0
    fi
    export PATH="$CARGO_HOME/bin:$PATH"
  fi
  # Fresh CARGO_HOME for the preinstalled branch too, so `cargo install`
  # lands in scratch instead of the image's own store.
  export CARGO_HOME="$BASE/cargo-home"
  mkdir -p "$CARGO_HOME"
  run_capture cargo install fno
  if [ "$RC" -ne 0 ]; then
    miss "cargo-install" "rc=$RC: $(printf '%s' "$OUT" | tail -2 | tr '\n' ' ')"
    return 0
  fi
  # cargo install lands the front door here; its first forwarded verb
  # bootstraps the wheel, whose data scripts carry fno-agents into the uv
  # tool bin ($HOME/.local/bin). `fno backlog` never provisions, so warm up
  # once, and keep the tool bin on PATH for the front door's bare-name
  # sibling fallback.
  export PATH="$HOME/.local/bin:$CARGO_HOME/bin:$PATH"
  run_capture "$CARGO_HOME/bin/fno" config get config.review.posture
  shared_smoke "$CARGO_HOME/bin" "$HOME/.local/bin"
}

row_skills_sh() {
  assert_clean_machine
  install_cli_via_npm skills || return 0
  cd "$BASE/work"
  # Install from THIS checkout, not from GitHub main: the row scores the PR's
  # own skill (the no-CLI fallback sentence lands with the PR that writes it).
  run_capture npx --yes skills add "$REPO_ROOT" --skill tdd --agent '*' -y
  if [ "$RC" -ne 0 ]; then
    miss "skills-add" "rc=$RC: $(printf '%s' "$OUT" | tail -1)"
    return 0
  fi
  local skill_md
  skill_md="$(find "$BASE" "$HOME" -name SKILL.md -path '*tdd*' 2>/dev/null | head -1)"
  if [ -z "$skill_md" ]; then
    miss "skill-installed" "no tdd SKILL.md found under the install cwd or HOME"
    return 0
  fi
  if grep -qF "project's own test command" "$skill_md"; then
    pass "skill-installed" "tdd SKILL.md installed and carries the no-CLI fallback"
  else
    miss "skill-installed" "installed tdd SKILL.md hard-requires the fno CLI (no fallback sentence)"
  fi
}

row_clone_setup() {
  assert_clean_machine
  local clone="$BASE/clone"
  copy_tree_to_scratch "$clone"
  cd "$clone"
  run_capture bash scripts/setup.sh
  if [ "$RC" -ne 0 ]; then
    miss "setup" "scripts/setup.sh rc=$RC: $(printf '%s' "$OUT" | tail -2 | tr '\n' ' ')"
    return 0
  fi
  shared_smoke "$HOME/.local/bin"
}

row_readme_commands() {
  run_capture bash "$REPO_ROOT/scripts/ci/check-readme-install-commands.sh"
  if [ "$RC" -eq 0 ]; then
    pass "readme" "every advertised install command is proven by a pass row"
  else
    miss "readme" "unproven README install commands: $(printf '%s' "$OUT" | tail -4 | tr '\n' ' ')"
  fi
}

run_row() {
  case "$ROW_KEY" in
    claude-marketplace)    row_claude_marketplace ;;
    claude-plugin-session) row_claude_plugin_session ;;
    codex-plugin-session)  row_codex_plugin_session ;;
    fno-sh-served)         row_fno_sh_served ;;
    fno-sh-head)           row_fno_sh_head ;;
    fno-sh-fresh)          row_fno_sh_fresh ;;
    install-sh-alias)      row_install_sh_alias ;;
    pypi-uv)               row_pypi_uv ;;
    pypi-uv-pinned)        row_pypi_uv_pinned ;;
    brew)                  row_brew ;;
    cargo)                 row_cargo ;;
    skills-sh)             row_skills_sh ;;
    clone-setup)           row_clone_setup ;;
    readme-commands)       row_readme_commands ;;
    *)
      printf 'FAIL[row] unknown row id: %s\n' "$ROW"
      exit 3
      ;;
  esac
  [ "$fail" -ne 0 ] && return 1
  return 0
}

# The row runs under a watchdog: an install that blocks forever must score as
# a hang (exit 43, which the workflow refuses to read as an expected fail),
# never as an honest channel failure.
run_row &
row_pid=$!
watched=0
while kill -0 "$row_pid" 2>/dev/null; do
  if [ "$watched" -ge 1500 ]; then
    kill "$row_pid" 2>/dev/null
    printf 'FAIL[row] row exceeded the 1500s bound; scoring as a hang, not an honest fail\n'
    exit 43
  fi
  sleep 5
  watched=$((watched + 5))
done
wait "$row_pid"; rc=$?

echo "---"
if [ "$rc" -ne 0 ]; then
  echo "channel matrix row $ROW: FAILED"
  exit "$rc"
fi
echo "channel matrix row $ROW: passed"
exit 0
