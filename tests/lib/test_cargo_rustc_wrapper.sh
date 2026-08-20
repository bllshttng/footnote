#!/usr/bin/env bash
# tests/lib/test_cargo_rustc_wrapper.sh
#
# Two assertions on scripts/lib/cargo-rustc-wrapper.sh's path announcement:
#   T01 - sccache on PATH, invoked with `rustc -vV` (cargo's compiler probe)
#         -> stderr names sccache, stdout is untouched
#   T02 - sccache absent, an ordinary compile argv -> stderr is silent
#
# Both use a PATH-shadowing stub in place of the real sccache/rustc, so the
# test needs neither installed.
#
# Exit codes: 0 pass, 1 fail
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
WRAPPER="${REPO_ROOT}/scripts/lib/cargo-rustc-wrapper.sh"
# Captured before any PATH override below - both cases replace PATH
# entirely (not prefix it) so a real sccache already on this machine's
# PATH can never leak into the "absent" case, but bash itself must still
# resolve.
BASH_BIN="$(command -v bash)"

FAILURES=0
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*" >&2; FAILURES=$((FAILURES + 1)); }

[[ -x "$WRAPPER" ]] || { echo "FAIL: cargo-rustc-wrapper.sh not executable at $WRAPPER" >&2; exit 1; }
bash -n "$WRAPPER" || { echo "FAIL: cargo-rustc-wrapper.sh failed bash -n" >&2; exit 1; }

t01_sccache_present_announces_on_probe() {
  local stub_dir out_file err_file rc
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  cat > "$stub_dir/sccache" <<'STUB'
#!/usr/bin/env bash
echo "stub-sccache-stdout"
STUB
  chmod +x "$stub_dir/sccache"
  out_file="$stub_dir/out.txt"
  err_file="$stub_dir/err.txt"

  PATH="$stub_dir:$PATH" "$BASH_BIN" "$WRAPPER" /fake/rustc -vV >"$out_file" 2>"$err_file"
  rc=$?

  [[ "$rc" -eq 0 ]] || { fail "T01: expected rc=0, got $rc (stderr: $(cat "$err_file"))"; rm -rf "$stub_dir"; return; }
  grep -q "cargo-rustc-wrapper:.*sccache" "$err_file" \
    || { fail "T01: stderr does not name sccache: $(cat "$err_file")"; rm -rf "$stub_dir"; return; }
  grep -q "cargo-rustc-wrapper" "$out_file" \
    && { fail "T01: the announcement leaked onto stdout: $(cat "$out_file")"; rm -rf "$stub_dir"; return; }
  grep -q "stub-sccache-stdout" "$out_file" \
    || fail "T01: stdout does not carry the compiler's own output: $(cat "$out_file")"
  pass "T01 sccache on PATH: -vV probe announces sccache on stderr, stdout untouched"
  rm -rf "$stub_dir"
}

t02_sccache_absent_ordinary_compile_silent() {
  local stub_dir out_file err_file rc
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  out_file="$stub_dir/out.txt"
  err_file="$stub_dir/err.txt"

  # A real sccache may be installed on this machine's normal PATH (it is,
  # as of 2026-08-19), so "absent" needs a PATH that genuinely excludes it
  # rather than a stub merely prepended ahead of the real one.
  PATH="/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" /bin/echo compiling >"$out_file" 2>"$err_file"
  rc=$?

  [[ "$rc" -eq 0 ]] || { fail "T02: expected rc=0, got $rc (stderr: $(cat "$err_file"))"; rm -rf "$stub_dir"; return; }
  [[ -s "$err_file" ]] && { fail "T02: expected silent stderr on an ordinary compile argv, got: $(cat "$err_file")"; rm -rf "$stub_dir"; return; }
  grep -q "compiling" "$out_file" \
    || fail "T02: stdout does not carry the compiler's own output: $(cat "$out_file")"
  pass "T02 sccache absent: an ordinary compile argv is silent on stderr"
  rm -rf "$stub_dir"
}

t01_sccache_present_announces_on_probe
t02_sccache_absent_ordinary_compile_silent

echo ""
if [[ "$FAILURES" -eq 0 ]]; then
  echo "ALL TESTS PASSED (test_cargo_rustc_wrapper.sh)"
else
  echo "FAILED: $FAILURES test(s) failed" >&2
  exit 1
fi
