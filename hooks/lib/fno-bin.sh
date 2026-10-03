# One resolver for the `fno` front door, new bg-session PATH gap. Companion to
# agents-bin.sh (fno-agents): a running session's PATH predates an in-session
# install, so bare `command -v fno` misreads an installed CLI as missing and
# the frontdoor hooks then rerun the installer over a working install
# (2026-10-02 gap audit 1 / hurdle A5, fresh-machine-hurdles B4). Checks the
# known install dirs before answering empty.
fno_bin() {
    if command -v fno >/dev/null 2>&1; then
        command -v fno
    elif [[ -x "${HOME:-}/.cargo/bin/fno" ]] ; then
        printf '%s' "$HOME/.cargo/bin/fno"
    elif [[ -x "${HOME:-}/.local/bin/fno" ]] ; then
        printf '%s' "$HOME/.local/bin/fno"
    else
        printf ''
    fi
}
