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
#         marked probe (refused), PASS with one (accepted), a PASS with no
#         ### Claims section or a claim row with no CMD: (refused), a claim
#         row recording VERDICT: FAIL under a PASS (refused), and each of
#         the no-verdict states passing through untouched.
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
  # Scope each search to its own section: a 🔍 or a CMD: anywhere else in the
  # report (a Findings note, an explanatory sentence) must not satisfy a check
  # whose whole point is that the step was actually run and the claim actually
  # carries its command.
  local steps_section claims_section
  steps_section="$(awk '/^### Steps/{f=1; next} /^### /{f=0} f' "$report")"
  claims_section="$(awk '/^### Claims/{f=1; next} /^### /{f=0} f' "$report")"
  if [[ "$verdict" != "PASS" ]]; then
    # FAIL/BLOCKED/SKIP carry no verdict on the change; pass through unless a
    # Claims section exists and a row in it is malformed.
    echo "validate-prove-it: $verdict recorded (no pass claimed)"
    if [[ -n "$claims_section" ]]; then
      _check_claim_rows "$claims_section" || return 1
    fi
    return 0
  fi
  if ! grep -q '🔍' <<<"$steps_section"; then
    echo "validate-prove-it: REFUSED - PASS with no marked probe (🔍) in the Steps list; a happy-path replay is not a verification. Add at least one probe off the claim's path and re-run." >&2
    return 1
  fi
  if ! grep -Eq '^[[:space:]]*([0-9]+\.|-|\*)?[[:space:]]*CLAIM:' <<<"$claims_section"; then
    echo "validate-prove-it: REFUSED - PASS with no ### Claims section; a completion claim must record the command that proved it. Add a ### Claims block, one row per claim with CLAIM:, CMD:, EXIT:, OUT: and VERDICT:, and re-run." >&2
    return 1
  fi
  _check_claim_rows "$claims_section" || return 1
  if grep -Eq '^[[:space:]]*VERDICT:[[:space:]]*FAIL' <<<"$claims_section"; then
    echo "validate-prove-it: REFUSED - a claim row records VERDICT: FAIL while the terminal record is PASS; there is no partial pass, three of four proven is FAIL until the fourth is. Fix the failing claim or downgrade the record." >&2
    return 1
  fi
  echo "validate-prove-it: PASS accepted (marked probe and claim evidence present)"
  return 0
}

# _check_claim_rows <section>: every CLAIM: row must carry a non-empty CMD:,
# an integer EXIT: and a non-empty OUT:. Emits one refusal quoting the first
# malformed row's claim text, so the author can find it.
_check_claim_rows() {
  local section="$1" row_scan bad_claim bad_fields
  row_scan="$(awk '
    function emit() {
      if (claim == "") return
      missing = ""
      if (cmd == "") missing = "CMD:"
      if (exitv == "") missing = missing (missing == "" ? "" : " ") "EXIT:"
      if (outv == "") missing = missing (missing == "" ? "" : " ") "OUT:"
      if (missing != "") {
        sub(/[[:space:]]+$/, "", claim)
        printf "BADROW\t%s\t%s\n", claim, missing
        claim = ""
        exit 1
      }
      claim = ""
    }
    # The row-start pattern is anchored so mid-line text (an OUT: capture
    # quoting "CLAIM:") cannot open a phantom row, and the empty-section grep
    # above uses the SAME anchor, so anything one check sees the other parses.
    /^[[:space:]]*([0-9]+\.|-|\*)?[[:space:]]*CLAIM:/ {
      if (claim != "") emit()
      line = $0
      sub(/^[[:space:]]*([0-9]+\.|-|\*)?[[:space:]]*CLAIM:[ ]*/, "", line)
      claim = line
      cmd = ""; exitv = ""; outv = ""
      next
    }
    claim != "" && /^[[:space:]]*CMD:/ {
      s = $0; sub(/^[[:space:]]*CMD:[ ]?/, "", s); if (s != "") cmd = s; next
    }
    claim != "" && /^[[:space:]]*EXIT:/ {
      s = $0; sub(/^[[:space:]]*EXIT:[ ]?/, "", s); if (s ~ /^-?[0-9]+$/) exitv = s; next
    }
    claim != "" && /^[[:space:]]*OUT:/ {
      s = $0; sub(/^[[:space:]]*OUT:[ ]?/, "", s); if (s != "") outv = s; next
    }
    END { if (claim != "") emit() }
  ' <<<"$section")"
  if [[ "$row_scan" == BADROW* ]]; then
    bad_claim="$(cut -f2 <<<"$row_scan")"
    bad_fields="$(cut -f3 <<<"$row_scan")"
    echo "validate-prove-it: REFUSED - claim '${bad_claim}' records no ${bad_fields} line; a claim with no command is a claim nobody checked. Run the command fresh and record CMD:, EXIT: and OUT: beside it, then re-run." >&2
    return 1
  fi
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
### Claims
1. CLAIM: resolve-level answers every provider
   CMD: `fno do review resolve-level high`
   EXIT: 0
   OUT: model gpt-5.6-sol, effort high
   VERDICT: PASS
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
### Claims
1. CLAIM: resolve-level answers every provider
   CMD: `fno do review resolve-level high`
   EXIT: 0
   OUT: model gpt-5.6-sol
   VERDICT: PASS
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

  cat > "$TMP/claims-good.md" <<'EOF'
## Verification: the route returns the header
**Verdict:** PASS
**Claim:** the badge refreshes on every focus event
**Method:** cold start
### Steps
1. ✅ focused the window -> badge text changed
2. 🔍 focused with a stale badge state -> still refreshed
### Claims
1. CLAIM: the badge refreshes on every focus event
   CMD: `npm run e2e -- badge.spec`
   EXIT: 0
   OUT: 2 passed, captured line 'CLAIM:' echoed inside the output
   VERDICT: PASS
### Findings
(none)
EOF
  printf 'fno-prove-it: {"verdict":"PASS","claim":"the badge refreshes on every focus event"}\n' >> "$TMP/claims-good.md"

  cat > "$TMP/claim-no-cmd.md" <<'EOF'
## Verification: the route returns the header
**Verdict:** PASS
**Claim:** the badge refreshes on every focus event
**Method:** cold start
### Steps
1. ✅ focused the window -> badge text changed
2. 🔍 focused with a stale badge state -> still refreshed
### Claims
1. CLAIM: the badge refreshes on every focus event
   EXIT: 0
   OUT: badge text changed to saved
   VERDICT: PASS
### Findings
(none)
EOF
  printf 'fno-prove-it: {"verdict":"PASS","claim":"the badge refreshes on every focus event"}\n' >> "$TMP/claim-no-cmd.md"

  cat > "$TMP/no-claims.md" <<'EOF'
## Verification: the route returns the header
**Verdict:** PASS
**Claim:** resolve-level answers every provider
**Method:** cold start
### Steps
1. ✅ ran resolve-level high -> model gpt-5.6-sol
2. 🔍 resolve-level bogus-provider -> resolved unscoped
### Findings
(none)
EOF
  printf 'fno-prove-it: {"verdict":"PASS","claim":"resolve-level answers every provider"}\n' >> "$TMP/no-claims.md"

  cat > "$TMP/claim-fail-verdict.md" <<'EOF'
## Verification: the route returns the header
**Verdict:** PASS
**Claim:** the route returns the header
**Method:** cold start
### Steps
1. ✅ drove the route -> 200
2. 🔍 empty body -> 400, clean error
### Claims
1. CLAIM: the route returns the header
   CMD: `curl -s localhost:8080/header`
   EXIT: 0
   OUT: x-prove-it: 1
   VERDICT: FAIL
### Findings
(none)
EOF
  printf 'fno-prove-it: {"verdict":"PASS","claim":"the route returns the header"}\n' >> "$TMP/claim-fail-verdict.md"

  validate "$TMP/good.md" >/dev/null 2>&1 && spass "PASS with a marked probe accepted" || sfail "PASS with a marked probe refused"
  validate "$TMP/no-probe.md" >/dev/null 2>&1 && sfail "PASS with NO probe accepted (the refusal is the behavior under test)" || spass "PASS with NO probe refused, naming the missing probe"
  refusal_msg="$(validate "$TMP/no-probe.md" 2>&1 || true)"
  grep -q 'no marked probe' <<<"$refusal_msg" && spass "refusal reason names the missing probe" || sfail "refusal reason does not name the missing probe"
  validate "$TMP/marker-outside-steps.md" >/dev/null 2>&1 && sfail "PASS accepted on a marker outside Steps (AC-scope: Findings note satisfied the check)" || spass "PASS with the marker only outside Steps is still refused"
  validate "$TMP/fail.md" >/dev/null 2>&1 && spass "FAIL passes through untouched" || sfail "FAIL rejected"
  validate "$TMP/blocked.md" >/dev/null 2>&1 && spass "BLOCKED passes through (no verdict claimed)" || sfail "BLOCKED rejected"
  validate "$TMP/skip.md" >/dev/null 2>&1 && spass "SKIP passes through (no verdict claimed)" || sfail "SKIP rejected"
  validate "$TMP/claims-good.md" >/dev/null 2>&1 && spass "PASS with a well-formed claim row accepted" || sfail "PASS with a well-formed claim row refused"
  refusal_msg="$(validate "$TMP/claim-no-cmd.md" 2>&1 || true)"
  if validate "$TMP/claim-no-cmd.md" >/dev/null 2>&1; then
    sfail "a claim with no command accepted (the refusal is the behavior under test)"
  elif grep -qF 'the badge refreshes on every focus event' <<<"$refusal_msg"; then
    spass "PASS: a claim with no command is refused"
  else
    sfail "a claim with no command refused without quoting the claim text"
  fi
  refusal_msg="$(validate "$TMP/no-claims.md" 2>&1 || true)"
  if validate "$TMP/no-claims.md" >/dev/null 2>&1; then
    sfail "PASS with no ### Claims section accepted (the refusal is the behavior under test)"
  elif grep -q '### Claims' <<<"$refusal_msg"; then
    spass "PASS with no ### Claims section refused, naming the missing section"
  else
    sfail "no-### Claims refusal does not name the missing section"
  fi
  validate "$TMP/claim-fail-verdict.md" >/dev/null 2>&1 && sfail "PASS accepted while a claim row records VERDICT: FAIL (there is no partial pass)" || spass "PASS with a VERDICT: FAIL claim row refused"

  echo
  echo "prove-it selftest: $PASS_CT passed, $FAIL_CT failed"
  [[ "$FAIL_CT" -eq 0 ]]
else
  validate "$1"
fi
