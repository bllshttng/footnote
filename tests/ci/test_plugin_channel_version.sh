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

if [ "$fails" -eq 0 ]; then
  echo "test_plugin_channel_version: ALL PASS"
fi
exit "$fails"
