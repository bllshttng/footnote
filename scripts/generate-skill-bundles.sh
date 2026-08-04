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

# Verify each pack before bundling any of its rows. Bundling makes a packaged
# agent callable at session start; an agent that fails agent-tools-bounded (or
# any other verify condition) must never be bundled, because activation - which
# verifies - runs later and only gates the role layer, not the bundled agent's
# harness-level invocation. Fail closed: a pack that fails, or cannot be
# verified, is not bundled. Returns 0 pass, 1 fail, 2 unverifiable.
_verify_pack() {
  local manifest="$1"
  # Prefer the IN-TREE fno-py: it reflects the current source (the installed
  # `fno` can be stale and reject a manifest field this branch added).
  if command -v uv >/dev/null 2>&1; then
    uv run --project "$SOURCE_ROOT/cli" fno-py plugins verify "$manifest" >/dev/null 2>&1 || return 1
  elif command -v fno >/dev/null 2>&1; then
    fno plugins verify "$manifest" >/dev/null 2>&1 || return 1
  else
    return 2
  fi
}
declare _VERIFIED_PACKS=""
while IFS=$'\t' read -r TYPE SKILL SOURCE DEST META; do
  [[ "$TYPE" == pack-* ]] || continue
  case " $_VERIFIED_PACKS " in *" $SKILL "*) continue ;; esac
  _VERIFIED_PACKS="$_VERIFIED_PACKS $SKILL"
  manifest="$SOURCE_ROOT/plugins/$SKILL/plugin.yaml"
  [[ -f "$manifest" ]] || { echo "ERROR: pack manifest not found: $manifest" >&2; exit 1; }
  _verify_pack "$manifest"
  rc=$?
  if [ "$rc" -eq 2 ]; then
    echo "ERROR: cannot verify pack $SKILL (no fno or uv available); refusing to bundle unverified" >&2; exit 1
  elif [ "$rc" -ne 0 ]; then
    echo "ERROR: pack $SKILL fails verification; refusing to bundle (run: fno plugins verify $manifest)" >&2; exit 1
  fi
done < "$ROWS_FILE"

# pack-marker check: a bundled pack destination carries a `pack:` key whose
# value equals the DECLARING pack id. The collision guard uses this so the
# generator never clobbers a hand-authored file, a different pack's output, or
# a mismatched-ownership destination. Returns 0 when the marker value matches.
_pack_marker_value() {
  local target="$1"
  local file=""
  if [[ -f "$target" ]]; then
    file="$target"
  elif [[ -d "$target" && -f "$target/SKILL.md" ]]; then
    file="$target/SKILL.md"
  else
    return 0  # no file to read -> empty value
  fi
  awk 'NR==1 && /^---[[:space:]]*$/ {f=1; next} f && /^---[[:space:]]*$/ {exit} f && /^pack:/ {sub(/^pack:[[:space:]]*/,""); gsub(/["'\'']/,""); print; exit}' "$file"
}

_pack_marker_matches() {
  local target="$1" declaring="$2"
  local marker
  marker="$(_pack_marker_value "$target")"
  [[ "$marker" == "$declaring" ]]
}

# Iterate manifest entries: <type>\t<skill>\t<source>\t<dest>\t<meta_json>
while IFS=$'\t' read -r TYPE SKILL SOURCE DEST META; do
  # Skip blank lines from the parser (shouldn't happen, but be defensive).
  if [[ -z "$TYPE" ]]; then
    continue
  fi
  SRC_PATH="$SOURCE_ROOT/$SOURCE"

  # Pack rows land at the plugin paths the harness reads (root-relative);
  # existing types stay under skills/<skill>/.
  case "$TYPE" in
    pack-skill|pack-agent)
      DST_PATH="$TARGET_ROOT/$DEST"
      ;;
    *)
      DST_PATH="$TARGET_ROOT/skills/$SKILL/$DEST"
      ;;
  esac

  # Source existence: a directory for pack-skill, a file otherwise.
  if [[ "$TYPE" == "pack-skill" ]]; then
    [[ -d "$SRC_PATH" ]] || { echo "ERROR: pack skill source dir not found: $SOURCE" >&2; exit 1; }
  else
    [[ -f "$SRC_PATH" ]] || { echo "ERROR: source not found: $SOURCE" >&2; exit 1; }
  fi

  # Collision guard: refuse to overwrite an existing destination unless its
  # `pack:` marker matches the declaring pack id. A hand-authored file (no
  # marker), a different pack's output, or a mismatched-ownership destination
  # all refuse - so a pack skill named `target` can never clobber the plugin's
  # own driver skill, and one pack can never overwrite another's bundle.
  if [[ "$TYPE" == pack-* && -e "$DST_PATH" ]] && ! _pack_marker_matches "$DST_PATH" "$SKILL"; then
    echo "ERROR: refusing to overwrite $DST_PATH: existing path's 'pack:' marker does not match declaring pack '$SKILL'" >&2
    exit 1
  fi

  mkdir -p "$(dirname "$DST_PATH")"

  # Write to a tmp path beside the destination, then atomically rename into
  # place. Direct writes would truncate the existing bundle before the copy
  # completes; the tmp + mv pattern keeps the committed bundle valid as long
  # as some prior generator run succeeded.
  TMP_DST="${DST_PATH}.tmp.$$"
  rm -rf "$TMP_DST"

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
      # via the helper so the dump parameters stay in one place.
      : > "$META_FILE"
      "${PY[@]}" "$FRONTMATTER_HELPER" json-to-yaml "$META" > "$META_FILE"
      "${PY[@]}" "$FRONTMATTER_HELPER" rewrite "$SRC_PATH" \
        --as subagent --meta-file "$META_FILE" > "$TMP_DST"
      ;;
    pack-agent)
      # A pack agent is copied verbatim: its frontmatter is the source of truth
      # (verified at agent-tools-bounded), so no rewrite.
      cp -p "$SRC_PATH" "$TMP_DST"
      ;;
    pack-skill)
      # A pack skill is a directory copied recursively.
      cp -R "$SRC_PATH" "$TMP_DST"
      ;;
    *)
      echo "ERROR: unknown bundle type: $TYPE" >&2
      rm -rf "$TMP_DST"
      exit 1
      ;;
  esac

  # pack-skill staged a directory; replace the destination whole.
  if [[ "$TYPE" == "pack-skill" && -e "$DST_PATH" ]]; then
    rm -rf "$DST_PATH"
  fi
  mv "$TMP_DST" "$DST_PATH"

  echo "bundled: $SOURCE -> $DEST [$TYPE]"
done < "$ROWS_FILE"
