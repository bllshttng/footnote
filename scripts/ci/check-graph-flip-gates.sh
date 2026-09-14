#!/usr/bin/env bash
# check-graph-flip-gates.sh - the source-tree half of the sqlite flip gate
# (`fno doctor graph backend sqlite`), re-run at flip time: the reader
# census, the writer ratchet, the table-ownership test, and the parity
# negative control. The soak evidence itself is keeper knowledge (the
# backend_gate op reads the sampler's own journal), so this script owns
# only the tree checks. Each failure prints one `flip-gate: FAIL: <gap>`
# line; exit 1 when any gate fails.
#
# Not a standalone CI gate: the flip verb is its only caller.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
gaps=()

PY=(uv run --project "$ROOT/cli" python3)

# Reader census: every read_graph consumer attributed (the wave 8 grep).
if ! "${PY[@]}" "$ROOT/scripts/diagnostics/tracker-consumers.py" --self-test >/dev/null 2>&1; then
    gaps+=("reader census self-test failed")
fi
if ! "${PY[@]}" "$ROOT/scripts/diagnostics/tracker-consumers.py" --verbs >/dev/null 2>&1; then
    gaps+=("reader census verbs failed")
fi
if ! "${PY[@]}" "$ROOT/scripts/diagnostics/tracker-consumers.py" --reads >/dev/null 2>&1; then
    gaps+=("reader census reads failed")
fi

# Writer ratchet: only the publish path (graph/store.py) and the archive
# leg (graph/cli.py) may call the store's atomic write. A new call site is
# a writer that would keep writing a file nobody reads after the flip.
offenders=$(grep -rEn '\b_write_json\(|write_atomic\(' --include='*.py' "$ROOT/cli/src/fno" \
    | grep -v '_write_atomic(' \
    | cut -d: -f1 | sort -u \
    | grep -vE '/graph/(store|cli)\.py$' \
    | sed "s|$ROOT/cli/src/fno/||")
if [[ -n "$offenders" ]]; then
    gaps+=("direct graph-store writers outside the publish path: $(echo "$offenders" | tr '\n' ' ' | sed 's/ $//')")
fi

# Table ownership (ruling 4): a write to an owned table outside its
# owner's file fails naming file and line.
if ! (cd "$ROOT/crates/fno-agents" && cargo test --lib table_ownership -q >/dev/null 2>&1); then
    gaps+=("table ownership test failed")
fi

# Negative control (AC2-HP): copies of the live pair must compare clean,
# and one mutated copy must diverge naming the id.
if ! (cd "$ROOT/cli" && uv run python -m fno.graph.parity --negative-control >/dev/null 2>&1); then
    gaps+=("negative control failed")
fi

for gap in "${gaps[@]}"; do
    echo "flip-gate: FAIL: $gap"
done
[[ ${#gaps[@]} -eq 0 ]]
