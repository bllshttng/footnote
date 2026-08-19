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
exec "$@"
EOF
chmod +x "$TMP/compiler" "$TMP/bin/sccache"

if [[ -x "$WRAPPER" ]]; then
  COMPILER_LOG="$TMP/logs/compiler-direct" PATH="/usr/bin:/bin" "$WRAPPER" "$TMP/compiler" --crate-name direct
  if grep -q '^unset|--crate-name direct$' "$TMP/logs/compiler-direct"; then pass "missing sccache falls through to real compiler"; else fail "direct fallback" "compiler receipt missing"; fi

  COMPILER_LOG="$TMP/logs/compiler-cache" SCCACHE_LOG="$TMP/logs/sccache-default" PATH="$TMP/bin:/usr/bin:/bin" "$WRAPPER" "$TMP/compiler" --crate-name cached
  if grep -q '^10G|' "$TMP/logs/sccache-default"; then pass "sccache defaults to bounded 10G cache"; else fail "sccache default cap" "10G receipt missing"; fi
  if grep -q -- '--crate-name cached' "$TMP/logs/compiler-cache"; then pass "sccache preserves the rustc invocation"; else fail "sccache argv" "compiler did not receive original argv"; fi

  COMPILER_LOG="$TMP/logs/compiler-override" SCCACHE_LOG="$TMP/logs/sccache-override" SCCACHE_CACHE_SIZE=3G PATH="$TMP/bin:/usr/bin:/bin" "$WRAPPER" "$TMP/compiler" --crate-name override
  if grep -q '^3G|' "$TMP/logs/sccache-override"; then pass "operator sccache cap is preserved"; else fail "sccache override" "3G receipt missing"; fi
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
