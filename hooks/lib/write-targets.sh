#!/usr/bin/env bash
# write-targets.sh - the paths one Edit, Write, or codex apply_patch call writes.
#
# write_targets FILE_PATH PATCH_COMMAND -> one path per line. A file_path
# payload contributes one path; an apply_patch command contributes its header
# paths. Paths come back as written in the payload (relative or absolute).
write_targets() {
    local file_path="$1" patch_command="$2" line path
    [[ -n "$file_path" ]] && printf '%s\n' "$file_path"
    [[ -n "$patch_command" ]] || return 0
    while IFS= read -r line; do
        case "$line" in
            "*** Add File: "*|"*** Update File: "*|"*** Delete File: "*|"*** Move to: "*)
                path="${line#*: }"
                path="${path%$'\r'}"
                [[ -n "$path" ]] && printf '%s\n' "$path"
                ;;
        esac
    done <<< "$patch_command"
    return 0
}

# payload_write_targets PAYLOAD -> one path per line on stdout: every path a
# hook payload names, resolved against the payload cwd when relative. A
# claude Edit or Write payload contributes tool_input.file_path; a codex
# apply_patch payload contributes its header paths from
# tool_input.command. jq first, python3 fallback, the same NUL-split read
# hooks/generated-write-guard.sh uses.
payload_write_targets() {
    local payload="$1" cwd file_path patch_command path
    if command -v jq >/dev/null 2>&1; then
        {
            IFS= read -r -d '' cwd
            IFS= read -r -d '' file_path
            IFS= read -r -d '' patch_command
        } < <(printf '%s' "$payload" | jq -j '
        def s: if type == "string" then . else "" end;
        (.cwd | s), "\u0000", (.tool_input.file_path | s), "\u0000",
        (.tool_input.command | s), "\u0000"' 2>/dev/null)
    elif command -v python3 >/dev/null 2>&1; then
        {
            IFS= read -r -d '' cwd
            IFS= read -r -d '' file_path
            IFS= read -r -d '' patch_command
        } < <(printf '%s' "$payload" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin); ti = d.get("tool_input") or {}
    s = lambda v: v if isinstance(v, str) else ""
    sys.stdout.write("\0".join([s(d.get("cwd")), s(ti.get("file_path")), s(ti.get("command"))]) + "\0")
except Exception:
    pass' 2>/dev/null)
    else
        return 0
    fi
    while IFS= read -r path; do
        [[ -n "$path" ]] || continue
        case "$path" in
            /*) printf '%s\n' "$path" ;;
            *) printf '%s\n' "${cwd:+$cwd/}$path" ;;
        esac
    done < <(write_targets "$file_path" "$patch_command")
    return 0
}
