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
# A `| grep -q` consumer closes the pipe at its first match; without this trap
# the next echo kills the script with SIGPIPE and a pipefail consumer reads
# exit 141 as a failed probe even though every check passed.
trap '' PIPE

SELFTEST=0
if [[ "${1:-}" == "--selftest" ]]; then
  SELFTEST=1
elif [[ $# -ne 1 ]] || [[ ! -f "$1" ]]; then
  echo "usage: validate-prove-it.sh <report-file>   (or --selftest)" >&2
  exit 2
fi

PASS_CT=0
FAIL_CT=0
# return 0 keeps the verdict on FAIL_CT, not on the echo's write status: with
# stdout dead (a `| grep -q` consumer left), a failing echo must not flip the
# `cmd && spass || sfail` chains into double-firing both counters.
spass() { echo "  PASS: $*"; PASS_CT=$((PASS_CT + 1)); return 0; }
sfail() { echo "  FAIL: $*"; FAIL_CT=$((FAIL_CT + 1)); return 0; }

# validate <report-file>: exits 0 when the record is honest, 1 when a PASS
# lacks its probe (the refusal this file exists for), 2 on a malformed record.
validate() {
  local report="$1"
  local last_line prefix record verdict claim_text
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
  claim_text="$(jq -r '.claim // ""' <<<"$record" 2>/dev/null)"
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
      _check_claim_rows "$claims_section" "" || return 1
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
  _check_claim_rows "$claims_section" "$claim_text" || return 1
  if grep -Eq '^[[:space:]]*VERDICT:[[:space:]]*FAIL' <<<"$claims_section"; then
    echo "validate-prove-it: REFUSED - a claim row records VERDICT: FAIL while the terminal record is PASS; there is no partial pass, three of four proven is FAIL until the fourth is. Fix the failing claim or downgrade the record." >&2
    return 1
  fi
  echo "validate-prove-it: PASS accepted (marked probe and claim evidence present)"
  return 0
}

# _check_claim_rows <section> <terminal-claim>: every CLAIM: row must carry
# non-empty claim text, a non-empty CMD:, an integer EXIT:, a non-empty OUT:
# and a VERDICT: of PASS or FAIL. On a PASS the rows must also state the
# claim the terminal record carries (word coverage), so a narrower or
# unrelated row cannot prove a broader claim. Emits one refusal naming the
# first offending row, so the author can find it.
_check_claim_rows() {
  local section="$1" terminal="$2" row_scan bad_claim bad_fields
  row_scan="$(awk -v t="$terminal" '
    function norm(s) {
      s = tolower(s)
      gsub(/[^[:alnum:]]/, " ", s)
      gsub(/ +/, " ", s)
      sub(/^ /, "", s)
      sub(/ $/, "", s)
      return s
    }
    function trim(s) { gsub(/^[[:space:]]+|[[:space:]]+$/, "", s); return s }
    function emit() {
      if (!opened) return
      ctext = trim(claim)
      missing = ""
      if (ctext == "") missing = "CLAIM: text"
      if (cmd == "") missing = missing (missing == "" ? "" : " ") "CMD:"
      if (exitv == "") missing = missing (missing == "" ? "" : " ") "EXIT:"
      if (outv == "") missing = missing (missing == "" ? "" : " ") "OUT:"
      if (verdv == "") missing = missing (missing == "" ? "" : " ") "VERDICT:"
      if (missing != "") {
        printf "BADROW\t%s\t%s\n", ctext, missing
        claim = ""; opened = 0
        exit 1
      }
      rows_all = rows_all " " norm(ctext)
      seen_any = 1
      claim = ""; opened = 0
    }
    # The row-start pattern is anchored so mid-line text (an OUT: capture
    # quoting "CLAIM:") cannot open a phantom row, and the empty-section grep
    # above uses the SAME anchor, so anything one check sees the other parses.
    /^[[:space:]]*([0-9]+\.|-|\*)?[[:space:]]*CLAIM:/ {
      emit()
      line = $0
      sub(/^[[:space:]]*([0-9]+\.|-|\*)?[[:space:]]*CLAIM:[ ]*/, "", line)
      claim = line
      cmd = ""; exitv = ""; outv = ""; verdv = ""
      opened = 1
      next
    }
    opened && /^[[:space:]]*CMD:/ {
      s = $0; sub(/^[[:space:]]*CMD:[ ]?/, "", s); if (s != "") cmd = s; next
    }
    opened && /^[[:space:]]*EXIT:/ {
      s = $0; sub(/^[[:space:]]*EXIT:[ ]?/, "", s); if (s ~ /^-?[0-9]+$/) exitv = s; next
    }
    opened && /^[[:space:]]*OUT:/ {
      s = $0; sub(/^[[:space:]]*OUT:[ ]?/, "", s); if (s != "") outv = s; next
    }
    opened && /^[[:space:]]*VERDICT:/ {
      s = $0; sub(/^[[:space:]]*VERDICT:[ ]?/, "", s)
      gsub(/[[:space:]]/, "", s)
      if (s == "PASS" || s == "FAIL") verdv = s
      next
    }
    END {
      emit()
      if (seen_any && t != "") {
        hay = " " rows_all " "
        n = split(norm(t), tk, " ")
        allc = 1
        for (i = 1; i <= n; i++) if (index(hay, " " tk[i] " ") == 0) allc = 0
        if (!allc) { printf "UNCOVERED\t%s\n", t; exit 1 }
      }
    }
  ' <<<"$section")"
  if [[ "$row_scan" == BADROW* ]]; then
    bad_claim="$(cut -f2 <<<"$row_scan")"
    bad_fields="$(cut -f3 <<<"$row_scan")"
    if [[ -z "$bad_claim" ]]; then
      echo "validate-prove-it: REFUSED - a claim row records no CLAIM: text (${bad_fields}); a row that names no claim checks nothing. State the claim and record CMD:, EXIT:, OUT: and VERDICT: beside it, then re-run." >&2
    else
      echo "validate-prove-it: REFUSED - claim '${bad_claim}' records no ${bad_fields} line; a claim with no command is a claim nobody checked. Run the command fresh and record the claim text with CMD:, EXIT:, OUT: and VERDICT: beside it, then re-run." >&2
    fi
    return 1
  fi
  if [[ "$row_scan" == UNCOVERED* ]]; then
    bad_claim="$(cut -f2- <<<"$row_scan")"
    echo "validate-prove-it: REFUSED - terminal record claim '${bad_claim}' is not evidenced by any ### Claims row; a narrower or unrelated row cannot prove a broader claim. Align a row CLAIM: with the record claim or add a row that states it, then re-run." >&2
    return 1
  fi
  return 0
}

if [[ "$SELFTEST" -eq 1 ]]; then
  TMP="$(mktemp -d -t prove-it-selftest-XXXXXX)"
  trap 'rm -rf "$TMP"' EXIT
  # The check output goes to a file and is cat'ed at the end: a selftest run
  # behind a consumer that closed the pipe early (`| grep -q`) must not have
  # builtin-write failures poison the counters or the exit status.
  exec 3>&1
  exec >"$TMP/selftest-output.txt"

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

  cat > "$TMP/claim-no-verdict.md" <<'EOF'
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
### Findings
(none)
EOF
  printf 'fno-prove-it: {"verdict":"PASS","claim":"the route returns the header"}\n' >> "$TMP/claim-no-verdict.md"

  cat > "$TMP/claim-empty-text.md" <<'EOF'
## Verification: the route returns the header
**Verdict:** PASS
**Claim:** curl starts
**Method:** cold start
### Steps
1. ✅ started the daemon -> pid written
2. 🔍 started twice -> second start refused cleanly
### Claims
1. CLAIM:
   CMD: `curl -s localhost:8080/health`
   EXIT: 0
   OUT: ok
   VERDICT: PASS
### Findings
(none)
EOF
  printf 'fno-prove-it: {"verdict":"PASS","claim":"curl starts"}\n' >> "$TMP/claim-empty-text.md"

  cat > "$TMP/claim-uncovered.md" <<'EOF'
## Verification: the route returns the header
**Verdict:** PASS
**Claim:** everything works
**Method:** cold start
### Steps
1. ✅ started the daemon -> pid written
2. 🔍 killed mid-request -> clean error, no hang
### Claims
1. CLAIM: curl can start
   CMD: `curl -s localhost:8080/health`
   EXIT: 0
   OUT: ok
   VERDICT: PASS
### Findings
(none)
EOF
  printf 'fno-prove-it: {"verdict":"PASS","claim":"everything works"}\n' >> "$TMP/claim-uncovered.md"

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
  validate "$TMP/claim-no-verdict.md" >/dev/null 2>&1 && sfail "a claim row with no VERDICT accepted (absence of failure is not a positive verdict)" || spass "a claim row with no VERDICT is refused"
  refusal_msg="$(validate "$TMP/claim-empty-text.md" 2>&1 || true)"
  if validate "$TMP/claim-empty-text.md" >/dev/null 2>&1; then
    sfail "a claim row with empty text accepted (a row that names no claim checks nothing)"
  elif grep -q 'CLAIM: text' <<<"$refusal_msg"; then
    spass "an empty-claim-text row is refused, naming the missing text"
  else
    sfail "empty-claim-text refusal does not name the missing claim text"
  fi
  refusal_msg="$(validate "$TMP/claim-uncovered.md" 2>&1 || true)"
  if validate "$TMP/claim-uncovered.md" >/dev/null 2>&1; then
    sfail "a broader terminal claim cleared by an unrelated row accepted"
  elif grep -qF 'everything works' <<<"$refusal_msg"; then
    spass "a terminal claim no row evidences is refused, naming the claim"
  else
    sfail "uncovered-claim refusal does not name the terminal claim"
  fi

  echo
  echo "prove-it selftest: $PASS_CT passed, $FAIL_CT failed"
  _st=0
  [[ "$FAIL_CT" -eq 0 ]] || _st=1
  exec 1>&3 3>&-
  cat "$TMP/selftest-output.txt" 2>/dev/null || true
  exit "$_st"
else
  validate "$1"
fi
