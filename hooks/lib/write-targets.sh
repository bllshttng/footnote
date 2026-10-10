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
    # A patch BODY is file content, not shell: extracting write forms from it
    # would block a legitimate patch whose added text merely mentions a
    # redirect. Only a real Bash command goes through the shell grammar.
    [[ "$patch_command" == *"*** Begin Patch"* ]] || bash_write_targets "$patch_command"
    return 0
}

# bash_write_targets COMMAND -> one path token per line for every write form the
# command carries. Enumerated floor mirroring the write gate's
# _bash_targets_protected: redirects, tee, sponge, cp/mv/install/truncate (last
# argument), dd of=, in-place editors. Every matching clause is reported, not
# just the leftmost, so a second write in a compound command is still seen. A
# token is a fully quoted span or a bare run of non-separator characters, and
# the quotes are stripped from the captured target; a quoted path holding
# spaces therefore resolves whole, while mid-token quote splices stay unseen.
bash_write_targets() {
    local cmd="$1" tok re clean
    [[ -n "$cmd" ]] || return 0
    tok="(\"[^\"]*\"|'[^']*'|[^[:space:];|&<>\"']+)"
    # Redirects read off the raw command: > >> 2> 2>> &> >| >!
    _bwt_each "$cmd" '[0-9]*(>>|>)[[:space:]]*'"$tok" 2
    _bwt_each "$cmd" '(&>|>\||>!)[[:space:]]*'"$tok" 2
    _bwt_each "$cmd" '(^|[^[:alnum:]_])tee[[:space:]]+(-[^[:space:]]+[[:space:]]+)*'"$tok" 3
    _bwt_each "$cmd" '(^|[^[:alnum:]_])sponge[[:space:]]+'"$tok" 2
    # Positional forms read off a copy with redirect clauses stripped, so the
    # last argument of a cp is not a trailing 2>/dev/null.
    clean="$(_strip_bash_redirects "$cmd")"
    _bwt_each "$clean" '(^|[^[:alnum:]_])(cp|mv|install|truncate)[[:space:]][^;|&]*[[:space:]]'"$tok" 3
    _bwt_each "$clean" '(^|[^[:alnum:]_])dd[[:space:]][^;|&]*of='"$tok" 2
    _bwt_each "$clean" '(^|[^[:alnum:]_])(sed|perl)[[:space:]][^;|&]*(-[a-zA-Z]*i|--in-place)[^;|&]*[[:space:]]'"$tok" 4
    _bwt_each "$clean" '(^|[^[:alnum:]_])jq[[:space:]][^;|&]*(-i|--in-place)[^;|&]*[[:space:]]'"$tok" 3
    _bwt_each "$clean" '(^|[^[:alnum:]_])(ex|ed)[[:space:]][^;|&]*[[:space:]]'"$tok" 3
    return 0
}

# _bwt_each CMD RE GROUP -> every capture of GROUP, left to right, quotes
# stripped. One capture per matching clause: a compound command naming two
# generated paths loses neither.
_bwt_each() {
    local s="$1" re="$2" grp="$3" off=0 pre frag q="'"
    while [[ "${s:off}" =~ $re ]]; do
        frag="${BASH_REMATCH[grp]}"
        frag="${frag%\"}"; frag="${frag#\"}"
        frag="${frag%"$q"}"; frag="${frag#"$q"}"
        [[ -n "$frag" ]] && printf '%s\n' "$frag"
        pre="${s:off}"
        pre="${pre%%"${BASH_REMATCH[0]}"*}"
        off=$(( off + ${#pre} + ${#BASH_REMATCH[0]} ))
        (( ${#BASH_REMATCH[0]} > 0 )) || off=$(( off + 1 ))
    done
}

# _strip_bash_redirects CMD -> CMD with every file-redirect clause removed
# (fd dups first, then each operator + target, quoted targets included), so
# positional write forms see their real last argument.
_strip_bash_redirects() {
    local tok='("[^"]*"|'"'"'[^'"'"']*'"'"'|[^[:space:];|&<>"'\'']+)'
    local cmd="$1"
    cmd="$(printf '%s' "$cmd" | sed -E -e 's/[0-9]*>&[0-9]+//g' \
        -e "s/(^|[[:space:]])(([0-9]*>>?|&>|>\\||>!)[[:space:]]*${tok})/\\1/g")"
    printf '%s\n' "$cmd"
}

# payload_write_targets PAYLOAD -> one path per line on stdout: every path a
# hook payload names, resolved against the payload cwd when relative. A
# claude Edit or Write payload contributes tool_input.file_path; a codex
# apply_patch payload contributes its header paths from
# tool_input.command. jq first, python3 fallback, the same NUL-split read
# hooks/write-gate.sh uses. format-on-edit.sh and
# edit-integrity.sh consume this adapter, not write_targets directly.
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
