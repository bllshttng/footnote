#!/usr/bin/env bash
# Propagate ONE release version to every version-bearing manifest in the repo.
#
# The plugin/extension manifests drifted (claude/marketplace/gemini/codex all
# lagged the CLI) precisely because nothing propagated the version to them. This
# is that single point: the release workflow (release.yml) and any manual bump
# both call it, so all surfaces stay in lockstep by construction.
#
# Accepts one PEP 440 version; cargo refuses PEP 440 spellings, so the two
# Cargo.toml files get the semver form while __init__.py and the JSON manifests
# keep the PEP 440 string (the codex plugin cache dir keys off __version__):
#   X.Y.Z        -> __init__.py / JSON: X.Y.Z             Cargo.toml: X.Y.Z
#   X.Y.ZrcN     -> __init__.py / JSON: X.Y.ZrcN          Cargo.toml: X.Y.Z-rc.N
#   X.Y.Z.devN   -> __init__.py / JSON: X.Y.Z.devN        Cargo.toml: X.Y.Z-dev.N
# Any other shape exits 2. --check compares each Cargo.toml against the semver
# form of __version__, so a hand-patched crate version fails the drift guard.
#
# Version-agnostic: it overwrites whatever is there with $1, so it works from any
# starting version and imposes no zero-padding (0.3.9 -> 0.3.10 -> 0.3.100 is
# fine; we expect to ride 0.3.x deep into 3-digit patches).
#
# perl -i (not `sed -i`) so it is identical on the ubuntu runner and a
# maintainer's macOS: BSD and GNU `sed -i` take their backup-suffix argument
# differently, which silently corrupts one or the other.
#
# Usage:
#   scripts/release/sync-version.sh 0.4.0      # set every surface to 0.4.0
#   scripts/release/sync-version.sh 0.4.0rc1   # pre-release; cargo gets 0.4.0-rc.1
#   scripts/release/sync-version.sh --check    # assert every surface already agrees
set -euo pipefail

arg="${1:?usage: sync-version.sh <X.Y.Z | X.Y.ZrcN | X.Y.Z.devN> | --check}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"

# PEP 440 -> semver: 0.4.0rc1 -> 0.4.0-rc.1, 0.4.0.dev20260925 -> 0.4.0-dev.20260925.
pep_to_semver() {
  local ver="$1" base tail
  case "$ver" in
    *rc[0-9]*) tail="${ver##*rc}"; base="${ver%rc*}"; echo "${base}-rc.${tail}" ;;
    *.dev[0-9]*) tail="${ver##*.dev}"; base="${ver%.dev*}"; echo "${base}-dev.${tail}" ;;
    *) echo "$ver" ;;
  esac
}

# Strict shape: plain X.Y.Z, or X.Y.Z followed by rc<digits> or .dev<digits>.
is_release_version() {
  printf '%s' "$1" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+(rc[0-9]+|\.dev[0-9]+)?$'
}

# The manifests whose top-level `"version"` must track the release. __init__.py
# is the source of truth, so --check compares everything else against it.
JSON_MANIFESTS=(
  .claude-plugin/plugin.json
  .claude-plugin/marketplace.json
  gemini-extension.json
  .codex-plugin/plugin.json
  .opencode/package.json
  plugins/openclaw/promise-tag-reader/package.json
)

# --check: fail (exit 1) if any surface disagrees with the wheel version. The
# drift guard that would have caught the plugins stranded at 0.2.x. Read-only.
if [[ "$arg" == "--check" ]]; then
  want_pep="$(perl -ne 'print $1 if /^__version__ = "(.*)"$/' cli/src/fno/__init__.py)"
  want_semver="$(pep_to_semver "$want_pep")"
  bad=0
  # Cargo carries the semver spelling; everything else carries the PEP 440 one.
  for cf in crates/fno/Cargo.toml crates/fno-agents/Cargo.toml; do
    got="$(perl -ne 'if(/^version = "(.*)"/){print $1; exit}' "$cf")"
    [[ "$got" == "$want_semver" ]] || { echo "drift: $cf = $got (want $want_semver)"; bad=1; }
  done
  for j in "${JSON_MANIFESTS[@]}"; do
    matches="$(perl -ne 'print "$1\n" while /"version"\s*:\s*"([^"]*)"/g' "$j")"
    # Zero version fields is itself drift: a dropped/renamed key would otherwise
    # pass silently (the while loop just never runs).
    if [[ -z "$matches" ]]; then
      echo "drift: $j has no \"version\" field (want $want_pep)"; bad=1; continue
    fi
    while IFS= read -r got; do
      [[ "$got" == "$want_pep" ]] || { echo "drift: $j = $got (want $want_pep)"; bad=1; }
    done <<< "$matches"
  done
  [[ "$bad" == 0 ]] && echo "sync-version: all surfaces agree at ${want_pep}"
  exit "$bad"
fi

ver="$arg"
is_release_version "$ver" || {
  echo "sync-version: '$ver' is not an X.Y.Z, X.Y.ZrcN or X.Y.Z.devN version" >&2
  exit 2
}
semver="$(pep_to_semver "$ver")"

# 1. Python wheel (the source of truth; cli/pyproject.toml reads it dynamically).
perl -i -pe "s/^__version__ = \".*\"/__version__ = \"${ver}\"/" cli/src/fno/__init__.py

# 2. Rust crates + their lockfiles, in the semver form. `^version = ` matches
#    only the [package] version (rust-version and dependency lines never start
#    with `version `).
for cf in crates/fno/Cargo.toml crates/fno-agents/Cargo.toml; do
  perl -i -pe "s/^version = \".*\"/version = \"${semver}\"/" "$cf"
  cargo update --manifest-path "$cf" -p "$(basename "$(dirname "$cf")")" \
    --precise "${semver}" >/dev/null 2>&1 || true
done

# 3. JSON plugin / extension manifests. Every `"version"` key in each of these is
#    the package version (verified: no nested/dependency `"version"` keys), so a
#    global key-anchored replace is safe and touches only the version line(s) -
#    marketplace.json legitimately carries two (listing metadata + the plugin row).
for j in "${JSON_MANIFESTS[@]}"; do
  [[ -f "$j" ]] || { echo "sync-version: manifest missing: $j" >&2; exit 1; }
  # Whitespace-tolerant match (handles a compact `"version":"x"` reformat too),
  # normalized to the canonical spaced form.
  perl -i -pe "s/\"version\"\s*:\s*\"[^\"]*\"/\"version\": \"${ver}\"/g" "$j"
  # Verify the bump actually landed. A missing/renamed version key - or a format
  # this regex somehow still missed - would otherwise no-op SILENTLY and ship a
  # stale plugin version behind a bumped CLI (the exact drift this guards, and
  # the nightly only re-verifies __version__). Fail loud instead.
  grep -q "\"version\": \"${ver}\"" "$j" \
    || { echo "sync-version: no \"version\" field set to ${ver} in $j (missing or unrecognized key?)" >&2; exit 1; }
done

echo "sync-version: all surfaces set to ${ver}"
