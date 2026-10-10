#!/usr/bin/env bash
# tests/lib/test_cargo_rustc_wrapper.sh
#
# The wrapper's contract, one test per behavior:
#   T01/T02 - the path announcement on a `-vV` probe; silence otherwise
#   T03-T05 - the compile door: probes never ask, a failing admission
#             builds anyway once per cargo, a signalled wait compiles nothing
#   T06-T08 - the --run door: the same three at the runner
#   T09/T10 - argv refusals
#   T11     - a build-script rustc (CARGO_CFG_* set) never asks
#   T12/T13 - a binary without the verb is named with its remedy and the
#             event is journaled, at both doors
#   T17-T19 - the sccache client watcher: a server that runs no compile is
#             stopped and the compile falls back; the bound spares a running
#             compile; a compile whose cargo is gone is reaped
#
# All use PATH-shadowing stubs in place of the real sccache/fno-agents/fno.
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

  SCCACHE_DISABLE=0 PATH="$stub_dir:$PATH" "$BASH_BIN" "$WRAPPER" /fake/rustc -vV >"$out_file" 2>"$err_file"
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
echo "\$* manifest=\${CARGO_MANIFEST_DIR:-}" >> "$calls"
STUB
  chmod +x "$stub_dir/fno-agents"

  PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" /bin/echo -vV >/dev/null 2>&1
  PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" /bin/echo --print=cfg >/dev/null 2>&1
  [[ -s "$calls" ]] && { fail "T03: a probe asked for admission: $(cat "$calls")"; rm -rf "$stub_dir"; return; }

  # Cargo sets CARGO_MANIFEST_DIR on the wrapper. The admission call must not
  # carry it, or the live-store fence refuses the claims store.
  CARGO_MANIFEST_DIR=/crate PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" /bin/echo compiling >/dev/null 2>&1
  rc=$?
  [[ "$rc" -eq 0 ]] || { fail "T03: expected rc=0, got $rc"; rm -rf "$stub_dir"; return; }
  grep -q "^test-run build-admit --cargo-pid [0-9][0-9]* --worktree ${REPO_ROOT} manifest=\$" "$calls" \
    || { fail "T03: compile did not ask admission without cargo's manifest dir: $(cat "$calls")"; rm -rf "$stub_dir"; return; }
  pass "T03 a compile asks build admission without CARGO_MANIFEST_DIR; -vV and --print probes do not"
  rm -rf "$stub_dir"
}

t04_admission_failure_builds_anyway() {
  local stub_dir out_file err_file rc
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  # The wrapper reads the verb's usage once after the failing admit, so the
  # stub answers two calls per cargo: a bare call (no --cargo-pid) gets the
  # usage refusal a binary with the verb prints, keeping T04 on the "error"
  # line rather than the "verb missing" line.
  cat > "$stub_dir/fno-agents" <<STUB
#!/usr/bin/env bash
echo called >> "$stub_dir/calls.txt"
case " \$* " in
  *--cargo-pid*) ;;
  *) echo "--cargo-pid is required" >&2 ;;
esac
exit 2
STUB
  chmod +x "$stub_dir/fno-agents"
  # The verb answered, so the journal row carries reason "error", not
  # "verb_missing" (AC4). A recording fno stub sits on PATH to catch it.
  fno_calls="$stub_dir/fno-calls.txt"
  cat > "$stub_dir/fno" <<STUB
#!/usr/bin/env bash
echo "\$*" >> "$fno_calls"
STUB
  chmod +x "$stub_dir/fno"
  out_file="$stub_dir/out.txt"
  err_file="$stub_dir/err.txt"

  # The wrapper's parent is this shell, standing in for cargo.
  TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" /bin/echo compiling >"$out_file" 2>"$err_file"
  rc=$?
  [[ "$rc" -eq 0 ]] || { fail "T04: expected rc=0, got $rc"; rm -rf "$stub_dir"; return; }
  grep -q "compiling" "$out_file" || { fail "T04: the compile did not run"; rm -rf "$stub_dir"; return; }
  grep -q "build admission unavailable (exit 2)" "$err_file" \
    || { fail "T04: stderr does not name the unadmitted build: $(cat "$err_file")"; rm -rf "$stub_dir"; return; }
  found=0
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    if [[ -s "$fno_calls" ]] \
      && grep -q "doctor event emit build_admission_unavailable" "$fno_calls" \
      && grep -q '"reason":"error"' "$fno_calls"; then
      found=1
      break
    fi
    sleep 0.2
  done
  [[ "$found" -eq 1 ]] \
    || { fail "T04: no build_admission_unavailable event with reason error: $(cat "$fno_calls" 2>/dev/null)"; rm -rf "$stub_dir"; return; }
  TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" /bin/echo compiling >"$out_file" 2>"$err_file"
  [[ "$(wc -l < "$stub_dir/calls.txt" | tr -d ' ')" == "2" ]] \
    || { fail "T04: a second compile under the same cargo asked again"; rm -rf "$stub_dir"; return; }
  [[ "$(wc -l < "$fno_calls" | tr -d ' ')" == "1" ]] \
    || { fail "T04: a second compile under the same cargo wrote a second event: $(cat "$fno_calls")"; rm -rf "$stub_dir"; return; }
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
echo "\$* manifest=\${CARGO_MANIFEST_DIR:-}" >> "$calls"
STUB
  chmod +x "$stub_dir/fno-agents"
  out_file="$stub_dir/out.txt"
  err_file="$stub_dir/err.txt"

  # The test binary keeps CARGO_MANIFEST_DIR: the live-store fence reads it to
  # keep tests off the operator store. Only the admission call drops it.
  CARGO_MANIFEST_DIR=/crate TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" --run /bin/sh -c 'echo ran "$@" "$CARGO_MANIFEST_DIR"; exit 7' x a b >"$out_file" 2>"$err_file"
  rc=$?
  [[ "$rc" -eq 7 ]] || { fail "T06: expected rc=7 (the binary's own exit), got $rc"; rm -rf "$stub_dir"; return; }
  grep -q '^ran a b /crate$' "$out_file" || { fail "T06: the binary did not run with its args and CARGO_MANIFEST_DIR: $(cat "$out_file")"; rm -rf "$stub_dir"; return; }
  grep -q "^test-run run-admit --cargo-pid [0-9][0-9]* --worktree ${REPO_ROOT} manifest=\$" "$calls" \
    || { fail "T06: the run door did not ask admission without cargo's manifest dir: $(cat "$calls")"; rm -rf "$stub_dir"; return; }
  pass "T06 --run asks run admission without CARGO_MANIFEST_DIR, then execs the binary with its args, env and exit code"
  rm -rf "$stub_dir"
}

t07_failed_run_admission_runs_anyway_once_per_cargo() {
  local stub_dir out_file err_file rc
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  # Same shape as T04: the bare usage read gets the marker a binary with
  # the verb prints, so T07 stays the "error" case.
  cat > "$stub_dir/fno-agents" <<STUB
#!/usr/bin/env bash
echo called >> "$stub_dir/calls.txt"
case " \$* " in
  *--cargo-pid*) ;;
  *) echo "--cargo-pid is required" >&2 ;;
esac
exit 2
STUB
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
  [[ "$(wc -l < "$stub_dir/calls.txt" | tr -d ' ')" == "2" ]] \
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

t11_build_script_probe_never_asks() {
  local stub_dir out_file rc
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  calls="$stub_dir/calls.txt"
  cat > "$stub_dir/fno-agents" <<STUB
#!/usr/bin/env bash
echo "\$*" >> "$calls"
STUB
  chmod +x "$stub_dir/fno-agents"
  out_file="$stub_dir/out.txt"

  # Cargo exports CARGO_CFG_* only to a build script's run, so this is the
  # thiserror-shaped probe: a rustc whose parent is build-script-build.
  CARGO_CFG_TARGET_ARCH=aarch64 PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" /bin/echo probe-compiles >"$out_file" 2>/dev/null
  rc=$?
  [[ "$rc" -eq 0 ]] || { fail "T11: expected rc=0, got $rc"; rm -rf "$stub_dir"; return; }
  grep -q "probe-compiles" "$out_file" || { fail "T11: the compile did not run"; rm -rf "$stub_dir"; return; }
  [[ -s "$calls" ]] && { fail "T11: a build-script rustc asked for admission: $(cat "$calls")"; rm -rf "$stub_dir"; return; }
  pass "T11 a build-script rustc (CARGO_CFG_* set) never asks and the compile runs"
  rm -rf "$stub_dir"
}

t12_missing_verb_is_named_and_journaled() {
  local stub_dir out_file err_file rc found
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  cat > "$stub_dir/fno-agents" <<'STUB'
#!/usr/bin/env bash
echo "fno-agents test-run: unrecognized argument before \`--\`: build-admit" >&2
exit 2
STUB
  fno_calls="$stub_dir/fno-calls.txt"
  cat > "$stub_dir/fno" <<STUB
#!/usr/bin/env bash
echo "\$*" >> "$fno_calls"
STUB
  chmod +x "$stub_dir/fno-agents" "$stub_dir/fno"
  out_file="$stub_dir/out.txt"
  err_file="$stub_dir/err.txt"

  TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" /bin/echo compiling >"$out_file" 2>"$err_file"
  rc=$?
  [[ "$rc" -eq 0 ]] || { fail "T12: expected rc=0, got $rc"; rm -rf "$stub_dir"; return; }
  grep -q "compiling" "$out_file" || { fail "T12: the compile did not run"; rm -rf "$stub_dir"; return; }
  grep -q "has no build-admit" "$err_file" \
    || { fail "T12: stderr does not name the missing verb: $(cat "$err_file")"; rm -rf "$stub_dir"; return; }
  grep -q "fno doctor update" "$err_file" \
    || { fail "T12: stderr does not name the remedy: $(cat "$err_file")"; rm -rf "$stub_dir"; return; }
  found=0
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    if [[ -s "$fno_calls" ]] \
      && grep -q "doctor event emit build_admission_unavailable" "$fno_calls" \
      && grep -q '"reason":"verb_missing"' "$fno_calls"; then
      found=1
      break
    fi
    sleep 0.2
  done
  [[ "$found" -eq 1 ]] \
    || { fail "T12: no build_admission_unavailable event with reason verb_missing: $(cat "$fno_calls" 2>/dev/null)"; rm -rf "$stub_dir"; return; }
  TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" /bin/echo compiling >"$out_file" 2>"$err_file"
  [[ "$(wc -l < "$fno_calls" | tr -d ' ')" == "1" ]] \
    || { fail "T12: a second compile under the same cargo wrote a second event: $(cat "$fno_calls")"; rm -rf "$stub_dir"; return; }
  [[ -s "$err_file" ]] && { fail "T12: the second compile repeated the warning: $(cat "$err_file")"; rm -rf "$stub_dir"; return; }
  pass "T12 a missing verb is named with its remedy and journaled once per cargo"
  rm -rf "$stub_dir"
}

t13_run_door_missing_verb_is_named() {
  local stub_dir out_file err_file rc found
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  cat > "$stub_dir/fno-agents" <<'STUB'
#!/usr/bin/env bash
echo "fno-agents test-run: unrecognized argument before \`--\`: run-admit" >&2
exit 2
STUB
  fno_calls="$stub_dir/fno-calls.txt"
  cat > "$stub_dir/fno" <<STUB
#!/usr/bin/env bash
echo "\$*" >> "$fno_calls"
STUB
  chmod +x "$stub_dir/fno-agents" "$stub_dir/fno"
  out_file="$stub_dir/out.txt"
  err_file="$stub_dir/err.txt"

  TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" --run /bin/echo running >"$out_file" 2>"$err_file"
  rc=$?
  [[ "$rc" -eq 0 ]] || { fail "T13: expected rc=0, got $rc"; rm -rf "$stub_dir"; return; }
  grep -q "running" "$out_file" || { fail "T13: the binary did not run"; rm -rf "$stub_dir"; return; }
  grep -q "has no run-admit" "$err_file" \
    || { fail "T13: stderr does not name the missing verb: $(cat "$err_file")"; rm -rf "$stub_dir"; return; }
  found=0
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    if [[ -s "$fno_calls" ]] \
      && grep -q "doctor event emit run_admission_unavailable" "$fno_calls" \
      && grep -q '"reason":"verb_missing"' "$fno_calls"; then
      found=1
      break
    fi
    sleep 0.2
  done
  [[ "$found" -eq 1 ]] \
    || { fail "T13: no run_admission_unavailable event with reason verb_missing: $(cat "$fno_calls" 2>/dev/null)"; rm -rf "$stub_dir"; return; }
  pass "T13 the run door names the missing run-admit and journals the event"
  rm -rf "$stub_dir"
}

# T14/T15: a slot-busy refusal (exit 86) is policy, not breakage. The wrapper
# exits with it at both doors: the compile or run stops, the door's answer
# reaches the worker, and the fail-open "admission unavailable" path never
# fires - building or running unadmitted under the very saturation the gate
# exists to cap is the one wrong answer here.
t14_slot_busy_refuses_the_compile() {
  local stub_dir out_file err_file rc
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  printf '#!/usr/bin/env bash\necho "[cargo slots] every run slot is busy: commit, push, CI runs it." >&2\nexit 86\n' > "$stub_dir/fno-agents"
  chmod +x "$stub_dir/fno-agents"
  out_file="$stub_dir/out.txt"
  err_file="$stub_dir/err.txt"

  TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" /bin/echo compiling >"$out_file" 2>"$err_file"
  rc=$?
  [[ "$rc" -eq 86 ]] || { fail "T14: expected rc=86, got $rc"; rm -rf "$stub_dir"; return; }
  grep -q "compiling" "$out_file" && { fail "T14: a slot-busy refusal still compiled"; rm -rf "$stub_dir"; return; }
  grep -q "commit, push, CI runs it" "$err_file" \
    || { fail "T14: stderr does not carry the door's answer: $(cat "$err_file")"; rm -rf "$stub_dir"; return; }
  pass "T14 a slot-busy refusal stops the compile and carries the door's answer"
  rm -rf "$stub_dir"
}

t15_slot_busy_refuses_the_run() {
  local stub_dir out_file err_file rc
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  printf '#!/usr/bin/env bash\necho "[cargo slots] every run slot is busy: commit, push, CI runs it." >&2\nexit 86\n' > "$stub_dir/fno-agents"
  chmod +x "$stub_dir/fno-agents"
  out_file="$stub_dir/out.txt"
  err_file="$stub_dir/err.txt"

  TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" "$BASH_BIN" "$WRAPPER" --run /bin/echo running >"$out_file" 2>"$err_file"
  rc=$?
  [[ "$rc" -eq 86 ]] || { fail "T15: expected rc=86, got $rc"; rm -rf "$stub_dir"; return; }
  grep -q "running" "$out_file" && { fail "T15: a slot-busy refusal still ran the binary"; rm -rf "$stub_dir"; return; }
  grep -q "commit, push, CI runs it" "$err_file" \
    || { fail "T15: stderr does not carry the door's answer: $(cat "$err_file")"; rm -rf "$stub_dir"; return; }
  pass "T15 a slot-busy refusal stops the run and carries the door's answer"
  rm -rf "$stub_dir"
}

t16_wrapper_exports_never_stop_idle_timeout() {
  local stub_dir out_file err_file rc idle
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  cat > "$stub_dir/sccache" <<'STUB'
#!/usr/bin/env bash
printf '%s' "${SCCACHE_IDLE_TIMEOUT-unset}"
STUB
  chmod +x "$stub_dir/sccache"
  out_file="$stub_dir/out.txt"
  err_file="$stub_dir/err.txt"

  SCCACHE_DISABLE=0 PATH="$stub_dir:$PATH" "$BASH_BIN" "$WRAPPER" /fake/rustc -vV >"$out_file" 2>"$err_file"
  rc=$?
  idle="$(cat "$out_file")"

  [[ "$rc" -eq 0 ]] || { fail "T16: expected rc=0, got $rc (stderr: $(cat "$err_file"))"; rm -rf "$stub_dir"; return; }
  [[ "$idle" == "0" ]] \
    || { fail "T16: the compiler saw SCCACHE_IDLE_TIMEOUT=$idle, expected 0"; rm -rf "$stub_dir"; return; }

  SCCACHE_DISABLE=0 PATH="$stub_dir:$PATH" SCCACHE_IDLE_TIMEOUT=3 "$BASH_BIN" "$WRAPPER" /fake/rustc -vV >"$out_file" 2>"$err_file"
  idle="$(cat "$out_file")"
  [[ "$idle" == "3" ]] \
    || fail "T16: an operator override SCCACHE_IDLE_TIMEOUT=3 became $idle; it must survive"
  pass "T16 the wrapper exports the never-stop idle timeout and keeps an override"
  rm -rf "$stub_dir"
}

# T17-T19 share one stub: `ps -A` prints a scripted process table, so the
# watcher reads a fake server and its children whatever sccache this machine
# runs. Every pid a test expects stopped is a real `sleep` the test owns.
watch_stubs() {
  local stub_dir="$1" client_body="$2"
  cat > "$stub_dir/ps" <<STUB
#!/usr/bin/env bash
if [[ "\${1:-}" == "-A" ]]; then cat "$stub_dir/table.txt"; exit 0; fi
exec /bin/ps "\$@"
STUB
  printf '#!/usr/bin/env bash\necho $$ > "%s/client.pid"\n%s\n' "$stub_dir" "$client_body" > "$stub_dir/sccache"
  chmod +x "$stub_dir/ps" "$stub_dir/sccache"
}

# A stopped process can linger as a zombie until init reaps it, and `kill -0`
# still finds a zombie. Two seconds covers the reap.
gone() {
  local state
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    state="$(/bin/ps -o stat= -p "$1" 2>/dev/null)"
    state="${state// /}"
    [[ -z "$state" || "$state" == Z* ]] && return 0
    sleep 0.2
  done
  return 1
}

t17_quiet_server_is_stopped_and_compile_falls_back() {
  local stub_dir out_file err_file rc server start
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  watch_stubs "$stub_dir" 'exec sleep 30'
  server="$(sleep 30 >/dev/null 2>&1 & echo $!)"
  printf '%s 1 /opt/bin/sccache\n' "$server" > "$stub_dir/table.txt"
  out_file="$stub_dir/out.txt"
  err_file="$stub_dir/err.txt"

  start=$SECONDS
  CI=1 SCCACHE_DISABLE=0 FNO_SCCACHE_STALL_SECS=2 TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" \
    "$BASH_BIN" "$WRAPPER" /bin/echo compiling >"$out_file" 2>"$err_file"
  rc=$?
  [[ "$rc" -eq 0 ]] || { fail "T17: expected rc=0, got $rc (stderr: $(cat "$err_file"))"; kill "$server" 2>/dev/null; rm -rf "$stub_dir"; return; }
  grep -q "compiling" "$out_file" || { fail "T17: the compile did not fall back to bare rustc"; kill "$server" 2>/dev/null; rm -rf "$stub_dir"; return; }
  grep -q "ran no compile for 2s" "$err_file" || { fail "T17: stderr does not name the stall: $(cat "$err_file")"; kill "$server" 2>/dev/null; rm -rf "$stub_dir"; return; }
  [[ $((SECONDS - start)) -lt 15 ]] || { fail "T17: the fallback took $((SECONDS - start))s"; kill "$server" 2>/dev/null; rm -rf "$stub_dir"; return; }
  gone "$server" || { fail "T17: the quiet server is still alive"; kill "$server" 2>/dev/null; rm -rf "$stub_dir"; return; }
  gone "$(cat "$stub_dir/client.pid")" || { fail "T17: the parked client is still alive"; rm -rf "$stub_dir"; return; }
  pass "T17 a server that runs no compile for the stall window is stopped and the compile falls back"
  rm -rf "$stub_dir"
}

t18_bound_spares_a_running_compile_only() {
  local stub_dir out_file err_file rc
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  watch_stubs "$stub_dir" 'sleep 4; echo client-done'
  # The server is busy, so the stall check never fires. Its child is another
  # crate's: this compile has not started, and the bound ends the wait.
  printf '7001 1 /opt/bin/sccache\n7002 7001 rustc --crate-name other -C extra-filename=-other\n' > "$stub_dir/table.txt"
  out_file="$stub_dir/out.txt"
  err_file="$stub_dir/err.txt"
  CI=1 SCCACHE_DISABLE=0 FNO_SCCACHE_CLIENT_TIMEOUT_SECS=2 FNO_SCCACHE_STALL_SECS=0 TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" \
    "$BASH_BIN" "$WRAPPER" /bin/echo compiling -C extra-filename=-mine >"$out_file" 2>"$err_file"
  rc=$?
  [[ "$rc" -eq 0 ]] || { fail "T18: expected rc=0, got $rc (stderr: $(cat "$err_file"))"; rm -rf "$stub_dir"; return; }
  grep -q "did not start this compile in 2s" "$err_file" \
    || { fail "T18: the bound did not fire for an unstarted compile: $(cat "$err_file")"; rm -rf "$stub_dir"; return; }
  grep -q "client-done" "$out_file" && { fail "T18: the client answered after the bound"; rm -rf "$stub_dir"; return; }

  # Now the server runs this crate's rustc: the compile is live, so the bound
  # leaves it to finish.
  printf '7001 1 /opt/bin/sccache\n7002 7001 rustc --crate-name mine -C extra-filename=-mine\n' > "$stub_dir/table.txt"
  CI=1 SCCACHE_DISABLE=0 FNO_SCCACHE_CLIENT_TIMEOUT_SECS=2 FNO_SCCACHE_STALL_SECS=0 TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" \
    "$BASH_BIN" "$WRAPPER" /bin/echo compiling -C extra-filename=-mine >"$out_file" 2>"$err_file"
  rc=$?
  [[ "$rc" -eq 0 ]] || { fail "T18: expected rc=0 for the running compile, got $rc"; rm -rf "$stub_dir"; return; }
  grep -q "client-done" "$out_file" || { fail "T18: the bound stopped a running compile: $(cat "$err_file")"; rm -rf "$stub_dir"; return; }
  grep -q "compiling" "$out_file" && { fail "T18: a running compile also fell back to bare rustc"; rm -rf "$stub_dir"; return; }
  pass "T18 the bound ends a wait for an unstarted compile and spares a running one"
  rm -rf "$stub_dir"
}

t19_compile_whose_cargo_is_gone_is_reaped() {
  local stub_dir ours wrapper_pid
  stub_dir="$(mktemp -d -t cargo-wrapper-test-XXXXXX)"
  watch_stubs "$stub_dir" 'exec sleep 30'
  ours="$(sleep 30 >/dev/null 2>&1 & echo $!)"
  printf '7001 1 /opt/bin/sccache\n%s 7001 rustc --crate-name mine -C extra-filename=-mine\n' "$ours" > "$stub_dir/table.txt"

  # The middle shell stands in for cargo and exits at once, so the wrapper is
  # orphaned before its first poll.
  CI=1 SCCACHE_DISABLE=0 FNO_SCCACHE_STALL_SECS=2 TMPDIR="$stub_dir" PATH="$stub_dir:/usr/bin:/bin" \
    "$BASH_BIN" -c '"$0" "$1" /bin/echo compiling -C extra-filename=-mine >"$2/out.txt" 2>&1 & echo $! > "$2/wrapper.pid"' \
    "$BASH_BIN" "$WRAPPER" "$stub_dir"
  wrapper_pid="$(cat "$stub_dir/wrapper.pid")"
  for _ in $(seq 1 50); do
    kill -0 "$wrapper_pid" 2>/dev/null || break
    sleep 0.2
  done
  if kill -0 "$wrapper_pid" 2>/dev/null; then
    fail "T19: the orphaned wrapper is still running"; kill "$wrapper_pid" "$ours" 2>/dev/null; rm -rf "$stub_dir"; return
  fi
  gone "$ours" || { fail "T19: the server-side rustc for this crate is still running"; kill "$ours" 2>/dev/null; rm -rf "$stub_dir"; return; }
  gone "$(cat "$stub_dir/client.pid")" || { fail "T19: the client is still running"; rm -rf "$stub_dir"; return; }
  grep -q "^compiling" "$stub_dir/out.txt" && { fail "T19: an orphaned compile fell back to bare rustc"; rm -rf "$stub_dir"; return; }
  grep -q "cargo (pid [0-9]*) is gone" "$stub_dir/out.txt" || { fail "T19: no orphan line: $(cat "$stub_dir/out.txt")"; rm -rf "$stub_dir"; return; }
  pass "T19 a compile whose cargo is gone stops its client and its server-side rustc"
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
t11_build_script_probe_never_asks
t12_missing_verb_is_named_and_journaled
t13_run_door_missing_verb_is_named
t14_slot_busy_refuses_the_compile
t15_slot_busy_refuses_the_run
t16_wrapper_exports_never_stop_idle_timeout
t17_quiet_server_is_stopped_and_compile_falls_back
t18_bound_spares_a_running_compile_only
t19_compile_whose_cargo_is_gone_is_reaped

echo ""
if [[ "$FAILURES" -eq 0 ]]; then
  echo "ALL TESTS PASSED (test_cargo_rustc_wrapper.sh)"
else
  echo "FAILED: $FAILURES test(s) failed" >&2
  exit 1
fi
