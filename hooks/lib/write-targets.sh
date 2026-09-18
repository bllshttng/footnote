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
