#!/usr/bin/env bash
# residual-check.sh -- CI guard for the abilities->fno rename (AC4-FR).
#
# Fails the build if any STRUCTURAL (technical) old-name pattern survives the
# sweep. This is the "no half-rename ships" gate. It checks only the anchored,
# build-critical patterns -- NOT prose uses of the English word "abilities",
# which are Tier-2 brand copy reviewed by hand.
#
# Intentional references to the old name (migration docs, the compat shims that
# READ the old env vars for one-release fallback) are exempted two ways:
#   1. files listed in KEEP_FILES below
#   2. any single line tagged with the marker `fno-rename-keep`
#
# Exit 0 = clean; exit 1 = residual found (prints offenders); exit 2 = env error.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)" || { echo "residual-check: not a git repo" >&2; exit 2; }
cd "$ROOT"

# Files allowed to mention the old names (the rename tooling + compat shims).
KEEP_FILES=(
  ':!scripts/rename/**'
  ':!*.lock'
  ':!**/*.lock'
  ':!cli/src/fno/_compat_env.py'
  ':!cli/tests/unit/test_compat_env.py'
  ':!crates/fno-agents/src/compat_env.rs'
  ':!CHANGELOG.md'
  ':!**/CHANGELOG.md'
  # .gitignore intentionally keeps the legacy `.abilities/` ignore through the
  # one-release migration window (pattern lines can't carry an inline marker).
  ':!.gitignore'
  # The loc-ratchet trajectory is an append-only ledger whose past entries record
  # the old `abi-agents`/`abilities.` names verbatim (and the rename entry itself
  # names both old and new paths). It is exempt from the rename guard.
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

A structural old-name pattern survived the rename. Re-run the sweep:
    scripts/rename/rename-to-fno.sh
or, for an intentional reference (migration doc / compat shim), add the line
marker `fno-rename-keep` or list the file in residual-check.sh KEEP_FILES.
EOF
  exit 1
fi

echo "residual-check: clean -- no structural old-name patterns remain."
