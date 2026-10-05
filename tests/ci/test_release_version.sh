#!/usr/bin/env bash
# tests/ci/test_release_version.sh
#
# Table test for the release version math: sync-version.sh's pre-release
# shapes and release-version.sh's channel/tag rules (nightly dev stamp,
# daily-cadence rcN bumps, stable promoting the newest candidate).
# Both run against a throwaway skeleton under mktemp -d; the real checkout
# is only read. Needs git on PATH.
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

root="$(pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fails=0
check() {
  if [ "$2" = "0" ]; then
    echo "  ok: $1"
  else
    echo "  FAIL: $1"
    fails=$((fails + 1))
  fi
}
check_rc() { # check_rc <desc> <actual> <expected>
  if [ "$2" -eq "$3" ]; then
    echo "  ok: $1"
  else
    echo "  FAIL: $1 (got exit $2, want $3)"
    fails=$((fails + 1))
  fi
}

# Run a script, capture stdout/stderr and the real exit code without set -e.
# The function IS the last command, so its status propagates without an
# explicit `return $?` (which loses the code on some bash 3.2 installs).
run() { # run <outfile> <errfile> <cmd...>
  local out="$1" err="$2"
  shift 2
  "$@" >"$out" 2>"$err"
}

# ---------------------------------------------------------------- sync-version
sync="$work/sync"
mkdir -p "$sync/scripts/release" "$sync/cli/src/fno" "$sync/crates/fno/src" "$sync/crates/fno-agents/src"
cp scripts/release/sync-version.sh "$sync/scripts/release/"
printf '# test skeleton\n__version__ = "0.3.2"\n' > "$sync/cli/src/fno/__init__.py"
printf '[package]\nname = "fno"\nversion = "0.3.2"\nedition = "2021"\n' > "$sync/crates/fno/Cargo.toml"
printf '[package]\nname = "fno-agents"\nversion = "0.3.2"\nedition = "2021"\n' > "$sync/crates/fno-agents/Cargo.toml"
# cargo metadata refuses a manifest with no targets; give each crate one so
# the metadata parse below tests the VERSION, not the skeleton's shape.
printf 'fn main() {}\n' > "$sync/crates/fno/src/main.rs"
printf 'fn main() {}\n' > "$sync/crates/fno-agents/src/main.rs"
json_manifests=".claude-plugin/plugin.json .claude-plugin/marketplace.json gemini-extension.json .codex-plugin/plugin.json .opencode/package.json plugins/openclaw/promise-tag-reader/package.json plugins/buddy/.claude-plugin/plugin.json"
for j in $json_manifests; do
  mkdir -p "$sync/$(dirname "$j")"
  printf '{\n  "name": "t",\n  "version": "0.3.2"\n}\n' > "$sync/$j"
done
sv() { bash "$sync/scripts/release/sync-version.sh" "$@"; }

# AC4: a semver spelling or a partial version exits 2 and writes nothing.
run "$work/o" "$work/e" sv 0.4.0-rc.1
rc=$?
check_rc "sync-version 0.4.0-rc.1 exits 2" "$rc" "2"
run "$work/o" "$work/e" sv 0.4
rc=$?
check_rc "sync-version 0.4 exits 2" "$rc" "2"
grep -q '__version__ = "0.3.2"' "$sync/cli/src/fno/__init__.py"
check "refused shapes leave __init__.py untouched" $?

# AC3: an rc shape lands as PEP 440 in Python/JSON and semver in Cargo.
run "$work/o" "$work/e" sv 0.4.0rc1
rc=$?
check_rc "sync-version 0.4.0rc1 exits 0" "$rc" "0"
grep -q '__version__ = "0.4.0rc1"' "$sync/cli/src/fno/__init__.py"
check "__init__.py reads 0.4.0rc1" $?
grep -q '^version = "0.4.0-rc.1"' "$sync/crates/fno/Cargo.toml"
check "crates/fno/Cargo.toml reads 0.4.0-rc.1" $?
grep -q '^version = "0.4.0-rc.1"' "$sync/crates/fno-agents/Cargo.toml"
check "crates/fno-agents/Cargo.toml reads 0.4.0-rc.1" $?
grep -q '"version": "0.4.0rc1"' "$sync/.claude-plugin/plugin.json"
check "plugin.json reads 0.4.0rc1" $?
run "$work/o" "$work/e" sv --check
rc=$?
check_rc "--check agrees after an rc bump (exit 0)" "$rc" "0"

# Dev shape converts the same way.
run "$work/o" "$work/e" sv 0.4.0.dev20260925
rc=$?
check_rc "sync-version 0.4.0.dev20260925 exits 0" "$rc" "0"
grep -q '^version = "0.4.0-dev.20260925"' "$sync/crates/fno/Cargo.toml"
check "Cargo.toml reads 0.4.0-dev.20260925" $?
run "$work/o" "$work/e" sv --check
rc=$?
check_rc "--check agrees after a dev bump (exit 0)" "$rc" "0"

# The stamp must leave a version cargo can actually parse: a PEP 440 spelling
# reaching a manifest fails every cargo build (the nightly shipped exactly
# that once). Parse both stamped manifests, not just the text.
for cf in "$sync/crates/fno/Cargo.toml" "$sync/crates/fno-agents/Cargo.toml"; do
  if cargo metadata --offline --no-deps --format-version 1 --manifest-path "$cf" > "$work/meta.json" 2>>"$work/e"; then
    echo "  ok: cargo metadata parses $(basename "$(dirname "$cf")") at the dev version"
  else
    echo "  FAIL: cargo metadata rejects $(basename "$(dirname "$cf")") at the dev version"
    fails=$((fails + 1))
  fi
  grep -q '"version":"0.4.0-dev.20260925"' "$work/meta.json"
  check "$(basename "$(dirname "$cf")") metadata reports 0.4.0-dev.20260925" $?
done

# Plain shape still round-trips, and a hand-patched crate fails --check.
run "$work/o" "$work/e" sv 0.5.0
rc=$?
check_rc "sync-version 0.5.0 exits 0" "$rc" "0"
sed -i.bak 's/^version = ".*"/version = "0.9.9"/' "$sync/crates/fno/Cargo.toml" && rm -f "$sync/crates/fno/Cargo.toml.bak"
run "$work/o" "$work/e" sv --check
rc=$?
check_rc "--check catches a drifted crate (exit 1)" "$rc" "1"

# ------------------------------------------------------------- release-version
rv="$work/gitrepo"
git init -q -b main "$rv"
git -C "$rv" config user.email t@example.com
git -C "$rv" config user.name t
: > "$rv/placeholder" && git -C "$rv" add placeholder && git -C "$rv" commit -qm init
git -C "$rv" tag v0.3.1
# release-version.sh reads tags in its CWD, so every invocation runs inside
# the throwaway repo (a subshell cd leaves the outer cwd alone).
rver() { (cd "$rv" && bash "$root/scripts/release/release-version.sh" "$@"); }

# Nightly math: dev-stamped version, rolling non-v tag.
run "$work/o" "$work/e" rver nightly 0.4.0 20260925
rc=$?
check_rc "nightly exits 0" "$rc" "0"
grep -q '^version=0.4.0.dev20260925$' "$work/o" && grep -q '^tag=nightly$' "$work/o"
check "nightly prints version=0.4.0.dev20260925 and tag=nightly" $?

# Stable without any candidate refuses; no rc tag exists yet, so the refusal
# is global, not version-scoped.
run "$work/o" "$work/e" rver stable 0.6.0 20260925
rc=$?
check_rc "stable with no v*rc* tag exits 1" "$rc" "1"
grep -q "cut an rc first" "$work/e"
check "stable refusal names the missing candidate" $?

# Daily-cadence rc math: candidates bump rcN on one base; the base only
# moves when main's src passes it.
git -C "$rv" tag v0.4.0rc1
run "$work/o" "$work/e" rver rc 0.4.0 20260925
rc=$?
check_rc "rc exits 0" "$rc" "0"
grep -q '^version=0.4.0rc2$' "$work/o" && grep -q '^tag=v0.4.0rc2$' "$work/o"
check "rc after v0.4.0rc1 prints version=0.4.0rc2 and tag=v0.4.0rc2" $?

# rcN sorts numerically: rc10 ranks past rc9, so the next bump is rc11.
git -C "$rv" tag v0.4.0rc9
git -C "$rv" tag v0.4.0rc10
run "$work/o" "$work/e" rver rc 0.4.0 20260925
grep -q '^tag=v0.4.0rc11$' "$work/o"
check "rc after v0.4.0rc10 prints v0.4.0rc11" $?

# A released v<src> refuses nightly only; rcN keeps cutting past it.
git -C "$rv" tag v0.4.0
run "$work/o" "$work/e" rver nightly 0.4.0 20260925
rc=$?
check_rc "nightly at a released v0.4.0 exits 1" "$rc" "1"
grep -q "sync-version" "$work/e"
check "refusal names the sync-version bump" $?
run "$work/o" "$work/e" rver rc 0.4.0 20260925
rc=$?
check_rc "rc at a released v0.4.0 exits 0" "$rc" "0"
grep -q '^tag=v0.4.0rc11$' "$work/o"
check "rc still prints v0.4.0rc11 past the released base" $?

# Stable promotes the newest candidate; its base is released -> idempotent 3.
run "$work/o" "$work/e" rver stable 0.4.0 20260925
rc=$?
check_rc "stable with a released base exits 3 (idempotent no-op)" "$rc" "3"

# Daily monotonic: the newest candidate (0.4.1rc1) drives the next rcN.
git -C "$rv" tag v0.4.1rc1
run "$work/o" "$work/e" rver rc 0.4.0 20260925
rc=$?
check_rc "daily rc exits 0" "$rc" "0"
grep -q '^version=0.4.1rc2$' "$work/o" && grep -q '^tag=v0.4.1rc2$' "$work/o"
check "daily rc after v0.4.1rc1 prints v0.4.1rc2" $?

# Main synced past the candidates: src wins again.
run "$work/o" "$work/e" rver rc 0.4.5 20260925
rc=$?
check_rc "rc with src past the newest candidate exits 0" "$rc" "0"
grep -q '^version=0.4.5rc1$' "$work/o"
check "rc with src 0.4.5 prints v0.4.5rc1" $?

# Stable derives from the newest candidate, not src, and rcN tags promote
# by base: v0.6.0rc1 wins over every 0.4.x candidate even though src says
# 0.4.0 (the old math refused here forever).
git -C "$rv" tag v0.6.0rc1
run "$work/o" "$work/e" rver stable 0.4.0 20260925
rc=$?
check_rc "stable with a newer candidate exits 0" "$rc" "0"
grep -q '^version=0.6.0$' "$work/o" && grep -q '^tag=v0.6.0$' "$work/o"
check "stable promotes the newest candidate v0.6.0" $?

# Misuse: a partial source version or a bad channel exits 2.
run "$work/o" "$work/e" rver nightly 0.4 20260925
rc=$?
check_rc "partial src 0.4 exits 2" "$rc" "2"
run "$work/o" "$work/e" rver beta 0.4.0 20260925
rc=$?
check_rc "unknown channel exits 2" "$rc" "2"

if [ "$fails" -eq 0 ]; then
  echo "test_release_version: ALL PASS"
fi
exit "$fails"
