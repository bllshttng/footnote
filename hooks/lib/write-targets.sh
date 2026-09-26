#!/usr/bin/env bash
# write-targets.sh - the paths one Edit, Write, Bash, or codex apply_patch call writes.
#
# write_targets FILE_PATH PATCH_COMMAND -> one path per line. A file_path
# payload contributes one path; a Bash command contributes its write-form
# targets; an apply_patch command contributes its header paths. Paths come
# back as written in the payload (relative or absolute).
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
    bash_write_targets "$patch_command"
    return 0
}

# bash_write_targets COMMAND -> one path token per line for every write form the
# command carries. Enumerated floor mirroring graph-write-protect.sh's
# _bash_targets_protected: redirects, tee, sponge, cp/mv/install/truncate (last
# argument), dd of=, in-place editors. A token is a run of non-separator,
# non-quote characters, so a quoted path holding spaces is not seen; leftmost
# clause wins when a form repeats. Positional and in-place arms keep a
# [[:space:]] before the token so greedy backtracking yields the whole final
# token, not its last character.
bash_write_targets() {
    local cmd="$1" tok re clean
    [[ -n "$cmd" ]] || return 0
    tok="([^[:space:];|&<>\"']+)"
    # Redirects read off the raw command: > >> 2> 2>> &> >| >!
    re='[0-9]*>>?[[:space:]]*'"$tok"
    [[ "$cmd" =~ $re ]] && printf '%s\n' "${BASH_REMATCH[1]}"
    re='&>[[:space:]]*'"$tok"
    [[ "$cmd" =~ $re ]] && printf '%s\n' "${BASH_REMATCH[1]}"
    re='(^|[^[:alnum:]_])tee[[:space:]]+(-[^[:space:]]+[[:space:]]+)*'"$tok"
    [[ "$cmd" =~ $re ]] && printf '%s\n' "${BASH_REMATCH[3]}"
    re='(^|[^[:alnum:]_])sponge[[:space:]]+'"$tok"
    [[ "$cmd" =~ $re ]] && printf '%s\n' "${BASH_REMATCH[2]}"
    # Positional forms read off a copy with redirect clauses stripped, so the
    # last argument of a cp is not a trailing 2>/dev/null.
    clean="$(_strip_bash_redirects "$cmd")"
    re='(^|[^[:alnum:]_])(cp|mv|install|truncate)[[:space:]][^;|&]*[[:space:]]'"$tok"
    [[ "$clean" =~ $re ]] && printf '%s\n' "${BASH_REMATCH[3]}"
    re='(^|[^[:alnum:]_])dd[[:space:]][^;|&]*of='"$tok"
    [[ "$clean" =~ $re ]] && printf '%s\n' "${BASH_REMATCH[2]}"
    re='(^|[^[:alnum:]_])(sed|perl)[[:space:]][^;|&]*(-[a-zA-Z]*i|--in-place)[^;|&]*[[:space:]]'"$tok"
    [[ "$clean" =~ $re ]] && printf '%s\n' "${BASH_REMATCH[4]}"
    re='(^|[^[:alnum:]_])jq[[:space:]][^;|&]*(-i|--in-place)[^;|&]*[[:space:]]'"$tok"
    [[ "$clean" =~ $re ]] && printf '%s\n' "${BASH_REMATCH[3]}"
    re='(^|[^[:alnum:]_])(ex|ed)[[:space:]][^;|&]*[[:space:]]'"$tok"
    [[ "$clean" =~ $re ]] && printf '%s\n' "${BASH_REMATCH[3]}"
    return 0
}

# _strip_bash_redirects CMD -> CMD with every file-redirect clause removed
# (fd dups first, then each operator + target), so positional write forms see
# their real last argument.
_strip_bash_redirects() {
    local tok='[^[:space:];|&<>"'\'']+' cmd="$1"
    cmd="$(printf '%s' "$cmd" | sed -E -e 's/[0-9]*>&[0-9]+//g' \
        -e "s/(^|[[:space:]])(([0-9]*>>?|&>|>\\||>!)[[:space:]]*${tok})/\\1/g")"
    printf '%s\n' "$cmd"
}
