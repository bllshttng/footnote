#!/usr/bin/env bash
# Channel -> version + tag math for release.yml's resolve job. Prints
# version=<pep440> and tag=<tag> lines suitable for $GITHUB_OUTPUT.
#
# Rules (reads existing tags with `git tag -l`; run inside a full checkout):
# - nightly: version=<src>.dev<yyyymmdd>, tag=nightly (rolling, no v* tag).
# - rc:      N is one more than the count of v<src>rc* tags; version=<src>rcN.
# - stable:  version=<src>, tag=v<src>. Refuses (exit 1) while no v<src>rc*
#            tag exists: stable promotes a candidate.
# - Any channel refuses (exit 1) when v<src> already exists, naming the
#   sync-version bump. For stable that same case exits 3: the release already
#   shipped, which the caller reads as an idempotent no-op, not a failure.
set -euo pipefail

usage="usage: release-version.sh <nightly|rc|stable> <X.Y.Z> <yyyymmdd>"

channel="${1:?${usage}}"
src="${2:?${usage}}"
day="${3:?${usage}}"

if ! printf '%s' "$src" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "release-version: source version '${src}' must be plain X.Y.Z" >&2
  exit 2
fi
if ! printf '%s' "$day" | grep -qE '^[0-9]{8}$'; then
  echo "release-version: date '${day}' must be yyyymmdd" >&2
  exit 2
fi
case "$channel" in
  nightly|rc|stable) : ;;
  *) echo "release-version: channel '${channel}' must be nightly, rc or stable" >&2; exit 2 ;;
esac

# v<src> already released: stable is an idempotent no-op (exit 3); every other
# channel is a real refusal naming the version bump.
if git rev-parse -q --verify "refs/tags/v${src}" >/dev/null; then
  if [ "$channel" = "stable" ]; then
    echo "release-version: v${src} is already released - nothing to promote" >&2
    exit 3
  fi
  echo "release-version: v${src} is released; bump main with scripts/release/sync-version.sh <next>" >&2
  exit 1
fi

case "$channel" in
  nightly)
    echo "version=${src}.dev${day}"
    echo "tag=nightly"
    ;;
  rc)
    count="$(git tag -l "v${src}rc*" | wc -l | tr -d ' ')"
    n=$((count + 1))
    echo "version=${src}rc${n}"
    echo "tag=v${src}rc${n}"
    ;;
  stable)
    if [ -z "$(git tag -l "v${src}rc*")" ]; then
      echo "release-version: no v${src}rc* tag exists - stable promotes a candidate; cut an rc first" >&2
      exit 1
    fi
    echo "version=${src}"
    echo "tag=v${src}"
    ;;
esac
