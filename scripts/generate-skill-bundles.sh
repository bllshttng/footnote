#!/usr/bin/env bash
# Generate skill bundles from the canonical mapping in skill-bundles.yaml.
# Idempotent: copies/transforms source -> dest with appropriate handling per
# bundle type.
#
# Three bundle types:
#   - file       cp -p source -> dest (preserves executable bits, mtime)
#   - reference  pipe source through bundle-frontmatter.py strip
#   - agent      pipe source through bundle-frontmatter.py rewrite --as subagent
#
# Run via pre-commit hook (or manually before pushing) so committed state
# always reflects the manifest. CI verifies via check-skill-bundles-fresh.sh.
#
# Pure shell + python3 (stdlib + PyYAML when references/agents are used).
# PyYAML comes from the host interpreter when it has it, else from `uv run
# --with pyyaml`; no host provisioning either way.
#
# Usage:
#   bash scripts/generate-skill-bundles.sh           # generate into repo root
#   REPO_ROOT=/tmp/xyz bash scripts/...              # override target root
set -euo pipefail

# Resolve repo root: prefer caller-supplied REPO_ROOT (used by the freshness
# check to redirect output into a temp dir), otherwise derive from git.
if [[ -n "${REPO_ROOT:-}" ]]; then
  TARGET_ROOT="$REPO_ROOT"
else
  # Defensive: explicit if-form so `set -e` + `inherit_errexit` (default on
  # newer CI bash) can't propagate git's rc=128 silently. See
  # scripts/lint/check-skill-bundles-fresh.sh for the same pattern.
  TARGET_ROOT=""
  if git_root=$(git rev-parse --show-toplevel 2>/dev/null); then
    TARGET_ROOT="$git_root"
  fi
  if [[ -z "$TARGET_ROOT" ]]; then
    echo "ERROR: not in a git repo and REPO_ROOT not set" >&2
    exit 1
  fi
fi

# The manifest + parser live alongside the canonical scripts. Resolve them
# from the script's own location so this script keeps working when the
# generator is invoked with REPO_ROOT pointing somewhere else (the temp
# dir used by check-skill-bundles-fresh.sh).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SOURCE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
MANIFEST="$SOURCE_ROOT/skill-bundles.yaml"
PARSER="$SOURCE_ROOT/scripts/lib/parse-bundle-manifest.py"
FRONTMATTER_HELPER="$SOURCE_ROOT/scripts/lib/bundle-frontmatter.py"

if [[ ! -f "$MANIFEST" ]]; then
  echo "ERROR: $MANIFEST not found" >&2
  exit 1
fi
if [[ ! -f "$PARSER" ]]; then
  echo "ERROR: $PARSER not found" >&2
  exit 1
fi
if [[ ! -f "$FRONTMATTER_HELPER" ]]; then
  echo "ERROR: $FRONTMATTER_HELPER not found" >&2
  exit 1
fi

# PyYAML: prefer the host interpreter, else an ephemeral uv env. Homebrew's
# python3 is PEP 668 externally-managed, so pyyaml is routinely absent there and
# `pip install` refuses. Probe uv rather than just detecting it: a uv that
# cannot materialize pyyaml (offline, cold cache) would otherwise fail later
# under the misleading "parse-bundle-manifest.py failed".
if python3 -c 'import yaml' 2>/dev/null; then
  PY=(python3)
elif command -v uv >/dev/null 2>&1 && uv run --no-project --with pyyaml python3 -c 'import yaml' 2>/dev/null; then
  PY=(uv run --no-project --with pyyaml python3)
else
  echo "ERROR: need PyYAML - install it for python3, or install uv" >&2
  exit 1
fi

# Capture parser output to a tempfile and check its exit code. Piping
# through process substitution would discard a non-zero parser rc - if
# the manifest is malformed the loop would silently process partial
# output and report success.
ROWS_FILE="$(mktemp)"
META_FILE="$(mktemp)"
trap 'rm -f "$ROWS_FILE" "$META_FILE" "${TMP_DST:-}"' EXIT
if ! "${PY[@]}" "$PARSER" "$MANIFEST" > "$ROWS_FILE"; then
  echo "ERROR: parse-bundle-manifest.py failed" >&2
  exit 1
fi

# Iterate manifest entries: <type>\t<skill>\t<source>\t<dest>\t<meta_json>
while IFS=$'\t' read -r TYPE SKILL SOURCE DEST META; do
  # Skip blank lines from the parser (shouldn't happen, but be defensive).
  if [[ -z "$TYPE" ]]; then
    continue
  fi
  SRC_PATH="$SOURCE_ROOT/$SOURCE"
  DST_PATH="$TARGET_ROOT/skills/$SKILL/$DEST"

  if [[ ! -f "$SRC_PATH" ]]; then
    echo "ERROR: source not found: $SOURCE" >&2
    exit 1
  fi

  mkdir -p "$(dirname "$DST_PATH")"

  # Write to a tmp file beside the destination, then atomically rename
  # into place. Direct `> "$DST_PATH"` would truncate the existing bundle
  # before python3 runs; if python3 then fails the destination is left
  # empty on disk (set -e aborts the script, but the empty file remains
  # and a subsequent commit can ship it). The tmp + mv pattern keeps the
  # committed bundle valid as long as some prior generator run succeeded.
  TMP_DST="${DST_PATH}.tmp.$$"

  case "$TYPE" in
    file)
      # cp -p preserves mode + timestamps; ensures executable bit copies cleanly.
      cp -p "$SRC_PATH" "$TMP_DST"
      ;;
    reference)
      # Strip frontmatter from the source; write body to dest.
      "${PY[@]}" "$FRONTMATTER_HELPER" strip "$SRC_PATH" > "$TMP_DST"
      ;;
    agent)
      # Rewrite frontmatter as subagent prompt. The parser emits
      # subagent_meta as a compact JSON string in column 5. Convert to YAML
      # via python3 -c so the helper can parse it without us implementing
      # JSON->YAML in bash. Truncate META_FILE first so a prior iteration's
      # content cannot leak through if the inline python3 fails before
      # writing.
      : > "$META_FILE"
      # JSON -> YAML conversion lives in bundle-frontmatter.py so the dump
      # parameters (width=10000, sort_keys=False, allow_unicode=True) stay
      # in one place. Previously this was an inline `python3 -c ...` that
      # could drift from _render_subagent_frontmatter's parameters.
      "${PY[@]}" "$FRONTMATTER_HELPER" json-to-yaml "$META" > "$META_FILE"
      "${PY[@]}" "$FRONTMATTER_HELPER" rewrite "$SRC_PATH" \
        --as subagent --meta-file "$META_FILE" > "$TMP_DST"
      ;;
    *)
      echo "ERROR: unknown bundle type: $TYPE" >&2
      rm -f "$TMP_DST"
      exit 1
      ;;
  esac

  mv "$TMP_DST" "$DST_PATH"

  echo "bundled: $SOURCE -> skills/$SKILL/$DEST [$TYPE]"
done < "$ROWS_FILE"
