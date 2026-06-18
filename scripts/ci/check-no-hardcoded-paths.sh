#!/usr/bin/env bash
# check-no-hardcoded-paths.sh - CI gate that blocks new hardcoded ~/.fno
# path literals from landing in the abilities codebase.
#
# After Phase 03 of the path-config migration (plan 2026-05-14-path-config-impl),
# all path resolution must go through:
#   Python : from fno import paths  (paths.state_dir(), paths.graph_json(), ...)
#   Bash   : source "$(fno paths shell-stub)"  then use $STATE_DIR, $GRAPH_JSON_PATH, etc.
#
# Run: bash scripts/ci/check-no-hardcoded-paths.sh
# Exits 0 when no violations found; exits 1 with a report when violations detected.
#
# Allowlist rationale
# -------------------
# Python (entire-file exclusions):
#   - fno/paths.py, paths_cli.py, paths_verify.py - canonical accessor defs
#   - fno/config/__init__.py    - bootstrap fallback (load before paths available)
#   - fno/setup/emit_shell.py   - generates paths.sh from schema
#   - fno/setup/migrate_paths.py - migration command
#   - fno/update.py             - has try/except fallback for source-path cache
#   - fno/megawalk_drivers/fallback.py - has try/except fallback for settings
#   - fno/adapters/providers/dispatch.py - has try/except fallback for providers
#   - fno/adapters/providers/staging.py  - has try/except fallback for providers
#   - graph/_constants.py             - uses _state_dir() helper with try/except
#   - cost/_register.py, cost/_session_cost.py - the moved standalone metric
#       scripts (ab-58645f63: former scripts/metrics/register-task.py +
#       session-cost.py). They keep their home-anchored ledger literal
#       (~/.fno/ledger.json), which is exactly what paths.ledger_json() defaults
#       to; the move-not-rewrite preserved them verbatim (no logic change).
#   - test_*.py                       - sandboxed tests
#   - scripts/metrics/*.py            - standalone analysis scripts (not in CLI)
#   - scripts/discovery-brief.py      - standalone script with docstring reference
#   - scripts/orchestrator.py / operator/ - standalone scripts
#
# Bash (entire-file or pattern exclusions):
#   - scripts/lib/paths.sh            - the generated stub
#   - scripts/tests/                  - harnesses redirect HOME=$TMP before use
#   - scripts/ci/                     - this script itself
#   - Lines using ${VAR:-$HOME/.fno/...} - already-migrated fallbacks
#   - Comment lines (# ...) and docstring lines (Python triple-quote strings)

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
# Python: Path.home() / ".fno" in CLI source files
#
# Files excluded (known legitimate fallbacks or bootstrap paths):
#   paths.py, paths_cli.py, paths_verify.py, config/__init__.py,
#   setup/emit_shell.py, setup/migrate_paths.py,
#   update.py (try/except), megawalk_drivers/fallback.py (try/except),
#   adapters/providers/dispatch.py (try/except), adapters/providers/staging.py (try/except),
#   adapters/providers/loader.py (bootstrap: settings loader self-reference)
#   sigma_dispatch.py (bootstrap: settings loader self-reference)
#   cost/_register.py, cost/_session_cost.py (moved standalone metric scripts)
#   graph/_constants.py (uses _state_dir() helper), test_*.py
# ---------------------------------------------------------------------------

PY_HITS=$(
    grep -rn 'Path\.home() / "\.fno"' \
        "$REPO_ROOT/cli/src/fno/" \
        --include='*.py' \
        --exclude='paths.py' \
        --exclude='paths_cli.py' \
        --exclude='paths_verify.py' \
        --exclude='test_*.py' \
        2>/dev/null \
    | grep -v 'setup/emit_shell\.py' \
    | grep -v 'setup/migrate_paths\.py' \
    | grep -v 'config/__init__\.py' \
    | grep -v 'graph/_constants\.py' \
    | grep -v 'update\.py' \
    | grep -v 'megawalk_drivers/fallback\.py' \
    | grep -v 'adapters/providers/dispatch\.py' \
    | grep -v 'adapters/providers/staging\.py' \
    | grep -v 'adapters/providers/loader\.py' \
    | grep -v 'sigma_dispatch\.py' \
    | grep -v 'cost/_register\.py' \
    | grep -v 'cost/_session_cost\.py' \
    || true
)
add_violation "Python bare Path.home() / \".fno\" violations in cli/src/fno/:" "$PY_HITS"

# ---------------------------------------------------------------------------
# Bash: bare $HOME/.fno/ WITHOUT ${VAR:-...} fallback wrapper
# Scopes: hooks/, skills/ (not scripts/tests/, not paths.sh, not this script)
# Excludes: comment lines, lines with ${VAR:-$HOME/.fno} pattern
# ---------------------------------------------------------------------------

HOOKS_SH_HITS=$(
    grep -rn '\$HOME/\.fno/' \
        "$REPO_ROOT/hooks/" \
        --include='*.sh' \
        2>/dev/null \
    | grep -v ':-.*\$HOME/\.fno' \
    | grep -v '^\s*#' \
    || true
)
add_violation "hooks/ bare \$HOME/.fno/ violations:" "$HOOKS_SH_HITS"

SKILLS_SH_HITS=$(
    grep -rn '\$HOME/\.fno/' \
        "$REPO_ROOT/skills/" \
        --include='*.sh' \
        2>/dev/null \
    | grep -v ':-.*\$HOME/\.fno' \
    | grep -v '^\s*#' \
    || true
)
add_violation "skills/ bare \$HOME/.fno/ violations:" "$SKILLS_SH_HITS"

# scripts/ directory: exclude scripts/tests/ (sandboxed) and scripts/ci/ (this script)
# also exclude scripts/lib/paths.sh
SCRIPTS_SH_HITS=$(
    grep -rn '\$HOME/\.fno/' \
        "$REPO_ROOT/scripts/" \
        --include='*.sh' \
        2>/dev/null \
    | grep -v 'scripts/tests/' \
    | grep -v 'scripts/ci/' \
    | grep -v 'scripts/lib/paths\.sh' \
    | grep -v ':-.*\$HOME/\.fno' \
    | grep -v '^\s*#' \
    || true
)
add_violation "scripts/ bare \$HOME/.fno/ violations (excluding tests/ and ci/):" "$SCRIPTS_SH_HITS"

# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------

if [[ $VIOLATIONS -eq 0 ]]; then
    echo "check-no-hardcoded-paths: no violations found"
    exit 0
fi

{
    echo "check-no-hardcoded-paths: $VIOLATIONS violation(s) found"
    echo "$REPORT"
    echo
    echo "To fix: route paths through 'from fno import paths' (Python)"
    echo "        or source \"\$(fno paths shell-stub)\" + \${STATE_DIR} (Bash)."
    echo "See: scripts/lib/paths.sh, cli/src/fno/paths.py"
} >&2
exit 1
