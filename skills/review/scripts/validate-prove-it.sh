#!/usr/bin/env bash
# Validate the machine-readable terminal record of `/fno:review prove-it` and
# refuse an unprovable PASS.
#
# The load-bearing rule (AC6-MARKER): a Steps list with no marked probe may
# not report PASS. /verify would still say PASS with a note; fno's prove-it
# REFUSES, because a happy-path replay is not half a verification, it is the
# shape of one with the evidence left out. Asserting that a good run passes
# proves nothing about the case the rule exists for, so the selftest's
# negative controls are the point.
#
# Report contract (references/prove-it.md): the LAST line is
#   fno-prove-it: {"verdict":"PASS|FAIL|BLOCKED|SKIP","claim":"..."}
# and the body's Steps section marks probes with a leading 🔍.
#
# Modes:
#   validate-prove-it.sh <report-file>    validate + emit verdict guidance
#   validate-prove-it.sh --selftest       run built-in fixtures proving the
#                                         validator detects: PASS with no
#         marked probe (refused), PASS with one (accepted), and each of the
#         no-verdict states passing through untouched.
set -uo pipefail

SELFTEST=0
if [[ "${1:-}" == "--selftest" ]]; then
  SELFTEST=1
elif [[ $# -ne 1 ]] || [[ ! -f "$1" ]]; then
  echo "usage: validate-prove-it.sh <report-file>   (or --selftest)" >&2
  exit 2
fi

PASS_CT=0
FAIL_CT=0
spass() { echo "  PASS: $*"; PASS_CT=$((PASS_CT + 1)); }
sfail() { echo "  FAIL: $*"; FAIL_CT=$((FAIL_CT + 1)); }

# validate <report-file>: exits 0 when the record is honest, 1 when a PASS
# lacks its probe (the refusal this file exists for), 2 on a malformed record.
validate() {
  local report="$1"
  local last_line prefix record verdict
  [[ -f "$report" ]] || { echo "validate-prove-it: report not found: $report" >&2; return 2; }
  last_line="$(awk 'NF { line=$0 } END { print line }' "$report")"
  [[ -n "$last_line" ]] || { echo "validate-prove-it: report is empty" >&2; return 2; }
  prefix="fno-prove-it: "
  [[ "$last_line" == "$prefix"* ]] || {
    echo "validate-prove-it: missing terminal fno-prove-it record" >&2
    return 2
  }
  record="${last_line#"$prefix"}"
  verdict="$(jq -r '.verdict // ""' <<<"$record" 2>/dev/null)"
  case "$verdict" in
    PASS|FAIL|BLOCKED|SKIP) ;;
    *) echo "validate-prove-it: terminal record carries no legal verdict" >&2; return 2 ;;
  esac
  if [[ "$verdict" != "PASS" ]]; then
    # FAIL/BLOCKED/SKIP carry no verdict on the change; nothing to refuse.
    echo "validate-prove-it: $verdict recorded (no pass claimed)"
    return 0
  fi
  # Scope the marker search to the Steps section alone: a 🔍 anywhere else in
  # the report (a Findings note, an explanatory sentence) must not satisfy a
  # check whose whole point is that a STEP was actually run off the happy path.
  local steps_section
  steps_section="$(awk '/^### Steps/{f=1; next} /^### /{f=0} f' "$report")"
  if ! grep -q '🔍' <<<"$steps_section"; then
    echo "validate-prove-it: REFUSED - PASS with no marked probe (🔍) in the Steps list; a happy-path replay is not a verification. Add at least one probe off the claim's path and re-run." >&2
    return 1
  fi
  echo "validate-prove-it: PASS accepted (marked probe present)"
  return 0
}

if [[ "$SELFTEST" -eq 1 ]]; then
  TMP="$(mktemp -d -t prove-it-selftest-XXXXXX)"
  trap 'rm -rf "$TMP"' EXIT

  cat > "$TMP/good.md" <<'EOF'
## Verification: the route returns the header
**Verdict:** PASS
**Claim:** /fno:review <level> resolves a model on every provider
**Method:** cold start; fno do review resolve-level
### Steps
1. ✅ ran resolve-level high -> model gpt-5.6-sol, effort high
2. 🔍 resolve-level bogus-provider -> resolved unscoped, never refused
### Findings
- 🔍 bogus provider -> unscoped pick, held
EOF
  printf 'fno-prove-it: {"verdict":"PASS","claim":"resolve-level answers every provider"}\n' >> "$TMP/good.md"

  cat > "$TMP/no-probe.md" <<'EOF'
## Verification: the route returns the header
**Verdict:** PASS
**Claim:** resolve-level answers every provider
**Method:** cold start
### Steps
1. ✅ ran resolve-level high -> model gpt-5.6-sol
### Findings
(none)
EOF
  printf 'fno-prove-it: {"verdict":"PASS","claim":"resolve-level answers every provider"}\n' >> "$TMP/no-probe.md"

  cat > "$TMP/marker-outside-steps.md" <<'EOF'
## Verification: the route returns the header
**Verdict:** PASS
**Claim:** resolve-level answers every provider
**Method:** cold start
### Steps
1. ✅ ran resolve-level high -> model gpt-5.6-sol
### Findings
- 🔍 a note that happens to carry the marker glyph, not a run step
EOF
  printf 'fno-prove-it: {"verdict":"PASS","claim":"resolve-level answers every provider"}\n' >> "$TMP/marker-outside-steps.md"

  cat > "$TMP/fail.md" <<'EOF'
## Verification: the route returns the header
**Verdict:** FAIL
**Claim:** the route returns the header
### Steps
1. ❌ drove the route -> 404
EOF
  printf 'fno-prove-it: {"verdict":"FAIL","claim":"the route returns the header"}\n' >> "$TMP/fail.md"

  cat > "$TMP/blocked.md" <<'EOF'
## Verification: no reachable artifact
**Verdict:** BLOCKED
**Claim:** unchanged
### Steps
1. ❌ build failed at step 2
EOF
  printf 'fno-prove-it: {"verdict":"BLOCKED","claim":"build blocked"}\n' >> "$TMP/blocked.md"

  cat > "$TMP/skip.md" <<'EOF'
## Verification: docs-only change
**Verdict:** SKIP
**Claim:** no runtime surface
### Steps
1. ✅ read the diff -> markdown only
EOF
  printf 'fno-prove-it: {"verdict":"SKIP","claim":"no runtime surface"}\n' >> "$TMP/skip.md"

  validate "$TMP/good.md" >/dev/null 2>&1 && spass "PASS with a marked probe accepted" || sfail "PASS with a marked probe refused"
  validate "$TMP/no-probe.md" >/dev/null 2>&1 && sfail "PASS with NO probe accepted (the refusal is the behavior under test)" || spass "PASS with NO probe refused, naming the missing probe"
  refusal_msg="$(validate "$TMP/no-probe.md" 2>&1 || true)"
  grep -q 'no marked probe' <<<"$refusal_msg" && spass "refusal reason names the missing probe" || sfail "refusal reason does not name the missing probe"
  validate "$TMP/marker-outside-steps.md" >/dev/null 2>&1 && sfail "PASS accepted on a marker outside Steps (AC-scope: Findings note satisfied the check)" || spass "PASS with the marker only outside Steps is still refused"
  validate "$TMP/fail.md" >/dev/null 2>&1 && spass "FAIL passes through untouched" || sfail "FAIL rejected"
  validate "$TMP/blocked.md" >/dev/null 2>&1 && spass "BLOCKED passes through (no verdict claimed)" || sfail "BLOCKED rejected"
  validate "$TMP/skip.md" >/dev/null 2>&1 && spass "SKIP passes through (no verdict claimed)" || sfail "SKIP rejected"

  echo
  echo "prove-it selftest: $PASS_CT passed, $FAIL_CT failed"
  [[ "$FAIL_CT" -eq 0 ]]
else
  validate "$1"
fi
