#!/usr/bin/env bash
# Count the operator notices a phone sink would forward in a window, and the
# done badge transitions behind them. Measured over one 7h54m window before
# the producer fix: 402 notices, 393 of them done badges with body `done`.
# After the fix a pass is zero done rows while the agents journal still shows
# at least one done transition in the same window - a zero with no
# transitions proves nothing (the window would also be quiet if the daemon
# were down).
#
# Read-only. Bash and jq. Exit 0 = quiet window. Exit 1 = done rows reached
# the journal (the first offender's ts and title are printed). Exit 2 = the
# window proves nothing: no done transition in it, or no daemon and no
# --since given.
#
# Usage:
#   bash scripts/probes/phone-notice-noise-probe.sh [--since <iso>] [--until <iso>]
#        [--space-events <path>] [--agents-events <path>] [--self-test]
# The default window runs from the daemon start (from
# `fno-agents status --json`, daemon.pid_start_time) to now.

set -u

if [ "${1:-}" = "--self-test" ]; then
    tmp="$(mktemp -d)" || exit 2
    trap 'rm -rf "$tmp"' EXIT

    # Case A: a done badge plus a done transition -> exit 1, name the row.
    printf '%s\n' '{"ts":"2026-09-15T01:00:00Z","type":"operator_notice","data":{"title":"king-a","body":"done"}}' > "$tmp/a-space.jsonl"
    printf '%s\n' '{"ts":"2026-09-15T01:00:01Z","type":"inside_leg_report","data":{"state":"done"}}' > "$tmp/a-agents.jsonl"
    out="$("$0" --space-events "$tmp/a-space.jsonl" --agents-events "$tmp/a-agents.jsonl" --since "2026-09-15T01:00:00Z" --until "2026-09-15T02:00:00Z" 2>&1)"
    rc=$?
    if [ "$rc" -eq 1 ] && printf '%s' "$out" | grep -q 'done=1' && printf '%s' "$out" | grep -q 'king-a'; then
        echo "case A: ok"
    else
        echo "case A: FAIL rc=$rc out=$out" >&2
        exit 1
    fi

    # Case B: a blocked notice plus a done transition -> exit 0.
    printf '%s\n' '{"ts":"2026-09-15T02:00:00Z","type":"operator_notice","data":{"title":"king-b","body":"Claude needs your permission"}}' > "$tmp/b-space.jsonl"
    printf '%s\n' '{"ts":"2026-09-15T02:00:01Z","type":"inside_leg_report","data":{"state":"done"}}' > "$tmp/b-agents.jsonl"
    out="$("$0" --space-events "$tmp/b-space.jsonl" --agents-events "$tmp/b-agents.jsonl" --since "2026-09-15T02:00:00Z" --until "2026-09-15T03:00:00Z" 2>&1)"
    rc=$?
    if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q 'notices=1 done=0 transitions=1'; then
        echo "case B: ok"
    else
        echo "case B: FAIL rc=$rc out=$out" >&2
        exit 1
    fi

    # Case C: no done transition in the window -> exit 2.
    : > "$tmp/c-agents.jsonl"
    out="$("$0" --space-events "$tmp/b-space.jsonl" --agents-events "$tmp/c-agents.jsonl" --since "2026-09-15T02:00:00Z" --until "2026-09-15T03:00:00Z" 2>&1)"
    rc=$?
    if [ "$rc" -eq 2 ]; then
        echo "case C: ok"
    else
        echo "case C: FAIL rc=$rc out=$out" >&2
        exit 1
    fi

    echo "self-test: ok"
    exit 0
fi

SINCE=""
UNTIL=""
SPACE=""
AGENTS=""
while [ $# -gt 0 ]; do
    case "$1" in
        --since) SINCE="$2"; shift 2 ;;
        --until) UNTIL="$2"; shift 2 ;;
        --space-events) SPACE="$2"; shift 2 ;;
        --agents-events) AGENTS="$2"; shift 2 ;;
        *)
            echo "probe: unknown flag: $1" >&2
            exit 2
            ;;
        esac
done

command -v jq >/dev/null 2>&1 || {
    echo "probe: jq is required" >&2
    exit 2
}

# Default window: the daemon start to now.
if [ -z "$SINCE" ] && [ -z "$UNTIL" ]; then
    start_us="$(fno-agents status --json 2>/dev/null | jq -r '.daemon.pid_start_time // empty')"
    if [ -z "$start_us" ]; then
        echo "probe: no daemon and no --since; pass --since <iso>" >&2
        exit 2
    fi
    SINCE="$(date -u -r "$((start_us / 1000000))" +%Y-%m-%dT%H:%M:%SZ)"
fi

if [ -z "$SPACE" ]; then
    SPACE="$(fno-agents state path events 2>/dev/null | tail -1)"
    if [ -z "$SPACE" ]; then
        echo "probe: cannot resolve the space events path" >&2
        exit 2
    fi
fi
AGENTS="${AGENTS:-${FNO_AGENTS_HOME:-$HOME/.fno/agents}/events.jsonl}"

S19="${SINCE:0:19}"
U19="${UNTIL:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"
U19="${U19:0:19}"

# One temp stream for the tagged rows; appended per file, never captured,
# so command-substitution trailing-newline stripping cannot glue rows.
streams="$(mktemp)" || exit 2
trap '/bin/rm -f "$streams"' EXIT

for f in "$SPACE.1" "$SPACE"; do
    [ -f "$f" ] || continue
    jq -r --arg s "$S19" --arg u "$U19" '
        select(.type == "operator_notice"
               and ((.ts // "") | .[0:19]) >= $s
               and ((.ts // "") | .[0:19]) <= $u) |
        if (.data.body // "") == "done"
        then "D\t" + (.ts | .[0:19]) + "\t" + (.data.title // "?")
        else "N\t" + (.ts | .[0:19]) + "\t" + (.data.title // "?")
        end' "$f" 2>/dev/null >> "$streams"
done
for f in "$AGENTS.1" "$AGENTS"; do
    [ -f "$f" ] || continue
    jq -r --arg s "$S19" --arg u "$U19" '
        select((.type // .kind // "") == "inside_leg_report"
               and ((.data.state // .state // "") == "done")
               and ((.ts // "") | .[0:19]) >= $s
               and ((.ts // "") | .[0:19]) <= $u) | "T"' "$f" 2>/dev/null >> "$streams"
done

notices="$(grep -c '^[ND]' "$streams" || true)"
done_count="$(grep -c '^D' "$streams" || true)"
transitions="$(grep -c '^T' "$streams" || true)"
printf 'since=%s until=%s notices=%s done=%s transitions=%s\n' "$S19" "$U19" "$notices" "$done_count" "$transitions"

if [ "$transitions" -eq 0 ]; then
    echo "probe: no done transition in the window; it proves nothing" >&2
    exit 2
fi
if [ "$done_count" -gt 0 ]; then
    first="$(grep '^D' "$streams" | head -1)"
    ts="$(printf '%s' "$first" | cut -f2)"
    title="$(printf '%s' "$first" | cut -f3)"
    echo "probe: done rows reached the journal; first: $ts title=$title" >&2
    exit 1
fi
exit 0
