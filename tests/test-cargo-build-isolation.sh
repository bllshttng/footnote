#!/usr/bin/env bash
# Proves repository Cargo builds keep worktree-local targets while the optional
# compiler cache is bounded and fail-open.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WRAPPER="$ROOT/scripts/lib/cargo-rustc-wrapper.sh"
CONFIG="$ROOT/.cargo/config.toml"
PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1 :: $2"; FAIL=$((FAIL + 1)); }

echo "== repository wrapper contract =="
if [[ -x "$WRAPPER" ]]; then pass "rustc wrapper exists and is executable"; else fail "rustc wrapper" "missing executable $WRAPPER"; fi
if [[ -f "$CONFIG" ]] && grep -q 'rustc-wrapper = "scripts/lib/cargo-rustc-wrapper.sh"' "$CONFIG"; then pass "Cargo config selects repository wrapper"; else fail "Cargo config" "missing repository rustc-wrapper"; fi

TMP=$(mktemp -d -t cargo-isolation.XXXXXX)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/bin" "$TMP/logs"

cat > "$TMP/compiler" <<'EOF'
#!/usr/bin/env bash
printf '%s|%s\n' "${SCCACHE_CACHE_SIZE:-unset}" "$*" >> "$COMPILER_LOG"
exit 0
EOF
cat > "$TMP/bin/sccache" <<'EOF'
#!/usr/bin/env bash
printf '%s|%s\n' "${SCCACHE_CACHE_SIZE:-unset}" "$*" >> "$SCCACHE_LOG"
printf '%s|%s|%s\n' "${CARGO_BUILD_BUILD_DIR-unset}" "${CARGO_BUILD_TARGET_DIR-unset}" "${CARGO_TARGET_DIR-unset}" >> "${CARGO_PATH_LOG:-/dev/null}"
exec "$@"
EOF
chmod +x "$TMP/compiler" "$TMP/bin/sccache"

if [[ -x "$WRAPPER" ]]; then
  COMPILER_LOG="$TMP/logs/compiler-direct" PATH="/usr/bin:/bin" "$WRAPPER" "$TMP/compiler" --crate-name direct
  if grep -q '^unset|--crate-name direct$' "$TMP/logs/compiler-direct"; then pass "missing sccache falls through to real compiler"; else fail "direct fallback" "compiler receipt missing"; fi

  COMPILER_LOG="$TMP/logs/compiler-cache" SCCACHE_LOG="$TMP/logs/sccache-default" PATH="$TMP/bin:/usr/bin:/bin" "$WRAPPER" "$TMP/compiler" --crate-name cached
  if grep -q '^30G|' "$TMP/logs/sccache-default"; then pass "sccache defaults to bounded 30G cache"; else fail "sccache default cap" "30G receipt missing"; fi
  if grep -q -- '--crate-name cached' "$TMP/logs/compiler-cache"; then pass "sccache preserves the rustc invocation"; else fail "sccache argv" "compiler did not receive original argv"; fi

  COMPILER_LOG="$TMP/logs/compiler-override" SCCACHE_LOG="$TMP/logs/sccache-override" SCCACHE_CACHE_SIZE=3G PATH="$TMP/bin:/usr/bin:/bin" "$WRAPPER" "$TMP/compiler" --crate-name override
  if grep -q '^3G|' "$TMP/logs/sccache-override"; then pass "operator sccache cap is preserved"; else fail "sccache override" "3G receipt missing"; fi

  COMPILER_LOG="$TMP/logs/compiler-paths" SCCACHE_LOG="$TMP/logs/sccache-paths" CARGO_PATH_LOG="$TMP/logs/cargo-paths" CARGO_BUILD_BUILD_DIR="$TMP/bd" CARGO_BUILD_TARGET_DIR="$TMP/td" CARGO_TARGET_DIR="$TMP/td2" PATH="$TMP/bin:/usr/bin:/bin" "$WRAPPER" "$TMP/compiler" --crate-name shared-key
  if grep -q '^unset|unset|unset$' "$TMP/logs/cargo-paths" && grep -q -- '--crate-name shared-key' "$TMP/logs/compiler-paths"; then pass "sccache key drops per-run cargo path vars"; else fail "cargo path vars in key" "sccache saw a per-run path or lost argv"; fi

  if COMPILER_LOG="$TMP/logs/compiler-direct-paths" CARGO_BUILD_BUILD_DIR="$TMP/bd" CARGO_BUILD_TARGET_DIR="$TMP/td" CARGO_TARGET_DIR="$TMP/td2" PATH="/usr/bin:/bin" "$WRAPPER" "$TMP/compiler" --crate-name direct-paths && grep -q '^unset|--crate-name direct-paths$' "$TMP/logs/compiler-direct-paths"; then pass "direct fallback unaffected by cargo path vars"; else fail "direct fallback" "receipt or exit missing with cargo path vars set"; fi
fi

echo "== fallback-base writer naming =="
if [[ -x "$WRAPPER" ]]; then
  FB_HOME="$TMP/home"
  FB_TMP="$TMP/tmp"
  mkdir -p "$FB_HOME" "$FB_TMP"
  FB_LOG="$FB_HOME/.fno/logs/cargo-fallback-writers.log"
  FB_ENV=(-u CI -u CARGO_BUILD_BUILD_DIR HOME="$FB_HOME" TMPDIR="$FB_TMP" PATH="/usr/bin:/bin")

  # Two env-less builds under one parent pid: the log gains exactly one line
  # naming that pid, and stderr carries the remedy once.
  env "${FB_ENV[@]}" COMPILER_LOG="$TMP/logs/fb-compiler-1" "$WRAPPER" "$TMP/compiler" --crate-name fb-one 2>"$TMP/logs/fb-stderr-1"
  RC1=$?
  env "${FB_ENV[@]}" COMPILER_LOG="$TMP/logs/fb-compiler-2" "$WRAPPER" "$TMP/compiler" --crate-name fb-two 2>"$TMP/logs/fb-stderr-2"
  RC2=$?
  if [[ $RC1 -eq 0 && $RC2 -eq 0 ]]; then pass "env-less builds still compile"; else fail "fallback writer rc" "rc1=$RC1 rc2=$RC2"; fi
  if grep -q 'fallback base' "$TMP/logs/fb-stderr-1" && ! grep -q 'fallback base' "$TMP/logs/fb-stderr-2"; then pass "remedy printed once on stderr"; else fail "stderr remedy" "first: $(grep -c 'fallback base' "$TMP/logs/fb-stderr-1" || true) second: $(grep -c 'fallback base' "$TMP/logs/fb-stderr-2" || true)"; fi
  if grep -q -- '--crate-name fb-one' "$TMP/logs/fb-compiler-1" && grep -q -- '--crate-name fb-two' "$TMP/logs/fb-compiler-2"; then pass "compiler receipts present for env-less builds"; else fail "compiler receipts" "missing"; fi
  if [[ -f "$FB_LOG" && "$(wc -l <"$FB_LOG" | tr -d ' ')" -eq 1 ]] && grep -q "cargo_pid=$$" "$FB_LOG" && grep -q "cwd=" "$FB_LOG" && grep -q "cargo=" "$FB_LOG" && grep -q "parent=" "$FB_LOG"; then pass "log names the env-less cargo once"; else fail "fallback log" "$(cat "$FB_LOG" 2>/dev/null || echo missing)"; fi

  # An env preset, CI, or a -vV probe: nothing is logged.
  env -u CI CARGO_BUILD_BUILD_DIR="$TMP/bd" HOME="$FB_HOME" TMPDIR="$FB_TMP" PATH="/usr/bin:/bin" COMPILER_LOG="$TMP/logs/fb-compiler-preset" "$WRAPPER" "$TMP/compiler" --crate-name fb-preset 2>"$TMP/logs/fb-stderr-preset"
  if grep -q -- '--crate-name fb-preset' "$TMP/logs/fb-compiler-preset"; then pass "preset-env build still compiles"; else fail "preset env" "receipt missing"; fi
  if ! grep -q 'fallback base' "$TMP/logs/fb-stderr-preset"; then pass "preset env prints no remedy"; else fail "preset stderr" "remedy printed"; fi
  if [[ "$(wc -l <"$FB_LOG" | tr -d ' ')" -eq 1 ]]; then pass "preset env logs nothing"; else fail "preset env logged" "$(cat "$FB_LOG" 2>/dev/null || true)"; fi

  env "${FB_ENV[@]}" CI=true COMPILER_LOG="$TMP/logs/fb-compiler-ci" "$WRAPPER" "$TMP/compiler" --crate-name fb-ci 2>"$TMP/logs/fb-stderr-ci"
  if [[ "$(wc -l <"$FB_LOG" | tr -d ' ')" -eq 1 ]]; then pass "CI build logs nothing"; else fail "CI logged" "$(cat "$FB_LOG" 2>/dev/null || true)"; fi
  if ! grep -q 'fallback base' "$TMP/logs/fb-stderr-ci"; then pass "CI build prints no remedy"; else fail "CI stderr" "remedy printed"; fi

  env "${FB_ENV[@]}" COMPILER_LOG="$TMP/logs/fb-compiler-vv" "$WRAPPER" "$TMP/compiler" -vV 2>"$TMP/logs/fb-stderr-vv"
  if [[ "$(wc -l <"$FB_LOG" | tr -d ' ')" -eq 1 ]]; then pass "version probe logs nothing"; else fail "version probe logged" "$(cat "$FB_LOG" 2>/dev/null || true)"; fi

  # An unwritable HOME: the compiler still runs and the wrapper exits 0.
  RO_HOME="$TMP/ro-home"
  mkdir -p "$RO_HOME"
  chmod 500 "$RO_HOME"
  env "${FB_ENV[@]}" HOME="$RO_HOME" COMPILER_LOG="$TMP/logs/fb-compiler-ro" "$WRAPPER" "$TMP/compiler" --crate-name fb-ro 2>"$TMP/logs/fb-stderr-ro"
  if [[ $? -eq 0 ]] && grep -q -- '--crate-name fb-ro' "$TMP/logs/fb-compiler-ro"; then pass "unwritable HOME still compiles"; else fail "unwritable HOME" "rc or receipt missing"; fi
  chmod 700 "$RO_HOME"
else
  echo "  SKIP: wrapper unavailable"
fi

echo "== positive cross-worktree build overlap =="
if command -v cargo >/dev/null 2>&1 && [[ -x "$WRAPPER" ]]; then
  REPO="$TMP/repo"
  git init -q -b main "$REPO"
  git -C "$REPO" config user.email test@example.com
  git -C "$REPO" config user.name Test
  mkdir -p "$REPO/.cargo" "$REPO/src"
  printf '[package]\nname = "overlap_fixture"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n' > "$REPO/Cargo.toml"
  printf 'fn main() {}\n' > "$REPO/src/lib.rs"
  cat > "$REPO/build.rs" <<'EOF'
use std::{env, fs::OpenOptions, io::Write, thread, time::{Duration, SystemTime, UNIX_EPOCH}};
fn now_ms() -> u128 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() }
fn main() {
    println!("cargo:rerun-if-env-changed=BUILD_MARKER");
    let marker = env::var("BUILD_MARKER").unwrap();
    let mut file = OpenOptions::new().create(true).append(true).open(marker).unwrap();
    writeln!(file, "start={}", now_ms()).unwrap();
    thread::sleep(Duration::from_secs(2));
    writeln!(file, "end={}", now_ms()).unwrap();
}
EOF
  printf '[build]\nrustc-wrapper = "%s"\n' "$WRAPPER" > "$REPO/.cargo/config.toml"
  git -C "$REPO" add Cargo.toml build.rs src/lib.rs .cargo/config.toml
  git -C "$REPO" commit -qm init
  git -C "$REPO" worktree add -q "$TMP/wt-a" -b build-a main
  git -C "$REPO" worktree add -q "$TMP/wt-b" -b build-b main
  gate=$(( $(date +%s) + 2 ))
  (
    while [[ $(date +%s) -lt $gate ]]; do sleep 0.05; done
    unset CARGO_TARGET_DIR
    BUILD_MARKER="$TMP/a.marker" PATH="/usr/bin:/bin:$PATH" cargo build --manifest-path "$TMP/wt-a/Cargo.toml" >"$TMP/a.log" 2>&1
  ) & A=$!
  (
    while [[ $(date +%s) -lt $gate ]]; do sleep 0.05; done
    unset CARGO_TARGET_DIR
    BUILD_MARKER="$TMP/b.marker" PATH="/usr/bin:/bin:$PATH" cargo build --manifest-path "$TMP/wt-b/Cargo.toml" >"$TMP/b.log" 2>&1
  ) & B=$!
  wait "$A"; A_RC=$?
  wait "$B"; B_RC=$?
  if [[ $A_RC -eq 0 && $B_RC -eq 0 ]]; then pass "both isolated Cargo builds completed"; else fail "Cargo builds" "rc_a=$A_RC rc_b=$B_RC"; fi
  A_START=$(sed -n 's/^start=//p' "$TMP/a.marker" | head -1)
  A_END=$(sed -n 's/^end=//p' "$TMP/a.marker" | head -1)
  B_START=$(sed -n 's/^start=//p' "$TMP/b.marker" | head -1)
  B_END=$(sed -n 's/^end=//p' "$TMP/b.marker" | head -1)
  SKEW=$(( A_START > B_START ? A_START - B_START : B_START - A_START ))
  if [[ $A_START -lt $B_END && $B_START -lt $A_END ]]; then pass "build-script intervals overlap positively"; else fail "build overlap" "a=$A_START..$A_END b=$B_START..$B_END"; fi
  if [[ $SKEW -le 1000 ]]; then pass "cross-worktree build start wait is at most 1 second"; else fail "build start bound" "skew_ms=$SKEW"; fi
  if [[ -d "$TMP/wt-a/target" && -d "$TMP/wt-b/target" ]]; then pass "Cargo produced distinct worktree-local targets"; else fail "target isolation" "one or both target dirs missing"; fi
else
  echo "  SKIP: cargo or wrapper unavailable"
fi

echo ""
echo "cargo build isolation: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
