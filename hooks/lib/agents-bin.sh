# One resolver for the fno-agents binary, most-local first. This is the shared
# copy of the resolver that hooks/agy-target-stop-hook.sh and
# hooks/inside-leg-report.sh inlined; target-stop-hook.sh keeps its own copy
# under separate ownership.
fno_agents_bin() {
    local root="${1:-.}"
    if [[ -n "${FNO_AGENTS_BIN:-}" ]] && [[ -x "${FNO_AGENTS_BIN}" ]]; then
        printf '%s' "$FNO_AGENTS_BIN"
    elif [[ -x "$root/crates/fno-agents/target/release/fno-agents" ]]; then
        printf '%s' "$root/crates/fno-agents/target/release/fno-agents"
    elif [[ -x "$root/crates/fno-agents/target/debug/fno-agents" ]]; then
        printf '%s' "$root/crates/fno-agents/target/debug/fno-agents"
    else
        command -v fno-agents || printf ''
    fi
}
