#!/usr/bin/env bash
# Plugin-channel version math, sourced by .claude-plugin/postinstall.sh and
# tests/ci/test_plugin_channel_version.sh. NOT meant to be executed directly.
#
# The two plugin channels resolve to distinct version strings: the `stable`
# ref carries the plain release version, the rolling `nightly` tag carries a
# semver-spelled dev stamp (0.4.0-dev.20260925). Installers report PEP 440
# spellings (0.4.0.dev20260925), so matching normalizes both spellings before
# comparing - an install counts as ours only when the versions are the same
# number, not the same string prefix.

# plugin_channel <declared-version> -> prints stable | rc | nightly
plugin_channel() {
  case "$1" in
    *-dev.*|*.dev*) echo "nightly" ;;
    *-rc.*|*rc[0-9]*) echo "rc" ;;
    *) echo "stable" ;;
  esac
}

# plugin_pep_normalize <version> -> PEP 440 spelling of a semver pre-release
plugin_pep_normalize() {
  printf '%s' "$1" | sed -E 's/-dev\.([0-9]+)$/.dev\1/; s/-rc\.([0-9]+)$/rc\1/'
}

# plugin_version_matches <installed> <declared> -> exit 0 when both spell the
# same version. A mismatch (the reserved 0.0.0 placeholder, a squatted name,
# or simply the wrong release) fails closed.
plugin_version_matches() {
  [ "$(plugin_pep_normalize "$1")" = "$(plugin_pep_normalize "$2")" ]
}

# plugin_wheel_platform <uname-s> <uname-m> -> an extended glob matching the
# release wheel's platform tag for this machine, or "" when unsupported.
plugin_wheel_platform() {
  case "$1/$2" in
    Darwin/arm64) echo "macosx*arm64" ;;
    Darwin/x86_64) echo "macosx*x86_64" ;;
    Linux/x86_64) echo "manylinux*x86_64" ;;
    Linux/aarch64) echo "manylinux*aarch64" ;;
    *) echo "" ;;
  esac
}
