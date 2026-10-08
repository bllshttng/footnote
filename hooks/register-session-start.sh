#!/usr/bin/env bash
# fno hook: SessionStart - register session start
# SessionStart hook: report this session to the fno daemon so the agent
# registry holds its full session id, transcript path and start source - for a
# SPAWNED worker, or an operator session that opted in.
#
# Two branches:
#   worker (FNO_AGENT_SELF set): one thin call to `fno-agents session-report`,
#     the raw hook payload passed through on stdin. The daemon stamps the row
#     additively (an empty primary session id fills, a resumed id fills the
#     related slot, a third distinct id refuses) so mail and liveness stop
#     guessing ids from transcripts. The verb never lazy-starts a daemon and
#     spools the payload to a capped file when the daemon is down, so a
#     session start never blocks on it. Fail-open, exit 0 always.
#   operator (no row of its own): registers a new row so peers can
#     `fno agents mail send` to it. Gated on agents.auto_register_sessions;
#     runs the Python entry point, which classifies a resumed id against the
#     recorded one (succession vs branch) with transcript reachability
#     evidence the daemon ingest deliberately does not gather.
#
# Hook contract: NEVER blocks session start. The worker path is bounded by the
# shared wall-clock helper (a missing helper exits 0 - same as
# inside-leg-report.sh); the Python path itself swallows any error into a
# `session_register_failed` event (AC7-ERR). stdout stays empty so this hook
# contributes nothing to the session preamble.
#
# Harness coverage: the SAME script reports for every harness fno has a
# session-start hook surface for - claude (hooks/context-hooks.json), codex
# (the session-start.sh wrapper hydrates CODEX_PLUGIN_ROOT), and the
# opencode/pi plugin events - one hooks directory covering every harness.
set -uo pipefail

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

REPO_ROOT="${CLAUDE_PROJECT_DIR:-${GEMINI_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}}"
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLI_DIR="$(cd "$HOOK_DIR/.." && pwd)/cli"

# The report RPC reaches a daemon socket that can stall; bounded with the
# shared wall-clock helper rather than the harness's hook timeout. Sourcing
# fails closed: a missing helper makes this fire-and-forget hook exit 0
# instead of blocking unbounded.
# shellcheck source=scripts/lib/with-timeout.sh
source "$HOOK_DIR/../scripts/lib/with-timeout.sh" 2>/dev/null || exit 0

# Detect the harness and read the SAME session-id env the rest of fno resolves
# on (harness_identity.HARNESS_SESSION_MARKERS): claude uses CLAUDE_CODE_SESSION_ID,
# not CLAUDE_SESSION_ID (the old name here was unset, so claude never registered).
if [[ -n "${GEMINI_PROJECT_DIR:-}" ]]; then
    HARNESS="gemini"; SESSION_ID="${GEMINI_SESSION_ID:-}"
elif [[ -n "${CODEX_PLUGIN_ROOT:-}" ]]; then
    HARNESS="codex"; SESSION_ID=""
    # Same-family rule the resolvers enforce: two DIFFERENT codex ids are a
    # disagreement, so no id is registered rather than the table-first guess.
    if [[ -z "${CODEX_THREAD_ID:-}" || -z "${CODEX_SESSION_ID:-}" \
          || "${CODEX_THREAD_ID}" == "${CODEX_SESSION_ID}" ]]; then
        SESSION_ID="${CODEX_THREAD_ID:-${CODEX_SESSION_ID:-}}"
    fi
elif [[ -n "${CLAUDE_PLUGIN_ROOT:-}" ]]; then
    HARNESS="claude"; SESSION_ID="${CLAUDE_CODE_SESSION_ID:-}"
else
    exit 0  # generic/unknown harness: nothing addressable to report
fi

# Resolve the fno-agents binary, most-local first (mirrors inside-leg-report.sh).
# shellcheck source=lib/agents-bin.sh
source "$HOOK_DIR/lib/agents-bin.sh"
BIN="$(fno_agents_bin "$REPO_ROOT")"

# ---------------------------------------------------------------------------
# Worker branch: one thin report; the daemon is the filter (an unknown row is
# dropped there, so this hook needs no "am I a grid pane?" gate).
if [[ -n "${FNO_AGENT_SELF:-}" ]]; then
    [[ -z "$BIN" ]] && exit 0
    # `report --kind session`: the SessionStart transport rides the existing
    # report action (the client action list is shrink-only).
    ARGS=(report --kind session --harness "$HARNESS" --agent-self "$FNO_AGENT_SELF")
    [[ -n "$SESSION_ID" ]] && ARGS+=(--session-id "$SESSION_ID")
    # Pane substrate only: the spawner writes the row AFTER `mux pane run`
    # returns, so a fast-booting child reports before its row exists. The
    # daemon polls for it under this flag; every other substrate has its row
    # already or never gets one.
    [[ "${FNO_AGENT_ROW_PENDING:-}" == "${FNO_AGENT_SELF:-}" ]] && ARGS+=(--wait-row)
    # The raw payload rides on stdin when the harness pipes one in; a tty
    # (a codex start reaching this script with the wrapper's terminal still
    # attached) must read /dev/null instead of hanging on the terminal.
    STDIN_SRC="/dev/null"; [[ ! -t 0 ]] && STDIN_SRC="/dev/stdin"
    with_timeout 2 "$BIN" "${ARGS[@]}" >/dev/null 2>&1 <"$STDIN_SRC" || true
    # Pane substrate ALSO keeps the bounded Python restamp: the daemon ingest
    # holds the id, but only that path heals the row's mux ref to this pane
    # and opens a parked pending graph row. Panes are the rare substrate;
    # every other lane stays on the thin verb alone.
    if [[ "${FNO_AGENT_ROW_PENDING:-}" == "${FNO_AGENT_SELF:-}" ]]; then
        PY_ARGS=(--harness "$HARNESS" --agent-self "$FNO_AGENT_SELF" --cwd "$REPO_ROOT")
        [[ -n "$SESSION_ID" ]] && PY_ARGS+=(--session-id "$SESSION_ID")
        cd "$REPO_ROOT" 2>/dev/null || true
        with_timeout 12 uv run --project "$CLI_DIR" \
            python3 -m fno.agents.register_session "${PY_ARGS[@]}" >/dev/null 2>&1 </dev/null || true
    fi
    exit 0
fi

# ---------------------------------------------------------------------------
# Every session leaves a record of the machine it began on, beside its
# transcript, whether or not it joins the roster. --origin-only writes that
# one file and sends nothing to the daemon. A tty stdin (a manual run) reads
# nothing instead of hanging.
PAYLOAD=""
[[ ! -t 0 ]] && PAYLOAD="$(cat)"
if [[ -n "$BIN" ]]; then
    ORIGIN_ARGS=(report --kind session --origin-only --harness "$HARNESS")
    [[ -n "$SESSION_ID" ]] && ORIGIN_ARGS+=(--session-id "$SESSION_ID")
    printf '%s' "$PAYLOAD" | with_timeout 2 "$BIN" "${ORIGIN_ARGS[@]}" >/dev/null 2>&1 || true
fi

# ---------------------------------------------------------------------------
# Operator branch: a hand-started session joins the roster only when asked.
# (config: agents.auto_register_sessions; /fno-me is the deliberate join.)
if [[ "$(fno config get agents.auto_register_sessions 2>/dev/null || true)" != "true" ]]; then
    exit 0
fi

# Nothing to register without a session id (the entry point also guards this).
[[ -n "$SESSION_ID" ]] || exit 0

cd "$REPO_ROOT" 2>/dev/null || true

ARGS=(--harness "$HARNESS" --session-id "$SESSION_ID" --cwd "$REPO_ROOT")

# Claude sends the SessionStart flavor (startup | resume | clear) in the
# hook-input JSON, consumed above: read the flavor from the captured payload
# instead of stdin, which the origin call already emptied.
if [[ "$HARNESS" == "claude" && -n "$PAYLOAD" ]]; then
    SOURCE="$(printf '%s' "$PAYLOAD" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("source",""))' 2>/dev/null || true)"
    [[ -n "$SOURCE" ]] && ARGS+=(--source "$SOURCE")
fi

uv run --project "$CLI_DIR" python3 -m fno.agents.register_session "${ARGS[@]}" 2>/dev/null || true

exit 0
