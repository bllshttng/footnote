#!/usr/bin/env bash
# generated-write-guard.sh - PreToolUse hook: refuse Edit and Write (and codex
# apply_patch) on generated copies, because the next regeneration silently
# wipes the edit. The refusal names the canonical source and the regen command.
#
# A path is generated when:
#   - it sits under an installed plugin copy (`/plugin-stage/fno/`), which
#     `fno doctor update` restages from the source checkout;
#   - it matches a row of `generated-artifacts.tsv` at the repo root; or
#   - it is a bundle destination in `skill-bundles.yaml` at the repo root.
#
# Any repo with neither manifest file approves every call. A plugin user who
# wants the same refusal adds a `generated-artifacts.tsv` to their own repo:
# tab-separated `path-glob`, `canonical source`, `regenerate command` per line.
#
# Exit 0 always (hook result is communicated via stdout JSON).
set -uo pipefail

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/guard-mark.sh
source "$HOOK_DIR/lib/guard-mark.sh" 2>/dev/null || true

_approve() {
    _guard_mark generated-write-guard allow 2>/dev/null || true
    printf '%s\n' '{}'
    exit 0
}

_block() {
    _guard_mark generated-write-guard block 2>/dev/null || true
    local reason="$1"
    if command -v jq >/dev/null 2>&1; then
        jq -nc --arg reason "$reason" '{
            decision: "block",
            reason: $reason,
            hookSpecificOutput: {
                hookEventName: "PreToolUse",
                permissionDecision: "deny",
                permissionDecisionReason: $reason
            }
        }'
    else
        python3 -c 'import json,sys; r=sys.argv[1]; print(json.dumps({"decision":"block","reason":r,"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":r}}))' "$reason"
    fi
    exit 0
}

# shellcheck source=lib/write-targets.sh
source "$HOOK_DIR/lib/write-targets.sh" 2>/dev/null || _approve

PAYLOAD="$(cat)"
CWD="" FILE_PATH="" PATCH_COMMAND=""
if command -v jq >/dev/null 2>&1; then
    {
        IFS= read -r -d '' CWD
        IFS= read -r -d '' FILE_PATH
        IFS= read -r -d '' PATCH_COMMAND
    } < <(printf '%s' "$PAYLOAD" | jq -j '
        def s: if type == "string" then . else "" end;
        (.cwd | s), "\u0000", (.tool_input.file_path | s), "\u0000",
        (.tool_input.command | s), "\u0000"' 2>/dev/null)
elif command -v python3 >/dev/null 2>&1; then
    {
        IFS= read -r -d '' CWD
        IFS= read -r -d '' FILE_PATH
        IFS= read -r -d '' PATCH_COMMAND
    } < <(printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin); ti = d.get("tool_input") or {}
    s = lambda v: v if isinstance(v, str) else ""
    sys.stdout.write("\0".join([s(d.get("cwd")), s(ti.get("file_path")), s(ti.get("command"))]) + "\0")
except Exception:
    pass' 2>/dev/null)
else
    _approve
fi

TARGETS=()
while IFS= read -r t; do
    TARGETS+=("$t")
done < <(write_targets "$FILE_PATH" "$PATCH_COMMAND")
[[ ${#TARGETS[@]} -gt 0 ]] || _approve

# _physical ABS -> ABS with its nearest existing ancestor resolved physically,
# so a symlinked temp dir compares equal to the toplevel git prints.
_physical() {
    local dir="$1" rest=""
    while [[ ! -d "$dir" ]]; do
        rest="/${dir##*/}$rest"
        dir="${dir%/*}"
        [[ -n "$dir" ]] || dir="/"
    done
    printf '%s%s\n' "$(cd -P "$dir" 2>/dev/null && pwd -P)" "$rest"
}

# Bundle rows are parsed at most once per repo root per call.
BUNDLE_ROWS=""
BUNDLE_ROOT=""
_load_bundle_rows() {
    local root="$1" parser="$1/scripts/lib/parse-bundle-manifest.py"
    [[ "$BUNDLE_ROOT" == "$root" ]] && return 0
    BUNDLE_ROOT="$root"
    BUNDLE_ROWS="$(cd "$root" && python3 "$parser" skill-bundles.yaml 2>/dev/null)" \
        || BUNDLE_ROWS="$(cd "$root" && uv run --no-project --with pyyaml python3 "$parser" skill-bundles.yaml 2>/dev/null)" \
        || { BUNDLE_ROWS=""; echo "generated-write-guard: could not parse skill-bundles.yaml; bundle copies are not guarded for this call" >&2; }
}

for t in "${TARGETS[@]}"; do
    case "$t" in
        /*) abs="$t" ;;
        *)  [[ -n "$CWD" ]] || continue; abs="$CWD/$t" ;;
    esac

    abs="$(_physical "$abs")"
    if [[ "$abs" == */plugin-stage/fno/* ]]; then
        inner="${abs#*/plugin-stage/fno/}"
        _block "$abs is the installed plugin copy. \`fno doctor update\` restages it from the footnote source checkout and discards this edit. Edit $inner in a feature worktree of the source checkout, then run \`fno doctor update\`."
    fi

    dir="${abs%/*}"
    while [[ -n "$dir" && ! -d "$dir" ]]; do dir="${dir%/*}"; done
    root="$(git -C "${dir:-/}" rev-parse --show-toplevel 2>/dev/null)" || continue
    [[ -n "$root" && "$abs" == "$root/"* ]] || continue
    rel="${abs#"$root"/}"

    manifest="$root/generated-artifacts.tsv"
    if [[ -f "$manifest" ]]; then
        while IFS=$'\t' read -r glob source regen; do
            [[ -z "$glob" || "$glob" == \#* ]] && continue
            # shellcheck disable=SC2053
            if [[ "$rel" == $glob ]]; then
                _block "$rel is generated from $source. The next \`$regen\` run overwrites an edit here. Edit $source, then run \`$regen\`."
            fi
        done < "$manifest"
    fi

    if [[ ( "$rel" == skills/* || "$rel" == agents/* ) && -f "$root/skill-bundles.yaml" && -f "$root/scripts/lib/parse-bundle-manifest.py" ]]; then
        _load_bundle_rows "$root"
        while IFS=$'\t' read -r type skill source dest _meta; do
            [[ -n "$type" ]] || continue
            case "$type" in
                pack-*) out="$dest" ;;
                *)      out="skills/$skill/$dest" ;;
            esac
            # A pack-skill dest is a directory: map the tail onto its source.
            if [[ "$rel" == "$out" || "$rel" == "$out/"* ]]; then
                src="$source${rel#"$out"}"
                _block "$rel is a bundled copy of $src (skill-bundles.yaml). The next \`bash scripts/generate-skill-bundles.sh\` run overwrites an edit here. Edit $src, then run \`bash scripts/generate-skill-bundles.sh\`."
            fi
        done <<< "$BUNDLE_ROWS"
    fi
done

_approve
