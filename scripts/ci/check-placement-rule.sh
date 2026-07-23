#!/usr/bin/env bash
# check-placement-rule.sh - CI gate enforcing the placement rule (ab-f063
# Wave 2): footnote's own accumulating state (logs, telemetry, corrections)
# lives ONLY under ~/.fno/, <project>/.fno/, or internal/<project>/ - never
# under .claude/ or ~/.claude/.
#
# Complements check-no-hardcoded-paths.sh (which bans bare $HOME/.fno and
# Path.home()/".fno" - i.e. paths that bypass the registry but still land in
# the RIGHT place). This script checks two different things:
#
#   (a) NEW `.claude/` or `~/.claude` path construction in cli/src, crates,
#       hooks, scripts. Run: bash scripts/ci/check-placement-rule.sh
#       Exits 0 when clean, 1 with a report when a non-allowlisted hit is
#       found.
#
#   (b) A `.fno/` write in hooks/*.sh built from a bare relative string
#       (`>> ".fno/x"`) instead of a $REPO_ROOT-anchored path or a registry
#       accessor - the exact cwd-anchoring bug class the Wave 2 audit found
#       (hooks firing with an unexpected cwd nest ~/.fno/.fno, ~/.fno/.claude,
#       etc). Scoped to hooks/ only: cli/src's CLI commands intentionally
#       default state args to a bare cwd-relative Path(".fno/...") (the
#       expected UX for a tool invoked from a repo root, like git), and
#       scripts/ contains slash-command setup scripts that only ever run
#       with cwd = the agent's project root. Neither is the "fires with an
#       unpredictable cwd" risk class this check targets.
#
# Allowlist rationale for (a)
# ----------------------------
# `.claude/` is legitimately touched, in code that is NOT a placement-rule
# violation, for three separate reasons - all pre-existing and out of this
# wave's scope:
#   1. Reading Claude Code's OWN files: ~/.claude/sessions, ~/.claude/projects
#      (transcripts), ~/.claude/jobs, .claude/settings.json /
#      settings.local.json (Claude Code's config, not footnote's state), and
#      scripts/save-session.py's read of the ~/.claude/.session-context.json
#      statusline sidecar (its OWN transcript writes were re-homed to
#      ${FNO_HOME}/sessions - see below - so only the sidecar READ remains).
#      setup/cli_hooks.py WRITES ~/.claude/settings.json for the same reason
#      its siblings write ~/.gemini/settings.json and ~/.codex/config.toml:
#      it wires a hook into the CLI's OWN config. `claude rm` runs with no
#      agent session and so never loads plugin hooks, which leaves the
#      settings file the only place a WorktreeRemove hook can reach it. That
#      is Claude Code config, not footnote state - nothing accumulates there.
#      This is a large, actively-developed surface (multi-provider agent
#      discovery) - allowlisted by file below rather than re-derived here.
#      The mux Connections UI (crates/fno/src/connections_view.rs) belongs
#      here too: its login-wizard default config dir `~/.claude-<id>` is a
#      per-account CLAUDE_CONFIG_DIR (a Claude Code config dir, not footnote
#      state), the same multi-account convention managed.py already uses.
#   2. The worktree-harness integration: `.claude/worktrees/<name>` is the
#      documented, SANCTIONED harness-native worktree default (see
#      .claude/rules/worktrees.md - "this is now allowed"), and
#      scripts/setup/setup-worktree.sh symlinks .claude/{agents,commands,
#      skills,settings.local.json,scheduled_tasks.*,...} from the canonical
#      checkout into a worktree per that same documented contract.
#   3. autocorrect's OWN remaining ~/.claude/ files that this wave
#      deliberately did NOT move (proposed-patches/, corrections-malformed.log,
#      the various watermark files, insights.md) - only corrections.log and
#      corrections-rejected.log were in scope (see skills/autocorrect/SKILL.md).
#
# Carveout cv-8dc3c6dc (RESOLVED): git-protection.py's state
# (git-protection.json + approve_no_verify.flag) and save-session.py's
# transcripts used to write under the harness state dir. Both were re-homed
# to ${FNO_HOME:-~/.fno} in the git-protection-hook re-home change:
#   - hooks/git-protection.py is now OFF this allowlist entirely - it writes
#     nothing under the harness dir and holds no reference to it, so CI
#     enforces the placement rule for it with no exception.
#   - scripts/save-session.py stays listed above under category (1): its
#     transcript WRITES moved to ${FNO_HOME}/sessions, and its only remaining
#     reference is the legitimate READ of Claude Code's own statusline sidecar.
#
# A file not on this list that starts referencing .claude/ must be a
# conscious addition: either it's another instance of (1)-(3) above (add it
# to the list here, in the same PR), or it's a genuine new footnote-state
# write that belongs under ~/.fno/ instead.

set -euo pipefail

REPO_ROOT=""
if git_root=$(git rev-parse --show-toplevel 2>/dev/null); then
    REPO_ROOT="$git_root"
fi
if [[ -z "$REPO_ROOT" ]]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    candidate="$SCRIPT_DIR"
    while [[ "$candidate" != "/" && "$candidate" != "." ]]; do
        if [[ -e "$candidate/.git" ]]; then
            REPO_ROOT="$candidate"
            break
        fi
        candidate="$(dirname "$candidate")"
    done
fi
if [[ -z "$REPO_ROOT" ]]; then
    echo "ERROR: could not resolve repo root" >&2
    exit 2
fi
cd "$REPO_ROOT"

VIOLATIONS=0
REPORT=""

add_violation() {
    local heading="$1"
    local hits="$2"
    if [[ -n "$hits" ]]; then
        local count
        count=$(echo "$hits" | wc -l | tr -d ' ')
        VIOLATIONS=$((VIOLATIONS + count))
        REPORT+=$'\n'"$heading"$'\n'"$hits"$'\n'
    fi
}

# ---------------------------------------------------------------------------
# (a) .claude/ path construction, allowlisted by file (relative to repo root)
# ---------------------------------------------------------------------------

CLAUDE_ALLOWLIST=$(cat <<'EOF'
cli/src/fno/adapters/__init__.py
cli/src/fno/adapters/_shared.py
cli/src/fno/adapters/providers/dispatch.py
cli/src/fno/adapters/providers/managed.py
cli/src/fno/adapters/providers/staging.py
cli/src/fno/adapters/providers/test_cli.py
cli/src/fno/adapters/providers/test_dispatch.py
cli/src/fno/adapters/providers/test_failover.py
cli/src/fno/adapters/providers/test_loader.py
cli/src/fno/adapters/providers/test_model.py
cli/src/fno/adapters/providers/test_rotation.py
cli/src/fno/adapters/providers/test_staging.py
cli/src/fno/adapters/test_claude_code.py
cli/src/fno/adapters/test_init.py
cli/src/fno/adapters/test_shared.py
cli/src/fno/agents/account_env.py
cli/src/fno/agents/cli.py
cli/src/fno/agents/discover.py
cli/src/fno/agents/dispatch.py
cli/src/fno/agents/format.py
cli/src/fno/agents/providers/_claude_session_registry.py
cli/src/fno/agents/providers/claude.py
cli/src/fno/agents/read.py
cli/src/fno/agents/registry.py
cli/src/fno/agents/rust_runtime.py
cli/src/fno/agents/spawn_gate.py
cli/src/fno/agents/test_account_env.py
cli/src/fno/agents/whoami.py
cli/src/fno/backlog/advance.py
cli/src/fno/backlog/batch.py
cli/src/fno/claims/session_pid.py
cli/src/fno/cost/_register.py
cli/src/fno/cost/_session_cost.py
cli/src/fno/cost/cost_tracker.py
cli/src/fno/doctor.py
cli/src/fno/graph/cli.py
cli/src/fno/graph/maintain.py
cli/src/fno/inbox/drain.py
cli/src/fno/observer/isolation.py
cli/src/fno/paths.py
cli/src/fno/provenance/resolver.py
cli/src/fno/recovery.py
cli/src/fno/relay/daemon.py
cli/src/fno/relay/registry.py
cli/src/fno/relay/roundtrip.py
cli/src/fno/review/confidence_scorer.py
cli/src/fno/review/runners/agents_spawn_runner.py
cli/src/fno/review/runners/claude_runner.py
cli/src/fno/review/scorers/__init__.py
cli/src/fno/runtime/cli.py
cli/src/fno/runtime/probe.py
cli/src/fno/runtime/worktree.py
cli/src/fno/scoreboard/fold.py
cli/src/fno/setup_cli.py
cli/src/fno/setup/cli_hooks.py
cli/src/fno/setup/integration.py
cli/src/fno/setup/recommended_rules.py
cli/src/fno/setup/test_recommended_rules.py
cli/src/fno/target_cli.py
cli/src/fno/test_sigma_dispatch.py
cli/src/fno/test_worktree_paths.py
cli/src/fno/update.py
cli/src/fno/wake/detect.py
cli/src/fno/worker/review.py
cli/src/fno/worktree_cli/cli.py
cli/src/fno/worktree_paths.py
cli/src/fno/worktree.py
crates/fno-agents/src/claude_adopt.rs
crates/fno-agents/src/claude_ask.rs
crates/fno-agents/src/claude_drive.rs
crates/fno-agents/src/claude_roster.rs
crates/fno-agents/src/client_verbs.rs
crates/fno-agents/src/daemon.rs
crates/fno-agents/src/finalize.rs
crates/fno-agents/src/provider.rs
crates/fno-agents/src/state.rs
crates/fno-agents/src/stream_worker.rs
crates/fno-agents/src/bin/client.rs
crates/fno-agents/tests/claude_ask_dispatch.rs
crates/fno-agents/tests/claude_ask_parity.rs
crates/fno/src/agents_view.rs
crates/fno/src/connections_view.rs
hooks/cache-keepalive-inject.sh
hooks/corrections-git-postcommit.sh
hooks/session-start.sh
hooks/worktree-setup.sh
hooks/helpers/check-impl-location.sh
scripts/autocorrect-pack.sh
scripts/autocorrect-review.sh
scripts/autocorrect-triage.sh
scripts/autocorrect-watcher.sh
scripts/ci/check-no-internal-refs.sh
scripts/ci/check-no-stale-skill-refs.sh
scripts/ci/check-placement-rule.sh
scripts/corrections-insights-tag.sh
scripts/corrections-log-init.sh
scripts/corrections-migrate-to-fno.sh
scripts/ensure-global-dir.sh
scripts/install-autocorrect-cron.sh
scripts/install-corrections-git-hook.sh
scripts/lib/config.sh
scripts/lib/corrections-lock.sh
scripts/lib/mission-emit.sh
scripts/lib/worktree-lifecycle.sh
scripts/lib/worktree-manager.sh
scripts/lint/no-invalid-events.sh
scripts/metrics/register-session-cost.sh
scripts/migrate-events-shape.py
scripts/diagnostics/token-diagnose.py
scripts/rename/rename-to-fno.sh
scripts/setup/setup-worktree.sh
scripts/setup/worktree-create-hook.sh
scripts/worktree-lifecycle.sh
scripts/save-session.py
EOF
)

# scripts/tests/ (sandboxed harnesses that legitimately reference .claude/ as
# fixture text, e.g. this lint's own test) and scripts/ci/ (this script and
# its siblings) are excluded wholesale, mirroring check-no-hardcoded-paths.sh's
# own convention - never scanned, not even via the allowlist above.
CLAUDE_HITS=$(
    grep -rnE '\.claude([/"'"'"']|$)' \
        cli/src crates hooks scripts \
        --include='*.py' --include='*.rs' --include='*.sh' \
        --exclude-dir=tests --exclude-dir=ci \
        2>/dev/null \
    | grep -v -F -f <(printf '%s\n' "$CLAUDE_ALLOWLIST") \
    || true
)
add_violation "New .claude/ or ~/.claude path construction (see allowlist rationale at the top of this script):" "$CLAUDE_HITS"

# ---------------------------------------------------------------------------
# (b) hooks/*.sh: a .fno/ write built from a bare relative string, not
# anchored to $REPO_ROOT / a registry accessor. Matches an append (>>) or
# clobber (>, not 2>) redirect targeting a literal ".fno/..." path.
# ---------------------------------------------------------------------------

HOOKS_FNO_WRITE_HITS=$(
    grep -rnE '(>>|[^0-9]>) *"?\.fno/' \
        hooks/*.sh \
        2>/dev/null \
    || true
)
add_violation "hooks/*.sh: .fno/ write using a bare cwd-relative string (anchor to \$REPO_ROOT or a registry accessor instead):" "$HOOKS_FNO_WRITE_HITS"

# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------

if [[ $VIOLATIONS -eq 0 ]]; then
    echo "check-placement-rule: no violations found"
    exit 0
fi

{
    echo "check-placement-rule: $VIOLATIONS violation(s) found"
    echo "$REPORT"
    echo
    echo "See the allowlist rationale at the top of scripts/ci/check-placement-rule.sh."
} >&2
exit 1
