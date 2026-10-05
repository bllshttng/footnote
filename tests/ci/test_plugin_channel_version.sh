#!/usr/bin/env bash
# tests/ci/test_plugin_channel_version.sh
#
# Table test for the plugin channels: the marketplace entries pin their refs
# and carry no version field (tasks 3.1/3.2), and plugin-version.sh's channel
# detection plus spelling-tolerant version match behave for every channel
# form postinstall.sh can meet (task 3.3). The real checkout is only read.
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

root="$(pwd)"
fails=0
ok() { echo "  ok: $1"; }
bad() { echo "  FAIL: $1"; fails=$((fails + 1)); }
check() { if [ "$2" = "0" ]; then ok "$1"; else bad "$1"; fi; }

# shellcheck disable=SC1091
source scripts/release/plugin-version.sh

# ---------------------------------------------------------- channel detection
expect_channel() { # expect_channel <version> <want>
  got="$(plugin_channel "$1")"
  [ "$got" = "$2" ]
}
expect_channel 0.4.0 stable; check "0.4.0 detects stable" $?
expect_channel 0.4.0rc1 rc; check "0.4.0rc1 detects rc" $?
expect_channel 0.4.0rc2 rc; check "0.4.0rc2 detects rc" $?
expect_channel 0.4.0-rc.1 rc; check "semver 0.4.0-rc.1 detects rc" $?
expect_channel 0.4.0.dev20260925 nightly; check "PEP 440 dev detects nightly" $?
expect_channel 0.4.0-dev.20260925 nightly; check "semver dev detects nightly" $?

# ------------------------------------------------------- spelling-tolerant match
expect_match() { # expect_match <installed> <declared> <want: 0 match | 1 no>
  plugin_version_matches "$1" "$2"
  [ "$?" = "$3" ]
}
expect_match 0.4.0 0.4.0 0; check "stable matches itself" $?
expect_match 0.4.0rc1 0.4.0rc1 0; check "rc matches itself" $?
expect_match 0.4.0rc1 0.4.0-rc.1 0; check "PEP 440 rc matches semver rc" $?
expect_match 0.4.0.dev20260925 0.4.0-dev.20260925 0; check "PEP 440 dev matches semver dev" $?
expect_match 0.3.1 0.4.0 1; check "an older release never matches" $?
expect_match 0.0.0 0.4.0 1; check "the 0.0.0 placeholder never matches" $?
expect_match "" 0.4.0 1; check "an unresolved install never matches" $?
expect_match 0.4.0rc1 0.4.0 1; check "an rc never satisfies a stable plugin" $?
expect_match 0.4.0.dev20260924 0.4.0-dev.20260925 1; check "yesterday's nightly never matches today's" $?

# ------------------------------------------------------------ platform regex
[ "$(plugin_wheel_platform Darwin arm64)" = "macosx.*arm64" ]; check "darwin arm64 wheel regex" $?
[ "$(plugin_wheel_platform Darwin x86_64)" = "macosx.*x86_64" ]; check "darwin x86_64 wheel regex" $?
[ "$(plugin_wheel_platform Linux x86_64)" = "manylinux.*x86_64" ]; check "linux x86_64 wheel regex" $?
[ "$(plugin_wheel_platform Linux aarch64)" = "manylinux.*aarch64" ]; check "linux aarch64 wheel regex" $?
[ -z "$(plugin_wheel_platform MINGW_NT x86_64)" ]; check "an unsupported platform has no pattern" $?
printf 'fno-0.4.0.dev20260925-py3-none-macosx_11_0_arm64.whl' | python3 -c 'import re,sys; sys.exit(0 if re.search("macosx.*arm64", sys.stdin.read()) else 1)'
check "the darwin arm64 regex matches a real wheel filename" $?

# ------------------------------------------------- marketplace + plugin shape
python3 - <<'PY' || fails=$((fails + 1))
import json, sys

with open(".claude-plugin/marketplace.json") as fh:
    marketplace = json.load(fh)
entries = {p["name"]: p for p in marketplace["plugins"]}

fail = []
for channel in ("fno", "fno-nightly"):
    entry = entries.get(channel)
    if entry is None:
        fail.append(f"{channel} entry missing")
        continue
    src = entry.get("source") or {}
    if src.get("source") != "github" or src.get("repo") != "bllshttng/footnote":
        fail.append(f"{channel} source is not the github repo")
    if "version" in entry:
        fail.append(f"{channel} entry carries a version field; plugin.json is the one version")
for name, ref in (("fno", "stable"), ("fno-nightly", "nightly")):
    if name in entries and (entries[name].get("source") or {}).get("ref") != ref:
        fail.append(f"{name} does not pin ref {ref}")

for manifest in (".claude-plugin/plugin.json", ".codex-plugin/plugin.json"):
    with open(manifest) as fh:
        plugin = json.load(fh)
    if not plugin.get("version"):
        fail.append(f"{manifest} carries no version")

if fail:
    for line in fail:
        print(f"  FAIL: {line}", file=sys.stderr)
    sys.exit(1)
PY
[ "$?" = "0" ]; check "marketplace entries pin stable/nightly refs; plugin.json holds the versions" $?

# ------------------------------------- fno.sh resolves the tree's channel (x-6f1d)
# scripts/install/fno.sh run INSIDE a tree must not pair the tree's nightly
# skills and hooks with the stable PyPI CLI. Drive the installer's own
# detect_channel + resolve_source in a sandbox tree; nothing here installs.
# fn_from_fno_sh prints one function's text from the real installer, so a
# rename there fails loud here instead of testing a stale copy.
fn_from_fno_sh() {
  sed -n "/^$1() {/,/^}/p" "$root/scripts/install/fno.sh"
}

# A nightly release body with one asset per supported platform family; the
# sandbox fetch `cat`s it where the real run reads the GitHub API.
NIGHTLY_RELEASE_JSON='{
  "assets": [
    {"name": "fno-0.4.1.dev20261003-py3-none-macosx_11_0_arm64.whl",
     "browser_download_url": "https://github.com/bllshttng/footnote/releases/download/nightly/fno-0.4.1.dev20261003-py3-none-macosx_11_0_arm64.whl"},
    {"name": "fno-0.4.1.dev20261003-py3-none-manylinux2014_x86_64.whl",
     "browser_download_url": "https://github.com/bllshttng/footnote/releases/download/nightly/fno-0.4.1.dev20261003-py3-none-manylinux2014_x86_64.whl"}
  ]
}'

FNO_SH_SANDBOX="$(mktemp -d)"
mkdir -p "$FNO_SH_SANDBOX/scripts/install" "$FNO_SH_SANDBOX/scripts/release" \
  "$FNO_SH_SANDBOX/.claude-plugin" "$FNO_SH_SANDBOX/bin"
cp "$root/scripts/install/fno.sh" "$FNO_SH_SANDBOX/scripts/install/fno.sh"
cp "$root/scripts/release/plugin-version.sh" "$FNO_SH_SANDBOX/scripts/release/plugin-version.sh"
# The stub fetch ignores the URL argument the real downloader is given and
# cats the body file: empty or unreadable body = an API read that failed.
printf '#!/bin/sh\ncat "$FAKE_FETCH_BODY" 2>/dev/null\n' > "$FNO_SH_SANDBOX/bin/fake-fetch"
chmod +x "$FNO_SH_SANDBOX/bin/fake-fetch"
printf '%s' "$NIGHTLY_RELEASE_JSON" > "$FNO_SH_SANDBOX/release.json"

# fno_sh_source_in_tree <declared-version> <uname-s> <uname-m> <fetch-body>
#   Runs detect_channel + resolve_source from the sandbox tree with $0 inside
#   it, a stubbed uname and a stubbed fetch; prints FNO_SOURCE. A resolve that
#   dies exits 9 with the reason on stderr.
fno_sh_source_in_tree() {
  _v="$1"; _us="$2"; _um="$3"; _fetch="$4"
  printf '{"name": "fno", "version": "%s"}\n' "$_v" \
    > "$FNO_SH_SANDBOX/.claude-plugin/plugin.json"
  printf '#!/bin/sh\ncase "$1" in -s) echo "%s";; -m) echo "%s";; *) : ;; esac\n' \
    "$_us" "$_um" > "$FNO_SH_SANDBOX/bin/uname"
  chmod +x "$FNO_SH_SANDBOX/bin/uname"
  PATH="$FNO_SH_SANDBOX/bin:$PATH" FAKE_FETCH_BODY="$_fetch" FNO_SH_SANDBOX="$FNO_SH_SANDBOX" sh -c '
    FNO_RELEASE_REPO=bllshttng/footnote
    FNO_DECLARED_VERSION=
    FNO_CHANNEL=stable
    FNO_WHEEL_URL=
    '"$(fn_from_fno_sh detect_channel; fn_from_fno_sh nightly_wheel_url; fn_from_fno_sh resolve_source)"'
    say() { :; }
    die() { printf "die: %s\n" "$1" >&2; exit 9; }
    fetch_pipe_cmd() { FNO_FETCH_TO_STDOUT="$FNO_SH_SANDBOX/bin/fake-fetch"; }
    detect_channel
    resolve_source
    printf "%s\n" "$FNO_SOURCE"
  ' "$FNO_SH_SANDBOX/scripts/install/fno.sh"
}

got="$(fno_sh_source_in_tree 0.4.1 Darwin arm64 /nonexistent)"
[ "$got" = "fno" ]; check "a stable tree keeps the by-name PyPI package" $?

got="$(fno_sh_source_in_tree 0.4.1-dev.20261003 Darwin arm64 "$FNO_SH_SANDBOX/release.json")"
case "$got" in
  https://*/nightly/fno-0.4.1.dev20261003-py3-none-macosx*arm64.whl) check "a nightly tree installs this platform's nightly wheel" 0 ;;
  *) check "a nightly tree installs this platform's nightly wheel (got: $got)" 1 ;;
esac

got="$(fno_sh_source_in_tree 0.4.1-dev.20261003 Linux x86_64 "$FNO_SH_SANDBOX/release.json")"
case "$got" in
  https://*/nightly/fno-0.4.1.dev20261003-py3-none-manylinux*x86_64.whl) check "the nightly wheel matches the linux platform regex too" 0 ;;
  *) check "the nightly wheel matches the linux platform regex too (got: $got)" 1 ;;
esac

fno_sh_source_in_tree 0.4.1-dev.20261003 FreeBSD amd64 "$FNO_SH_SANDBOX/release.json" >/dev/null 2>"$FNO_SH_SANDBOX/err"
rc=$?
[ "$rc" = "9" ]; check "a nightly with no wheel for the platform dies, never downgrades" $?
grep -q "FNO_INSTALL_WHEEL" "$FNO_SH_SANDBOX/err"; check "the nightly refusal names the manual repair" $?

fno_sh_source_in_tree 0.4.1-dev.20261003 Darwin arm64 /nonexistent >/dev/null 2>&1
rc=$?
[ "$rc" = "9" ]; check "an unreadable nightly release dies on a nightly tree" $?

got="$(fno_sh_source_in_tree 0.4.0-rc.1 Darwin arm64 /nonexistent)"
[ "$got" = "fno==0.4.0rc1" ]; check "an rc tree pins the PEP 440 candidate" $?

got="$(FNO_VERSION=0.3.9 fno_sh_source_in_tree 0.4.1-dev.20261003 Darwin arm64 "$FNO_SH_SANDBOX/release.json")"
[ "$got" = "fno==0.3.9" ]; check "an explicit FNO_VERSION still wins over the tree channel" $?

rm -rf "$FNO_SH_SANDBOX"

if [ "$fails" -eq 0 ]; then
  echo "test_plugin_channel_version: ALL PASS"
fi
exit "$fails"
