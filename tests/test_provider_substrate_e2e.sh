#!/usr/bin/env bash
# test_provider_substrate_e2e.sh
#
# End-to-end integration test for the provider rotation substrate (ab-256f6b6e).
# Exercises: add -> list -> stage -> use -> dispatch_env -> cost.update -> remove
#
# All operations run against isolated TEST_HOME and TEST_REPO so the user's
# real ~/.fno/ and project directories are never touched.
#
# Phase 05, AC05.1-HP

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CLI_DIR="$REPO_ROOT/cli"

# ---------------------------------------------------------------------------
# Sandbox setup
# ---------------------------------------------------------------------------
TEST_HOME=$(mktemp -d -t provider-e2e-home.XXXXXX)
TEST_REPO=$(mktemp -d -t provider-e2e-repo.XXXXXX)

# Cleanup on any exit (success or failure)
trap 'rm -rf "$TEST_HOME" "$TEST_REPO"' EXIT

# Override HOME so no real ~/.fno/ is touched.
export HOME="$TEST_HOME"

# Create a fake secondary credentials directory (mimics ~/.claude.secondary)
SECONDARY_CREDS="$TEST_HOME/.claude.secondary"
mkdir -p "$SECONDARY_CREDS"
echo '{"oauth_token": "fake-secondary-token"}' > "$SECONDARY_CREDS/.credentials.json"

# Initialise a minimal git repo so project-scoped settings.yaml lands under
# TEST_REPO/.fno/
cd "$TEST_REPO"
git init -q
mkdir -p .fno

echo "PASS: step 0 - sandbox initialised (HOME=$TEST_HOME, REPO=$TEST_REPO)"

# ---------------------------------------------------------------------------
# Helper: run fno via uv from CLI_DIR, with CWD set to TEST_REPO so project-
# scoped settings.yaml resolves correctly.
# ---------------------------------------------------------------------------
fno() {
    (cd "$TEST_REPO" && uv --project "$CLI_DIR" run --package fno fno "$@")
}

# ---------------------------------------------------------------------------
# Helper: run a Python one-liner via uv with env vars exported as positional
# args (avoids heredoc quoting issues with single-quoted <<'PYEOF' blocks).
# ---------------------------------------------------------------------------
run_py() {
    # Usage: run_py "<python code string>"
    CLI_DIR="$CLI_DIR" \
    TEST_REPO="$TEST_REPO" \
    TEST_HOME="$TEST_HOME" \
    SECONDARY_CREDS="$SECONDARY_CREDS" \
        uv --project "$CLI_DIR" run python3 -c "$1"
}

# ---------------------------------------------------------------------------
# Step 1: fno providers add
# ---------------------------------------------------------------------------
fno providers add claude-max-secondary \
    --cli claude --auth oauth_dir \
    --credentials-source "$SECONDARY_CREDS" \
    --scope project

echo "PASS: step 1 - fno providers add succeeded"

# ---------------------------------------------------------------------------
# Step 2: fno providers list shows the record
# ---------------------------------------------------------------------------
LIST_OUT=$(fno providers list)
if ! echo "$LIST_OUT" | grep -q "claude-max-secondary"; then
    echo "FAIL: step 2 - claude-max-secondary not in providers list"
    echo "  Output: $LIST_OUT"
    exit 1
fi
echo "PASS: step 2 - fno providers list shows claude-max-secondary"

# ---------------------------------------------------------------------------
# Step 3: stage the provider explicitly.
# Phase 02's 'add' has a TODO(phase-03) marker - staging is NOT wired into
# 'add'. Call staging.stage() directly via Python.
# ---------------------------------------------------------------------------
PROVIDERS_ROOT="$TEST_HOME/.fno/providers"

STAGE_SCRIPT="
import sys, os
from pathlib import Path
sys.path.insert(0, str(Path(os.environ['CLI_DIR']) / 'src'))
from fno.adapters.providers.loader import load_providers
from fno.adapters.providers.staging import stage

root = Path(os.environ['TEST_HOME']) / '.fno' / 'providers'
cfg = load_providers(repo_root=Path(os.environ['TEST_REPO']))
record = cfg.by_id.get('claude-max-secondary')
if record is None:
    print('ERROR: claude-max-secondary not found in config', file=sys.stderr)
    sys.exit(1)
staged_path = stage(record, root=root)
print('staged:', staged_path)
"
run_py "$STAGE_SCRIPT"

EXPECTED_LINK="$PROVIDERS_ROOT/claude-max-secondary/.claude"
if [[ ! -L "$EXPECTED_LINK" ]]; then
    echo "FAIL: step 3 - symlink not created at $EXPECTED_LINK"
    exit 1
fi
echo "PASS: step 3 - staged (symlink at $EXPECTED_LINK -> $(readlink "$EXPECTED_LINK"))"

# ---------------------------------------------------------------------------
# Step 4: fno providers use
# ---------------------------------------------------------------------------
fno providers use claude-max-secondary --scope project

SHOW_OUT=$(fno providers show claude-max-secondary)
if ! echo "$SHOW_OUT" | grep -q "active:.*yes"; then
    echo "FAIL: step 4 - claude-max-secondary not marked active after 'use'"
    echo "  show output: $SHOW_OUT"
    exit 1
fi
echo "PASS: step 4 - fno providers use set claude-max-secondary as active"

# ---------------------------------------------------------------------------
# Step 5: dispatch_env returns the right env dict
# dispatch_env for claude+oauth_dir should produce CLAUDE_CONFIG_DIR
# ---------------------------------------------------------------------------
DISPATCH_SCRIPT="
import sys, os, json
from pathlib import Path
sys.path.insert(0, str(Path(os.environ['CLI_DIR']) / 'src'))
from fno.adapters.providers.dispatch import dispatch_env

root = Path(os.environ['TEST_HOME']) / '.fno' / 'providers'
env = dispatch_env(
    'claude-max-secondary',
    repo_root=Path(os.environ['TEST_REPO']),
    root=root,
)
print(json.dumps(env))
"
ENV_JSON=$(run_py "$DISPATCH_SCRIPT")

if ! echo "$ENV_JSON" | python3 -c "
import json, sys
d = json.load(sys.stdin)
assert 'CLAUDE_CONFIG_DIR' in d, f'missing CLAUDE_CONFIG_DIR in {d}'
"; then
    echo "FAIL: step 5 - dispatch_env did not return CLAUDE_CONFIG_DIR"
    echo "  ENV_JSON: $ENV_JSON"
    exit 1
fi
echo "PASS: step 5 - dispatch_env returned CLAUDE_CONFIG_DIR for oauth_dir provider"

# ---------------------------------------------------------------------------
# Step 6: cost.update with provider attribution lands in ledger
# ---------------------------------------------------------------------------
LEDGER_PATH="$TEST_HOME/.fno/ledger.json"

COST_SCRIPT="
import sys, os
from pathlib import Path
sys.path.insert(0, str(Path(os.environ['CLI_DIR']) / 'src'))
from fno.cost import update

update(
    'test-session-e2e',
    100,
    0.01,
    provider_id='claude-max-secondary',
    account_id='account-secondary',
    ledger_path=Path(os.environ['LEDGER_PATH']),
)
"
LEDGER_PATH="$LEDGER_PATH" run_py "$COST_SCRIPT"

if [[ ! -f "$LEDGER_PATH" ]]; then
    echo "FAIL: step 6 - ledger.json not written at $LEDGER_PATH"
    exit 1
fi

# Ledger is {"entries": [...]} (canonical dict shape written by cost.py post-fix).
# Fall back to bare-list shape for back-compat with pre-fix on-disk ledgers.
PROVIDER_IN_LEDGER=$(jq -r 'if type == "array" then .[-1].provider_id else .entries[-1].provider_id end' "$LEDGER_PATH")
ACCOUNT_IN_LEDGER=$(jq -r 'if type == "array" then .[-1].account_id else .entries[-1].account_id end' "$LEDGER_PATH")

if [[ "$PROVIDER_IN_LEDGER" != "claude-max-secondary" ]]; then
    echo "FAIL: step 6 - ledger[-1].provider_id = '$PROVIDER_IN_LEDGER' (expected claude-max-secondary)"
    exit 1
fi
if [[ "$ACCOUNT_IN_LEDGER" != "account-secondary" ]]; then
    echo "FAIL: step 6 - ledger[-1].account_id = '$ACCOUNT_IN_LEDGER' (expected account-secondary)"
    exit 1
fi
echo "PASS: step 6 - cost.update wrote provider_id + account_id to ledger"

# ---------------------------------------------------------------------------
# Step 7: fno providers remove
# ---------------------------------------------------------------------------
fno providers remove claude-max-secondary --force --scope project

REMOVE_LIST=$(fno providers list)
if echo "$REMOVE_LIST" | grep -q "claude-max-secondary"; then
    echo "FAIL: step 7 - claude-max-secondary still in list after remove"
    echo "  list output: $REMOVE_LIST"
    exit 1
fi
echo "PASS: step 7 - fno providers remove succeeded"

# ---------------------------------------------------------------------------
# Done (trap handles cleanup of TEST_HOME and TEST_REPO)
# ---------------------------------------------------------------------------
echo ""
echo "PASS: provider substrate e2e - all 7 assertions passed"
