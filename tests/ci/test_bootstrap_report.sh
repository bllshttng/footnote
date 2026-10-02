#!/usr/bin/env bash
# test_bootstrap_report.sh
#
# The zero-binary bootstrap script (scripts/install/bootstrap.sh) is the one
# thing an agent can run when no fno binary exists. Its report is consumed by
# machines, so the contract this test pins is: always-valid JSON, the seven
# prerequisite checks with absolute paths, an honest restart_needed flag, and
# the never-edits-shell-rc promise (repair sets FNO_NO_MODIFY_PATH and runs
# postinstall.sh instead of touching a profile).
#
# Every case runs in a throwaway sandbox: a fake HOME, a fake `uv` on PATH, a
# fake plugin tree, and stub binaries. No network, no real install.
#
# Exit codes: 0 pass / 1 assertion failed / 77 skipped (missing deps)

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
SCRIPT="$REPO/scripts/install/bootstrap.sh"

[[ -r "$SCRIPT" ]] || { echo "skip: no bootstrap.sh at $SCRIPT"; exit 77; }
command -v python3 >/dev/null 2>&1 || { echo "skip: python3 needed to validate JSON"; exit 77; }

fail() { echo "FAIL: $*" >&2; exit 1; }

SANDBOXES=()
cleanup() {
  [[ ${#SANDBOXES[@]} -eq 0 ]] || rm -rf "${SANDBOXES[@]}"
  rm -f "${REPORTS[@]:-}"
}
trap cleanup EXIT

REPORTS=()

# Build one sandbox and leave its layout under $SB. Echoes the sandbox root.
#   mk_sandbox <green:0|1> <toolbin_on_path:0|1>
#   green=1 also seeds the fake venv/tool-bin binaries and a plugin tree.
mk_sandbox() {
  local green="$1" on_path="$2"
  local sb
  sb="$(mktemp -d)" || fail "mktemp failed"
  SANDBOXES+=("$sb")
  mkdir -p "$sb/home" "$sb/bin"

  # Plugin tree: the real script + timeout lib, a minimal plugin.json, and a
  # stub postinstall that records it ran and exits 0.
  mkdir -p "$sb/plugin/scripts/install" "$sb/plugin/scripts/lib" "$sb/plugin/.claude-plugin"
  cp "$SCRIPT" "$sb/plugin/scripts/install/bootstrap.sh"
  cp "$REPO/scripts/lib/with-timeout.sh" "$sb/plugin/scripts/lib/with-timeout.sh"
  printf '{"name":"fno","version":"0.4.0"}\n' > "$sb/plugin/.claude-plugin/plugin.json"
  printf '#!/bin/sh\ntouch "%s/postinstall-ran"\nexit 0\n' "$sb" > "$sb/plugin/.claude-plugin/postinstall.sh"
  chmod +x "$sb/plugin/.claude-plugin/postinstall.sh"

  # Fake uv answering the two `tool dir` forms (--bin must be distinguished
  # from the bare form, which shares the first two words).
  cat > "$sb/bin/uv" <<EOF
#!/bin/sh
case "\$1 \$2" in
  "tool dir")
    case " \$3" in
      " --bin") echo "$sb/home/tools-bin" ;;
      *) echo "$sb/home/tools" ;;
    esac ;;
  *) exit 0 ;;
esac
EOF
  chmod +x "$sb/bin/uv"

  if [[ "$green" == 1 ]]; then
    mkdir -p "$sb/home/tools/fno/bin" "$sb/home/tools-bin"
    printf '#!/bin/sh\n[ "$1" = --version ] && { echo "fno 0.4.0"; exit 0; }\nexit 1\n' > "$sb/home/tools/fno/bin/fno-py"
    printf '#!/bin/sh\n[ "$1" = "mux" ] && exit 0\n[ "$1" = --version ] && { echo "fno 0.4.0"; exit 0; }\nexit 1\n' > "$sb/home/tools/fno/bin/fno"
    chmod +x "$sb/home/tools/fno/bin/fno-py" "$sb/home/tools/fno/bin/fno"
    for b in fno-agents fno-agents-daemon fno-agents-worker; do
      printf '#!/bin/sh\nexit 0\n' > "$sb/home/tools-bin/$b"
      chmod +x "$sb/home/tools-bin/$b"
    done
    printf '#!/bin/sh\necho "cargo 1.0"\n' > "$sb/bin/cargo"
    chmod +x "$sb/bin/cargo"
  fi

  # A marker rc file: no code path may touch it.
  printf '# user rc, do not touch\n' > "$sb/home/.zshrc"

  if [[ "$on_path" == 1 ]]; then
    printf '%s/bin:%s/home/tools-bin:/usr/bin:/bin\n' "$sb" "$sb" > "$sb/pathfile"
  else
    printf '%s/bin:/usr/bin:/bin\n' "$sb" > "$sb/pathfile"
  fi
  printf '%s\n' "$sb"
}

# run_bs <sandbox> <args...>: runs the script sandboxed; stdout goes to a
# per-sandbox report file; echoes the exit code.
run_bs() {
  local sb="$1"; shift
  local out="$sb/report.json"
  REPORTS+=("$out")
  HOME="$sb/home" PATH="$(cat "$sb/pathfile")" \
    bash "$sb/plugin/scripts/install/bootstrap.sh" "$@" >"$out" 2>"$sb/report.err"
  echo $?
}

# assert_json <file> <python-expr over d> <label>
# d is the parsed report; n is the checks keyed by name.
assert_json() {
  python3 -c "
import json, sys
try:
    d = json.load(open('$1'))
except Exception as e:
    print('FAIL: report is not valid JSON:', e); sys.exit(1)
n = {c['name']: c for c in d['checks']}
sys.exit(0 if ($2) else 1)
" || fail "$3"
}

### 1. All-green machine: exit 0, ready, no restart, every required check ok.
SB="$(mk_sandbox 1 1)"
RC="$(run_bs "$SB")"
[[ "$RC" == 0 ]] || fail "all-green report should exit 0, got $RC; stderr: $(cat "$SB/report.err")"
assert_json "$SB/report.json" \
  "d['ready'] is True and d['restart_needed'] is False and d['mode'] == 'report' and len(d['checks']) == 7" \
  "all-green top-level fields"
assert_json "$SB/report.json" \
  "all(c['state'] == 'ok' for c in d['checks'] if c['required'])" \
  "all-green required checks ok"
assert_json "$SB/report.json" \
  "(lambda n: n['path'] == '$SB/home/tools/fno/bin/fno-py' and n['path'].startswith('/'))(n['wheel'])" \
  "wheel check carries an absolute path"

### 2. Empty machine (no uv anywhere): exit 1, JSON still valid, fixes named.
SB="$(mk_sandbox 0 0)"
rm -f "$SB/bin/uv"
RC="$(run_bs "$SB")"
[[ "$RC" == 1 ]] || fail "empty machine should exit 1, got $RC"
assert_json "$SB/report.json" "d['ready'] is False" "empty machine not ready"
assert_json "$SB/report.json" \
  "n['uv']['state'] == 'missing' and 'astral.sh' in n['uv']['fix']" \
  "uv missing names the uv installer fix"
assert_json "$SB/report.json" \
  "n['wheel']['state'] == 'missing' and n['wheel']['fix']" \
  "wheel missing names a fix"
assert_json "$SB/report.json" \
  "n['rust']['state'] == 'optional_missing' and n['rust']['required'] is False" \
  "rust is optional and its absence is not a failure"

### 3. Binaries fine, tool bin not on PATH: still ready (absolute paths work),
###    but restart_needed must say so, and the fix carries the real path.
SB="$(mk_sandbox 1 0)"
RC="$(run_bs "$SB")"
[[ "$RC" == 0 ]] || fail "binaries-ok-but-off-PATH should still be ready (exit 0), got $RC"
assert_json "$SB/report.json" "d['restart_needed'] is True" "off-PATH sets restart_needed"
assert_json "$SB/report.json" \
  "n['path']['state'] == 'missing' and 'export PATH=\"$SB/home/tools-bin:\$PATH\"' == n['path']['fix']" \
  "path fix is the exact export with the absolute tool bin"

### 4. --from-source on a non-checkout: usage error, exit 2, no report expected.
SB="$(mk_sandbox 1 1)"
RC="$(run_bs "$SB" --from-source "$SB/not-a-checkout")"
[[ "$RC" == 2 ]] || fail "--from-source on a bogus checkout should exit 2, got $RC"

### 5. Repair delegates to postinstall.sh, never touches rc files, and a fresh
###    rc file set stays exactly as the sandbox seeded it.
SB="$(mk_sandbox 0 0)"
RC="$(run_bs "$SB" --repair)"
[[ "$RC" == 1 ]] || fail "repair on an empty machine should exit 1 (still not ready), got $RC"
[[ -f "$SB/postinstall-ran" ]] || fail "repair did not run the plugin's own postinstall.sh"
[[ "$(cat "$SB/home/.zshrc")" == '# user rc, do not touch' ]] || fail "repair modified .zshrc"
for rcfile in .bashrc .zprofile .bash_profile .profile; do
  [[ -e "$SB/home/$rcfile" ]] && fail "repair created $rcfile"
done
assert_json "$SB/report.json" \
  "d['mode'] == 'repair' and d['repair']['installer_exit'] == 0 and d['restart_needed'] is True" \
  "repair report names the installer exit and flags restart"

### 6. Unknown flag: exit 2.
SB="$(mk_sandbox 1 1)"
RC="$(run_bs "$SB" --bogus)"
[[ "$RC" == 2 ]] || fail "unknown flag should exit 2, got $RC"

### 7. --from-source on a checkout-shaped dir: mode runs, records itself and
###    the checkout in the report; the sandbox stubs install nothing, so the
###    machine is still not ready and the exit says so.
SB="$(mk_sandbox 0 0)"
mkdir -p "$SB/checkout/cli" "$SB/checkout/crates/fno" "$SB/checkout/crates/fno-agents"
touch "$SB/checkout/cli/pyproject.toml" "$SB/checkout/crates/fno/Cargo.toml" "$SB/checkout/crates/fno-agents/Cargo.toml"
RC="$(run_bs "$SB" --from-source "$SB/checkout")"
[[ "$RC" == 1 ]] || fail "from-source into a stub environment should exit 1 (nothing landed), got $RC"
assert_json "$SB/report.json" \
  "d['mode'] == 'from-source' and d['from_source']['ran'] is True and d['from_source']['checkout'] == '$SB/checkout'" \
  "from-source report records the run and the checkout"

echo "PASS: bootstrap report contract (7 cases)"
