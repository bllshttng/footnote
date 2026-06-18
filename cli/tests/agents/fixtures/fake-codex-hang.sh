#!/usr/bin/env bash
# Fake codex shim that hangs (sleeps) instead of doing real work, used by
# the AC-FR signal-handling tests. The exact stdout shape mimics what
# `codex exec --json` emits up to (but not including) the turn.completed
# event, so the parser stays in its read loop until SIGTERM / SIGINT
# arrives or the watchdog fires.
#
# Behavior toggled by the first positional arg:
#   create   - emit a thread.started event then sleep for FAKE_HANG_SECS
#              (default 60). The parser captures session_id but never sees
#              turn.completed, mimicking a stuck codex create.
#   resume   - emit nothing useful, just sleep. Used for follow-up timeout
#              test (we don't need session_id capture on the resume path).
#   exit-1   - emit a banner then exit 1 without any JSONL. Used to verify
#              the "no session id" branch fires with an empty types_seen
#              set.
#
# stdout is the JSONL stream the parser drains via Popen(stdout=PIPE).
# stderr is intentionally NOT touched here because the real providers/codex.py
# sets stderr=subprocess.STDOUT, which would merge any stderr writes into
# the same pipe and confuse the test's signal-handling assertions.

set -u

MODE="${1:-create}"
HANG="${FAKE_HANG_SECS:-60}"

case "$MODE" in
    create)
        echo '{"type":"thread.started","thread_id":"019e0000-fake-7000-aaaa-cccccccccccc"}'
        sleep "$HANG"
        # If we somehow exit the sleep cleanly, emit a completion so the
        # parser doesn't deadlock on a real test run.
        echo '{"type":"turn.completed","usage":{}}'
        ;;
    complete-then-hang)
        # Emit a full happy-path stream including turn.completed, then
        # sleep forever. Exercises _wait_with_grace's process-group kill
        # path: the parser breaks out of the read loop on turn.completed
        # but the subprocess never EOFs / exits, so _wait_with_grace
        # must SIGTERM (then SIGKILL on overrun) the process group to
        # reap. Gemini code review on PR #305 caught the test gap.
        echo '{"type":"thread.started","thread_id":"019e0000-fake-7000-aaaa-cccccccccccc"}'
        echo '{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"hung after complete"}}'
        echo '{"type":"turn.completed","usage":{}}'
        sleep "$HANG"
        ;;
    resume)
        sleep "$HANG"
        echo '{"type":"turn.completed","usage":{}}'
        ;;
    exit-1)
        echo "fake codex: simulated startup failure" >&2
        exit 1
        ;;
    *)
        echo "fake-codex-hang.sh: unknown mode '$MODE'" >&2
        exit 2
        ;;
esac
