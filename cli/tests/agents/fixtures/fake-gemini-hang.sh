#!/usr/bin/env bash
# Fake gemini shim that hangs instead of doing real work, used by the
# AC-FR signal-handling tests. The provider module's parser does
# `json.load(proc.stdout)` AFTER `proc.wait()`, so a stalled shim that
# never EOFs simulates "gemini is wedged" — the watchdog timer or a
# SIGINT must reap it.
#
# Behavior toggled by the first positional arg:
#   create        emit nothing on stdout, sleep FAKE_HANG_SECS (default 60).
#                 The parser blocks on proc.stdout.read() until SIGTERM
#                 closes the pipe.
#   resume        same as create; the real provider treats resume
#                 identically at the subprocess driver level.
#   exit-1        emit a banner on stderr then exit 1 with empty stdout.
#                 Exercises the GeminiInvocationError branch.
#   echo-then-hang  emit a partial-but-invalid JSON snippet on stdout,
#                   then sleep forever. The parser sees bytes but no EOF,
#                   so SIGTERM-via-watchdog is the only exit route.
#
# Notes:
# - This shim NEVER touches stderr unless explicitly testing it
#   (the providers/gemini.py module keeps stderr on a separate pipe;
#    untargeted stderr writes corrupt the integration-test assertions).
# - The shim is invoked via Popen with start_new_session=True so signals
#   targeting the process group reach this script's descendants.

set -u

MODE="${1:-create}"
HANG="${FAKE_HANG_SECS:-60}"

case "$MODE" in
    create|resume)
        # Block forever on stdout silence. The watchdog timer in the
        # provider module fires SIGTERM at the process group; this
        # script's sleep is interruptible so SIGTERM exits us cleanly.
        sleep "$HANG"
        ;;
    echo-then-hang)
        printf '{"session_id": "11111111-1111-1111-1111-111111111111", "partial'
        sleep "$HANG"
        ;;
    flood-stderr-then-emit)
        # Codex P1 + Gemini high-priority deadlock regression (PR #317).
        # Floods stderr beyond the kernel pipe buffer (64KB) BEFORE
        # emitting the stdout JSON blob. Pre-fix, the provider's
        # sequential `proc.stdout.read()` would deadlock here because
        # gemini blocked on stderr write while parent blocked on stdout
        # read. Post-fix, the concurrent stderr drainer keeps gemini
        # unblocked and the stdout JSON arrives normally.
        for i in $(seq 1 4000); do
            echo "noisy-warning-line-${i}" >&2
        done
        # Real JSON blob after the stderr flood (one-shot at EOF).
        cat <<EOF
{"session_id": "fa1afe11-1111-2222-3333-444444444444", "response": "post-flood ok", "stats": {}}
EOF
        ;;
    exit-1)
        echo "fake gemini: simulated startup failure" >&2
        exit 1
        ;;
    *)
        echo "fake-gemini-hang.sh: unknown mode '$MODE'" >&2
        exit 2
        ;;
esac
