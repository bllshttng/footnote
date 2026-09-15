#!/usr/bin/env bash
# check-tracker-consumers.sh - gate the graph-consumer census (task 5.1).
#
# Runs both census modalities plus the self-test. Every direct read_graph
# consumer (Python) and every direct graph.json open (Rust, production
# sources) must attribute to a named class; every backlog verb must carry
# exactly one classification marker; the self-test must prove the detectors
# detect before this gate reports clean. Real exit codes throughout - the
# caller must not pipe this into a truncating command.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TOOL="$ROOT/scripts/diagnostics/tracker-consumers.py"

# The census imports the fno package (the live registry), so it runs under
# the cli project's interpreter; bare python3 has no typer/pydantic in a
# hermetic environment.
PY=(uv run --project "$ROOT/cli" python3)

"${PY[@]}" "$TOOL" --self-test | grep -q "self-test OK" || {
    echo "check-tracker-consumers: self-test did not pass" >&2
    exit 1
}
"${PY[@]}" "$TOOL" --verbs >/dev/null
"${PY[@]}" "$TOOL" --reads >/dev/null

# Writer ratchet: only the publish path (graph/store.py) and the archive
# leg (graph/cli.py) may call the store's atomic write. A new call site is
# a writer that would keep writing a file nobody reads after the flip.
# `|| true`: a clean tree exits the last grep 1 (no selection), and this
# script runs under `set -euo pipefail`.
offenders=$(grep -rEn '\b_write_json\(|write_atomic\(' --include='*.py' "$ROOT/cli/src/fno" \
    | grep -v '_write_atomic(' \
    | cut -d: -f1 | sort -u \
    | grep -vE '/graph/(store|cli)\.py$' \
    | sed "s|$ROOT/cli/src/fno/||" || true)
if [[ -n "$offenders" ]]; then
    echo "check-tracker-consumers: direct graph-store writers outside the publish path:" >&2
    echo "$offenders" | sed 's/^/  /' >&2
    exit 1
fi

echo "check-tracker-consumers: OK - verbs classified, reads attributed, self-test green"
