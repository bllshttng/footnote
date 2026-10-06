#!/usr/bin/env bash
# Channel -> version + tag math for release.yml's resolve job. Prints
# version=<pep440> and tag=<tag> lines suitable for $GITHUB_OUTPUT.
#
# Rules (reads existing tags with `git tag -l`; run inside a full checkout):
# - nightly: version=<src>.dev<yyyymmdd>, tag=nightly (rolling, no v* tag).
# - rc:      daily-cadence math; candidates bump rcN within a base
#            (0.4.1rc1, 0.4.1rc2, ...), and the base moves only when main's
#            __version__ passes it (the post-promotion sync):
#              no v*rc* tag yet       -> <src>rc1
#              newest rc base < src   -> <src>rc1     (main synced past it)
#              newest rc base >= src  -> <base>rc<N+1> (highest N at base)
# - stable:  promotes the NEWEST v*rc* tag: version = its base, tag=v<base>.
#            Going by the candidate (not <src>) is what lets a candidate cut
#            on a later patch than main's __version__ still promote.
#            Refuses (exit 1) while no v*rc* tag exists: stable promotes a
#            candidate.
# - Nightly refuses (exit 1) when v<src> already exists, naming the
#   sync-version bump. Stable exits 3 on an already-promoted candidate: the
#   release already shipped, which the caller reads as an idempotent no-op.
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

# Nightly is the only channel pinned to src, so it is the only one a released
# v<src> stops. Stable exits 3 on an already-promoted candidate, and rc's
# daily-cadence math below always lands past both.
if [ "$channel" = "nightly" ] && git rev-parse -q --verify "refs/tags/v${src}" >/dev/null; then
  echo "release-version: v${src} is released; bump main with scripts/release/sync-version.sh <next>" >&2
  exit 1
fi

case "$channel" in
  nightly)
    echo "version=${src}.dev${day}"
    echo "tag=nightly"
    ;;
  rc)
    newest_rc="$(git tag -l 'v*rc*' --sort=-v:refname | head -1 || true)"
    base="$src"
    n=0
    if [ -n "$newest_rc" ]; then
      cand="${newest_rc%rc*}"
      cand="${cand#v}"
      newer="$(printf '%s\n%s\n' "$cand" "$src" | sort -V | tail -1)"
      if [ "$newer" = "$cand" ]; then
        base="$cand"
        n="$(printf '%s' "$newest_rc" | sed -E 's/.*rc([0-9]+)$/\1/')"
      fi
    fi
    echo "version=${base}rc$((n + 1))"
    echo "tag=v${base}rc$((n + 1))"
    ;;
  stable)
    newest_rc="$(git tag -l 'v*rc*' --sort=-v:refname | head -1 || true)"
    if [ -z "$newest_rc" ]; then
      echo "release-version: no v*rc* tag exists - stable promotes a candidate; cut an rc first" >&2
      exit 1
    fi
    version="${newest_rc%rc*}"
    version="${version#v}"
    if git rev-parse -q --verify "refs/tags/v${version}" >/dev/null; then
      echo "release-version: v${version} is already released - nothing to promote" >&2
      exit 3
    fi
    echo "version=${version}"
    echo "tag=v${version}"
    ;;
esac
