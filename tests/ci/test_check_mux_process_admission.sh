#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GATE="$REPO_ROOT/scripts/ci/check-mux-process-admission.sh"
FIXTURE="$(mktemp -d "${TMPDIR:-/tmp}/fno-admission-gate.XXXXXX")"
trap 'rm -rf "$FIXTURE"' EXIT

mkdir -p "$FIXTURE/crates/fno/src"
cat >"$FIXTURE/crates/fno/src/safe.rs" <<'EOF'
fn safe() {}
#[cfg(test)]
mod tests {
    fn test_only() {
        let _ = std::process::Command::new("fixture");
    }
}
EOF
cat >"$FIXTURE/crates/fno/src/bad.rs" <<'EOF'
fn bad() {
    let _ = std::process::Command::new("fixture").spawn();
}
EOF

if output=$(bash "$GATE" --root "$FIXTURE" 2>&1); then
    echo "$output"
    echo "expected the production bypass fixture to fail" >&2
    exit 1
fi
grep -q "bad.rs:2" <<<"$output"

rm "$FIXTURE/crates/fno/src/bad.rs"
output=$(bash "$GATE" --root "$FIXTURE")
grep -Eq 'process-admission coverage: inspected=[1-9][0-9]* bypasses=0' <<<"$output"

echo "[mux-process-admission] scanner tests passed"
