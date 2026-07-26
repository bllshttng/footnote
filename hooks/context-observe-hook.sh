#!/usr/bin/env bash
# Preserve one hook's exact output while recording its delivered directive bytes.
set -uo pipefail

observer_args=()
original=()
found_command=0
while (($#)); do
    if [[ "$1" == "--" ]]; then
        shift
        original=("$@")
        found_command=1
        break
    fi
    observer_args+=("$1")
    shift
done

(( found_command )) && ((${#original[@]})) || exit 0

SOURCE_ID=""
EXPECTED=""
ENTRY=""
valid_args=1
set -- "${observer_args[@]}"
while (($#)); do
    case "$1" in
        --source-id)
            (($# >= 2)) || { valid_args=0; break; }
            SOURCE_ID="$2"
            shift 2
            ;;
        --expected)
            (($# >= 2)) || { valid_args=0; break; }
            EXPECTED="$2"
            shift 2
            ;;
        --entry)
            (($# >= 2)) || { valid_args=0; break; }
            ENTRY="$2"
            shift 2
            ;;
        --final)
            shift
            ;;
        *)
            valid_args=0
            break
            ;;
    esac
done

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_ROOT="$(cd "$HOOK_DIR/.." && pwd)"
HELPER="$PLUGIN_ROOT/cli/src/fno/context_observation.py"
if (( ! valid_args )) || [[ -z "$SOURCE_ID" || -z "$EXPECTED" ]] \
    || [[ ! -f "$HELPER" ]] || ! command -v python3 >/dev/null 2>&1 \
    || ! command -v uv >/dev/null 2>&1; then
    exec "${original[@]}"
fi

TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fno-context-hook.XXXXXX" 2>/dev/null || true)"
[[ -n "$TEMP_DIR" ]] || exec "${original[@]}"
INPUT_FILE="$TEMP_DIR/input.json"
OUTPUT_FILE="$TEMP_DIR/output"
if [[ ! -t 0 ]]; then
    cat >"$INPUT_FILE" 2>/dev/null || : >"$INPUT_FILE"
else
    : >"$INPUT_FILE"
fi

set +e
"${original[@]}" <"$INPUT_FILE" >"$OUTPUT_FILE"
HOOK_RC=$?
cat "$OUTPUT_FILE"

record_args=(
    record
    --source-id "$SOURCE_ID"
    --expected "$EXPECTED"
    --carrier "${original[*]}"
    --input-file "$INPUT_FILE"
    --output-file "$OUTPUT_FILE"
    --hook-rc "$HOOK_RC"
)
[[ -n "$ENTRY" ]] && record_args+=(--entry "$ENTRY")
python3 "$HELPER" "${record_args[@]}" >/dev/null 2>&1 || true

collect_args=(collect --expected "$EXPECTED" --input-file "$INPUT_FILE")
[[ -n "$ENTRY" ]] && collect_args+=(--entry "$ENTRY")
python3 "$HELPER" run-bounded \
    --timeout "${FNO_CONTEXT_OBSERVER_TIMEOUT_SECONDS:-3}" -- \
    uv run --project "$PLUGIN_ROOT/cli" python3 "$HELPER" \
    "${collect_args[@]}" >/dev/null 2>&1 || true

rm -f "$INPUT_FILE" "$OUTPUT_FILE" 2>/dev/null || true
rmdir "$TEMP_DIR" 2>/dev/null || true
exit "$HOOK_RC"
