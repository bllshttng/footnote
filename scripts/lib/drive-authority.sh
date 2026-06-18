#!/usr/bin/env bash
# scripts/lib/drive-authority.sh
# Operator-authority enforcement seam (Phase 6 Wave 8, cv-b36e32b1).
#
# When an operator holds an interactive/step/paranoid drive window on any agent
# (design LD3/LD29), the bytes flowing into that agent's PTY were authored by
# the operator, not the LLM. Gate signals the stop hook observes during that
# window -- most importantly a <promise> completion tag -- are therefore
# operator-initiated and MUST NOT be honored as LLM authorship.
#
# Detection is delegated to the shipped primitive `fno agents drive-authority`
# (Wave 4, ab-8d258ddb): `--json` reports every open authority window
# (interactive/step/paranoid; "watch" is read-only and excluded), reading each
# agent's daemon-owned state.json. This module is the bash consumption seam the
# Python-layer stop hook + graph-write-protect hook call.
#
# ── Session scoping by per-agent identity (cv-140f09c3) ─────────────────────
# `fno agents drive-authority` reports windows MACHINE-WIDE: every agent under
# the shared `~/.fno/agents/` store, across every project and provider.
# The original seam returned "active" on the mere existence of any window, so an
# operator driving an UNRELATED agent -- even one in a different project -- hung
# a finished target session whose <promise> the stop hook then refused.
#
# Only an operator driving the agent whose PTY runs THIS session can type a
# <promise> into THIS transcript. We scope to that agent by IDENTITY, not by
# cwd: the PTY worker stamps the agent's short_id into the child's environment
# as FNO_AGENTS_SELF_SHORT_ID (crates/fno-agents/src/worker.rs), and the child
# (claude/codex) plus any Stop / graph-write-protect hook it spawns inherit it.
# The guard fires only when an open authority window targets that same short_id.
#
# Identity beats cwd on both correctness fronts that scoped this before:
#   - `fno agents` allows multiple named agents in one cwd, so a terminal
#     `/target` or a second worker sharing a repo would be over-blocked by a
#     cwd match (codex P2 #1 on PR #394). short_id is unique per agent.
#   - There is no registry read, so a `config.state_dir` / FNO_AGENTS_HOME
#     override can no longer leave windows unresolved -> fail-open (codex P2 #2).
#
# A plain (non-daemon) terminal session has no FNO_AGENTS_SELF_SHORT_ID, so no
# PTY of ours is drivable -> it is never blocked.
#
# Fail-open: if `fno`/`jq` is absent or the env var is unset, treat it as "no
# authority window." If FNO_AGENTS_SELF_SHORT_ID does not reach the hook (e.g.
# the harness scrubs env), the guard never blocks -- safe for "no hang," at the
# cost of not refusing an operator-typed promise on a genuinely-driven session.

# drive_authority_active
#   rc 0  an operator authority window is open ON THIS SESSION'S AGENT
#         (a window whose short_id == $FNO_AGENTS_SELF_SHORT_ID)
#   rc 1  no such window, OR self-identity unknown / fno|jq absent (fail-open)
#
# Read-only. No stdout. The caller branches on the exit code:
#   if drive_authority_active; then <refuse the gate signal>; fi
drive_authority_active() {
    command -v fno >/dev/null 2>&1 || return 1
    command -v jq  >/dev/null 2>&1 || return 1

    # Scope to THIS session by agent identity. A session with no stamped
    # short_id (a plain terminal) is never drivable -> never blocked.
    local self
    self="${FNO_AGENTS_SELF_SHORT_ID:-}"
    [[ -n "$self" ]] || return 1

    # Active iff an open authority window targets MY short_id. Guard with
    # `type == "object"` so a non-object element in .sessions (unexpected API
    # response / future schema drift) cannot make jq throw and exit nonzero
    # (gemini review on PR #396); a parse error here would read as "active",
    # the wrong direction. Filtering keeps the fail-open contract intact.
    fno agents drive-authority --json 2>/dev/null \
        | jq -e --arg me "$self" '.sessions[]? | select(type == "object" and .short_id == $me)' >/dev/null 2>&1
}
