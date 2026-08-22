#!/usr/bin/env bash
# The spawn-seed provenance sidecar (node x-3a64).
#
# Four properties, and each one is a way the hook could quietly do the wrong
# thing rather than fail loudly:
#   1. a startup with seed fields emits ONE <fno_mail> quoting the seed verbatim
#   2. a compaction emits nothing (else one spawn overcounts as several)
#   3. a hand-started session emits nothing (no peer sender to name)
#   4. a corrupt sidecar emits nothing rather than a half-attributed envelope

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/spawn-seed-provenance-session-start.sh"
# A linked worktree has no venv of its own; the canonical checkout owns it, the
# same way `fno doctor test` resolves an interpreter and then pins THIS tree's
# PYTHONPATH. Falling straight to a bare python3 would run against a system
# interpreter with no fno dependencies and fail for a reason unrelated to the hook.
PY="$REPO_ROOT/cli/.venv/bin/python"
if [[ ! -x "$PY" ]]; then
    CANONICAL="$(git -C "$REPO_ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
    CANONICAL="${CANONICAL%/.git}"
    PY="$CANONICAL/cli/.venv/bin/python"
fi
[[ -x "$PY" ]] || PY="python3"

fail=0
check() {
    local name="$1" expected="$2" actual="$3"
    if [[ "$actual" == "$expected" ]]; then
        printf 'ok   %s\n' "$name"
    else
        printf 'FAIL %s\n  expected: %s\n  actual:   %s\n' "$name" "$expected" "$actual"
        fail=1
    fi
}

# Render with the SOURCE fno, not whatever is deployed: this test exists to
# check the hook against the tree it ships from, and a stale installed binary
# would make it pass or fail on the wrong code.
SHIM="$(mktemp -d)/fno"
cat >"$SHIM" <<SHIMEOF
#!/usr/bin/env bash
exec env PYTHONPATH="$REPO_ROOT/cli/src" "$PY" -c 'from fno.cli import app; app()' "\$@"
SHIMEOF
chmod +x "$SHIM"
export FNO_BIN="$SHIM"

SEED='/fno:target x-1234'
SEED_B64="$("$PY" -c 'import base64,sys; print(base64.b64encode(sys.argv[1].encode()).decode())' "$SEED")"

seed_env=(
    "FNO_SEED_PROV_SEED_B64=$SEED_B64"
    "FNO_SEED_PROV_FROM=119e3c52"
    "FNO_SEED_PROV_FROM_SESSION=119e3c52-0000-7000-8000-000000000000"
    "FNO_SEED_PROV_HARNESS=claude-code"
    "FNO_SEED_PROV_MODEL=claude-opus-5"
    "FNO_SEED_PROV_NODE=x-1234"
    "FNO_SEED_PROV_MSG_ID=msg-abc123"
)

run_hook() {
    # $1 = SessionStart source; remaining args are extra env assignments.
    local source="$1"; shift
    printf '{"source":"%s"}' "$source" \
        | env CLAUDE_PLUGIN_ROOT="$REPO_ROOT" FNO_BIN="$FNO_BIN" "$@" bash "$HOOK" 2>/dev/null
}

# 1. startup + seed fields -> exactly one envelope, quoting the seed verbatim.
out="$(run_hook startup "${seed_env[@]}")"
opens="$(printf '%s' "$out" | grep -c '<fno_mail ' || true)"
closes="$(printf '%s' "$out" | grep -c '</fno_mail>' || true)"
check "startup emits one open tag" "1" "$opens"
check "startup emits one close tag" "1" "$closes"
if printf '%s' "$out" | grep -q 'from_session=\\"119e3c52-0000-7000-8000-000000000000\\"'; then
    printf 'ok   full sender session is the reply address\n'
else
    printf 'FAIL full sender session is the reply address\n  actual: %s\n' "$out"
    fail=1
fi
# The seed is quoted VERBATIM: the sidecar's whole claim is that a reader can
# see the exact message that defined this worker's task.
if printf '%s' "$out" | grep -qF '/fno:target x-1234'; then
    printf 'ok   the seed is quoted verbatim\n'
else
    printf 'FAIL the seed is quoted verbatim\n  actual: %s\n' "$out"
    fail=1
fi

# 2. compaction is silent. SessionStart fires on compact too, and emitting
#    there puts a second copy of one spawn's envelope in the transcript.
out="$(run_hook compact "${seed_env[@]}")"
check "compact emits no envelope" "" "$(printf '%s' "$out" | grep -o '</fno_mail>' || true)"

# 3. resume is silent for the same reason.
out="$(run_hook resume "${seed_env[@]}")"
check "resume emits no envelope" "" "$(printf '%s' "$out" | grep -o '</fno_mail>' || true)"

# 4. a hand-started session has no peer sender, so there is nothing to
#    attribute and inventing one would be the same lie in the other direction.
out="$(run_hook startup)"
check "no seed fields emits no envelope" "" "$(printf '%s' "$out" | grep -o '</fno_mail>' || true)"

# 5. an unusable sidecar refuses rather than emitting a half-attributed
#    envelope. A corrupt blob is not evidence about who sent the seed.
out="$(run_hook startup "FNO_SEED_PROV_SEED_B64=not!valid!base64" \
    "FNO_SEED_PROV_FROM_SESSION=119e3c52-0000-7000-8000-000000000000")"
check "corrupt seed emits no envelope" "" "$(printf '%s' "$out" | grep -o '</fno_mail>' || true)"

# 6. an unreadable source fails CLOSED. If the hook cannot tell a startup from a
#    compaction it must not emit: the wrong guess duplicates the envelope on
#    every compaction for the rest of the session.
out="$(printf 'not json' | env CLAUDE_PLUGIN_ROOT="$REPO_ROOT" FNO_BIN="$FNO_BIN" "${seed_env[@]}" bash "$HOOK" 2>/dev/null)"
check "unreadable source emits no envelope" "" "$(printf '%s' "$out" | grep -o '</fno_mail>' || true)"

# 7. a seed carrying a control byte still emits VALID JSON. The escaper covers
#    backslash, quote, LF, CR and TAB and nothing else, so an ESC from a pasted
#    terminal capture used to make the emitted object unparseable. The harness
#    then discards the WHOLE payload, which means the sidecar does not degrade,
#    it vanishes. Asserted by parsing the output rather than by grepping it: a
#    grep for the envelope passes on malformed JSON, which is the failure.
ESC_SEED="$("$PY" -c 'import base64; print(base64.b64encode(b"/fno:target x-1234 \x1b[31mred\x1b[0m").decode())')"
out="$(run_hook startup "FNO_SEED_PROV_SEED_B64=$ESC_SEED" \
    "FNO_SEED_PROV_FROM=119e3c52" \
    "FNO_SEED_PROV_FROM_SESSION=119e3c52-0000-7000-8000-000000000000" \
    "FNO_SEED_PROV_HARNESS=claude-code" \
    "FNO_SEED_PROV_MODEL=claude-opus-5" \
    "FNO_SEED_PROV_MSG_ID=msg-abc123")"
check "a control byte in the seed still emits parseable JSON" "ok" \
    "$(printf '%s' "$out" | "$PY" -c 'import json,sys
try:
    json.load(sys.stdin)
    print("ok")
except Exception as exc:
    print(f"unparseable: {exc}")')"
check "the envelope survives the strip" "1" \
    "$(printf '%s' "$out" | grep -c '</fno_mail>' || true)"

exit "$fail"
