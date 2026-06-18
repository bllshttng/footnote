#!/usr/bin/env bash
# scripts/ci/check-config-schema-drift.sh
#
# Config-schema anti-drift gate. Mirrors `fno bundle check`:
# the Pydantic SettingsModel is the single source of truth, and the generated
# docs/configuration-guide.md MUST match the generator. This fails CI the
# moment the committed reference drifts from the model + registry.
#
# The deeper guards (registry completeness, wizard-key existence,
# bash-default equality) live in cli/tests/test_config_schema_drift.py and run
# in the normal pytest step; this shell gate is the explicit docs-freshness
# surface so a reviewer sees a named red check on drift.
#
# Fail-closed: any error resolving fno / the repo is a non-zero exit.
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$REPO_ROOT/cli"

# Prefer the in-tree build (`uv run`) over an installed snapshot: a stale
# global `fno` may predate the `config schema` verb (install staleness).
if command -v uv >/dev/null 2>&1; then
    FNO=(uv run fno)
elif command -v fno >/dev/null 2>&1; then
    FNO=(fno)
else
    echo "check-config-schema-drift: neither 'uv' nor 'fno' available" >&2
    exit 2
fi

"${FNO[@]}" config schema --markdown --check
