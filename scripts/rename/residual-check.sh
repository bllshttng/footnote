#!/usr/bin/env bash
# residual-check.sh -- permanent zero-tolerance gate for legacy `abilities`/`abi`
# branding. The one-release back-compat window is closed: no compat shims, no
# migration exemptions. Any structural old-name pattern fails the build.
#
# This file itself names the forbidden patterns (that is what a guard does), so
# it is self-exempt; third-party lockfiles (`abi3` wheel tags, the `hermit-abi`
# crate) are unrelated packaging vocabulary and are exempt too.
#
# Exit 0 = clean; exit 1 = residual found (prints offenders); exit 2 = env error.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)" || { echo "residual-check: not a git repo" >&2; exit 2; }
cd "$ROOT"

# The only allowed mentions: this guard's own pattern list, third-party
# packaging tokens in lockfiles (abi3 wheel tags, hermit-abi crate), and the
# loc-ratchet trajectory - an append-only audit ledger whose past entries record
# the original abilities->fno rename verbatim. Rewriting those reasons would
# falsify history AND break the ratchet checker, which keys each entry's identity
# on its reason (a modification reads as a removal). Old names there are history.
KEEP_FILES=(
  ':!scripts/rename/**'
  ':!*.lock'
  ':!**/*.lock'
  ':!scripts/ci/loc-ratchet-trajectory.yaml'
)

# pattern  label  [extra-pathspec ...]
PATTERNS=(
  '\b(from|import) abilities\b|python import|'
  'python[0-9]? -m abilities\b|python -m abilities module|'
  '\b(uv run|--package|--project) abilities\b|abilities command/selector handle|'
  '\babilities\.(?!(?:sh|py|md|json|ya?ml|txt|toml|rs|lock|jsonl|cfg|ini|template|example|bak)\b)[a-z_]|module qualifier abilities.|'
  '(?<![\w./])abilities/[a-z_]+\.py|code-path abilities/<file>.py|'
  'abi-agents|rust crate (hyphen)|'
  'abi_agents|rust lib (underscore)|'
  '\bABILITIES_|env var ABILITIES_|'
  '\bABI_|env var ABI_|'
  '/abilities:|skill namespace|'
  '\.abilities\b|state dir|'
  'src/abilities|python package path|'
  '(?<![\w-])abi(?![\w-])|bare command abi|:!cli/benchmarks/**'
)

fail=0
for spec in "${PATTERNS[@]}"; do
  IFS='|' read -r pat label extra <<<"$spec"
  # Collect matches, dropping any line that opts out via the keep marker.
  hits="$(git grep -nP -e "$pat" -- "${KEEP_FILES[@]}" ${extra:+"$extra"} 2>/dev/null | grep -v 'fno-rename-keep' || true)"
  if [[ -n "$hits" ]]; then
    n="$(printf '%s\n' "$hits" | grep -c . || true)"
    printf '\n[FAIL] residual "%s" (%s): %s occurrence(s)\n' "$label" "$pat" "$n"
    printf '%s\n' "$hits" | head -20
    fail=1
  fi
done

if [[ $fail -ne 0 ]]; then
  cat >&2 <<'EOF'

A legacy abilities/abi name reached the tree. Rename it to the fno equivalent.
There is no back-compat exemption: the migration window is closed.
EOF
  exit 1
fi

echo "residual-check: clean -- no structural old-name patterns remain."
