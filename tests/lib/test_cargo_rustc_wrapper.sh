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

t03_compile_asks_admission_and_probe_does_not() {
  local stub_dir calls rc
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  calls="$stub_dir/calls.txt"
  cat > "$stub_dir/fno-agents" <<STUB
#!/usr/bin/env bash
echo "\$*" >> "$calls"
STUB
  chmod +x "$stub_dir/fno-agents"

  PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" /bin/echo -vV >/dev/null 2>&1
  PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" /bin/echo --print=cfg >/dev/null 2>&1
  [[ -s "$calls" ]] && { fail "T03: a probe asked for admission: $(cat "$calls")"; rm -rf "$stub_dir"; return; }

  PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" /bin/echo compiling >/dev/null 2>&1
  rc=$?
  [[ "$rc" -eq 0 ]] || { fail "T03: expected rc=0, got $rc"; rm -rf "$stub_dir"; return; }
  grep -q "^test-run build-admit --cargo-pid [0-9][0-9]* --worktree ${REPO_ROOT}\$" "$calls" \
    || { fail "T03: compile did not ask admission as expected: $(cat "$calls")"; rm -rf "$stub_dir"; return; }
  pass "T03 a compile asks build admission; -vV and --print probes do not"
  rm -rf "$stub_dir"
}

t04_admission_failure_builds_anyway() {
  local stub_dir out_file err_file rc
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  printf '#!/usr/bin/env bash\necho called >> "%s/calls.txt"\nexit 2\n' "$stub_dir" > "$stub_dir/fno-agents"
  chmod +x "$stub_dir/fno-agents"
  out_file="$stub_dir/out.txt"
  err_file="$stub_dir/err.txt"

  # The wrapper's parent is this shell, standing in for cargo.
  TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" /bin/echo compiling >"$out_file" 2>"$err_file"
  rc=$?
  [[ "$rc" -eq 0 ]] || { fail "T04: expected rc=0, got $rc"; rm -rf "$stub_dir"; return; }
  grep -q "compiling" "$out_file" || { fail "T04: the compile did not run"; rm -rf "$stub_dir"; return; }
  grep -q "build admission unavailable (exit 2)" "$err_file" \
    || { fail "T04: stderr does not name the unadmitted build: $(cat "$err_file")"; rm -rf "$stub_dir"; return; }
  TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" /bin/echo compiling >"$out_file" 2>"$err_file"
  [[ "$(wc -l < "$stub_dir/calls.txt" | tr -d ' ')" == "1" ]] \
    || { fail "T04: a second compile under the same cargo asked again"; rm -rf "$stub_dir"; return; }
  [[ -s "$err_file" ]] && { fail "T04: the second compile repeated the warning: $(cat "$err_file")"; rm -rf "$stub_dir"; return; }
  pass "T04 a failing admission is named once per cargo and every compile still runs"
  rm -rf "$stub_dir"
}

t05_signalled_admission_compiles_nothing() {
  local stub_dir out_file rc
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  printf '#!/usr/bin/env bash\nexit 130\n' > "$stub_dir/fno-agents"
  chmod +x "$stub_dir/fno-agents"
  out_file="$stub_dir/out.txt"

  TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" /bin/echo compiling >"$out_file" 2>/dev/null
  rc=$?
  [[ "$rc" -eq 130 ]] || { fail "T05: expected rc=130, got $rc"; rm -rf "$stub_dir"; return; }
  grep -q "compiling" "$out_file" && { fail "T05: a signalled wait still compiled"; rm -rf "$stub_dir"; return; }
  pass "T05 a wait stopped by a signal exits with it and compiles nothing"
  rm -rf "$stub_dir"
}

t06_run_door_admits_then_execs() {
  local stub_dir out_file err_file rc
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  calls="$stub_dir/calls.txt"
  cat > "$stub_dir/fno-agents" <<STUB
#!/usr/bin/env bash
echo "\$*" >> "$calls"
STUB
  chmod +x "$stub_dir/fno-agents"
  out_file="$stub_dir/out.txt"
  err_file="$stub_dir/err.txt"

  TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" --run /bin/sh -c 'echo ran "$@"; exit 7' x a b >"$out_file" 2>"$err_file"
  rc=$?
  [[ "$rc" -eq 7 ]] || { fail "T06: expected rc=7 (the binary's own exit), got $rc"; rm -rf "$stub_dir"; return; }
  grep -q '^ran a b$' "$out_file" || { fail "T06: the binary did not run with its args: $(cat "$out_file")"; rm -rf "$stub_dir"; return; }
  grep -q "^test-run run-admit --cargo-pid [0-9][0-9]* --worktree ${REPO_ROOT}\$" "$calls" \
    || { fail "T06: the run door did not ask admission: $(cat "$calls")"; rm -rf "$stub_dir"; return; }
  pass "T06 --run asks run admission, then execs the binary with its own args and exit code"
  rm -rf "$stub_dir"
}

t07_failed_run_admission_runs_anyway_once_per_cargo() {
  local stub_dir out_file err_file rc
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  printf '#!/usr/bin/env bash\necho called >> "%s/calls.txt"\nexit 2\n' "$stub_dir" > "$stub_dir/fno-agents"
  chmod +x "$stub_dir/fno-agents"
  out_file="$stub_dir/out.txt"
  err_file="$stub_dir/err.txt"

  # The wrapper's parent is this shell, standing in for cargo.
  TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" --run /bin/echo running >"$out_file" 2>"$err_file"
  rc=$?
  [[ "$rc" -eq 0 ]] || { fail "T07: expected rc=0, got $rc"; rm -rf "$stub_dir"; return; }
  grep -q "running" "$out_file" || { fail "T07: the binary did not run"; rm -rf "$stub_dir"; return; }
  grep -q "run admission unavailable (exit 2)" "$err_file" \
    || { fail "T07: stderr does not name the unadmitted run: $(cat "$err_file")"; rm -rf "$stub_dir"; return; }
  TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" --run /bin/echo running >"$out_file" 2>"$err_file"
  [[ "$(wc -l < "$stub_dir/calls.txt" | tr -d ' ')" == "1" ]] \
    || { fail "T07: a second run under the same parent asked again"; rm -rf "$stub_dir"; return; }
  [[ -s "$err_file" ]] && { fail "T07: the second run repeated the warning: $(cat "$err_file")"; rm -rf "$stub_dir"; return; }
  pass "T07 a failing run admission is named once per cargo and every run still executes"
  rm -rf "$stub_dir"
}

t08_signalled_run_admission_runs_nothing() {
  local stub_dir out_file rc
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  printf '#!/usr/bin/env bash\nexit 130\n' > "$stub_dir/fno-agents"
  chmod +x "$stub_dir/fno-agents"
  out_file="$stub_dir/out.txt"

  TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" --run /bin/echo running >"$out_file" 2>/dev/null
  rc=$?
  [[ "$rc" -eq 130 ]] || { fail "T08: expected rc=130, got $rc"; rm -rf "$stub_dir"; return; }
  grep -q "running" "$out_file" && { fail "T08: a signalled wait still ran the binary"; rm -rf "$stub_dir"; return; }
  pass "T08 a run wait stopped by a signal exits with it and runs nothing"
  rm -rf "$stub_dir"
}

t09_run_without_a_program_refuses() {
  local err_file rc
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  err_file="$stub_dir/err.txt"
  TMPDIR="$stub_dir" PATH="/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" --run 2>"$err_file"
  rc=$?
  [[ "$rc" -eq 2 ]] || { fail "T09: expected rc=2, got $rc"; rm -rf "$stub_dir"; return; }
  grep -q -e "--run needs a program" "$err_file" || { fail "T09: stderr does not name the refusal: $(cat "$err_file")"; rm -rf "$stub_dir"; return; }
  pass "T09 --run with no program refuses with exit 2"
  rm -rf "$stub_dir"
}

t10_joined_runner_arrays_run_the_binary_once() {
  local stub_dir out_file rc
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  calls="$stub_dir/calls.txt"
  printf '#!/usr/bin/env bash\necho "$*" >> "%s"\n' "$calls" > "$stub_dir/fno-agents"
  chmod +x "$stub_dir/fno-agents"
  out_file="$stub_dir/out.txt"

  # A nested worktree hands cargo both runner arrays joined. The repeat names
  # the wrapper by a relative path that does not exist from the crate dir.
  (cd "$stub_dir" && TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" \
    --run scripts/lib/cargo-rustc-wrapper.sh --run /bin/sh -c 'echo ran "$@"' x a b) >"$out_file" 2>/dev/null
  rc=$?
  [[ "$rc" -eq 0 ]] || { fail "T10: expected rc=0, got $rc"; rm -rf "$stub_dir"; return; }
  grep -q '^ran a b$' "$out_file" || { fail "T10: the binary did not run with its args: $(cat "$out_file")"; rm -rf "$stub_dir"; return; }
  [[ "$(wc -l < "$calls" | tr -d ' ')" == "1" ]] \
    || { fail "T10: expected one run admission, got: $(cat "$calls")"; rm -rf "$stub_dir"; return; }
  pass "T10 joined runner arrays drop the repeated wrapper and run the binary once"
  rm -rf "$stub_dir"
}

t01_sccache_present_announces_on_probe
t02_sccache_absent_ordinary_compile_silent
t03_compile_asks_admission_and_probe_does_not
t04_admission_failure_builds_anyway
t05_signalled_admission_compiles_nothing
t06_run_door_admits_then_execs
t07_failed_run_admission_runs_anyway_once_per_cargo
t08_signalled_run_admission_runs_nothing
t09_run_without_a_program_refuses
t10_joined_runner_arrays_run_the_binary_once

echo ""
if [[ "$FAILURES" -eq 0 ]]; then
  echo "ALL TESTS PASSED (test_cargo_rustc_wrapper.sh)"
else
  echo "FAILED: $FAILURES test(s) failed" >&2
  exit 1
fi
