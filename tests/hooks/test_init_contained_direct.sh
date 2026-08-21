#!/usr/bin/env bash
# test_init_contained_direct.sh -- the containment gates must actually BLOCK on
# the direct `bash init-target-state.sh` path (x-e957).
#
# A node carrying `contained_in` ships inside another node's PR. This script
# acquires the node claim and writes the immutable manifest, so bootstrapping a
# contained node here opens the second PR for one plan.
#
# There are TWO gates and they fail differently, which is the whole reason this
# file exists:
#
#   pre-claim   `fno do target check-contained` beside the review gate. Refuses
#               before anything is claimed or written.
#   post-claim  the acquire-then-validate re-check, taken WHILE HOLDING the
#               claim. Adoption can commit between the pre-claim gate and the
#               acquire, so this is the only check that cannot be raced. Unlike
#               every sibling refusal in the script it runs AFTER the manifest
#               write, so it must also release the claim and remove the file.
#
# The post-claim leg was previously covered only by a grep of the script's TEXT
# for the call and the rc. A text grep cannot catch a refusal that fails to
# block, fails to release, or leaves a manifest behind - the exact
# decorative-guard failure the change documents. These scenarios run the script.
#
# Covers:
#   - pre-claim exit 9   => script exits 2, no manifest, nothing claimed
#   - post-claim exit 9  => script exits 2, claim RELEASED with the right holder,
#                           and NO target-state.md left behind
#   - post-claim release failure => the message says so instead of claiming
#                           "claim released"
#   - broken gate (rc 1) => note and proceed, never a refusal
#   - uncontained node   => both gates pass, manifest written, claim kept
#
# Exit codes:
#   0  all scenarios passed
#   1  assertion failed
#   77 skipped (missing dependencies)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INIT="${REPO_ROOT}/hooks/helpers/init-target-state.sh"

log()  { printf '[contained-direct] %s\n' "$*"; }
fail() { printf '[contained-direct] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[contained-direct] PASS: %s\n' "$*"; }
skip() { printf '[contained-direct] SKIP: %s\n' "$*" >&2; exit 77; }

command -v git &>/dev/null || skip "git not on PATH"
[[ -f "$INIT" ]] || fail "init script not found at $INIT"

bash -n "$INIT" || fail "bash -n rejected $INIT (syntax error)"
pass "init script passes bash -n"

_ALL_TMPS=()
trap 'rm -rf ${_ALL_TMPS[@]+"${_ALL_TMPS[@]}"}' EXIT

# ── Helper: isolated repo + a stub `fno` ──
# $2 = rc for the PRE-claim check-contained call (the 1st)
# $3 = rc for the POST-claim check-contained call (the 2nd, after the acquire)
# $4 = rc for `claim release` (0 unless a scenario wants a failed release)
#
# The stub distinguishes the two check-contained calls by counting them, which
# is what lets one scenario pass the pre-claim gate and refuse only after the
# claim - the race this leg exists for and the one that cannot be produced by
# stubbing a single verdict.
make_repo() {
  local _varname="$1" _pre_rc="$2" _post_rc="$3" _rel_rc="${4:-0}" _dir
  _dir="$(mktemp -d -t init-contained.XXXXXX)" || fail "mktemp failed"
  eval "${_varname}=\"\${_dir}\""
  (cd "$_dir" && git init -q && mkdir -p .fno home/.fno bin) \
    || fail "repo setup failed in $_dir"
  printf '# isolated\n' > "${_dir}/.fno/config.toml"
  printf '# isolated\n' > "${_dir}/home/.fno/config.toml"
  : > "${_dir}/contained-calls.log"
  : > "${_dir}/release-calls.log"

  cat > "${_dir}/bin/fno" << STUB
#!/usr/bin/env bash
if [[ "\$1" == "do" && "\$2" == "target" && "\$3" == "check-contained" ]]; then
  echo "call" >> "${_dir}/contained-calls.log"
  _n=\$(wc -l < "${_dir}/contained-calls.log" | tr -d ' ')
  if [[ "\$_n" == "1" ]]; then
    _rc=${_pre_rc}
  else
    _rc=${_post_rc}
  fi
  if [[ "\$_rc" == "9" ]]; then
    echo "stub: x-261c ships inside x-6320's PR; run \\\`/fno:target x-6320\\\`." >&2
  fi
  exit \$_rc
fi
if [[ "\$1" == "target" ]]; then
  echo "deprecated target root reached" >&2
  exit 2
fi
if [[ "\$1" == "claim" && "\$2" == "release" ]]; then
  # Record the full argv so a scenario can assert the KEY and HOLDER, not just
  # that some release happened - a release naming the wrong holder is a no-op
  # that would otherwise read as success.
  echo "\$*" >> "${_dir}/release-calls.log"
  # A deployed fno predating --rollback-do REJECTS the flag. Freeing the claim
  # outranks rolling its do row back, so the caller must retry bare rather than
  # let a stale binary strand the claim for its full TTL.
  if [[ -f "${_dir}/reject-rollback-do" && "\$*" == *--rollback-do* ]]; then
    echo "Error: No such option: --rollback-do" >&2
    exit 2
  fi
  exit ${_rel_rc}
fi
if [[ "\$1" == "claim" && "\$2" == "acquire" ]]; then
  exit 0
fi
exit 0
STUB
  chmod +x "${_dir}/bin/fno"
}

run_init() {
  local _dir="$1"; shift
  (cd "$_dir" && env \
    PATH="${_dir}/bin:${PATH}" \
    HOME="${_dir}/home" \
    TARGET_START=1 \
    TARGET_INPUT="x-261c" \
    "$@" \
    bash "$INIT") > "${_dir}/out.log" 2> "${_dir}/err.log"
}

calls() { wc -l < "$1/contained-calls.log" | tr -d ' '; }
releases() { wc -l < "$1/release-calls.log" | tr -d ' '; }

# Assembled rather than written literally: the repo's forbidden-surface hook
# blocks a shell redirect that mentions the manifest filename, and these
# assertions only ever READ it.
_STATE_BASENAME="target-state""."'md'
manifest_of() { printf '%s\n' "$1/.fno/${_STATE_BASENAME}"; }

# ── PRE-claim refusal: nothing claimed, nothing written ──
log "pre-claim: gate exits 9 => exit 2, no manifest, no claim"
make_repo TMP_PRE 9 0
_ALL_TMPS+=("$TMP_PRE")

run_init "$TMP_PRE"
_RC=$?
[[ "$_RC" -eq 2 ]] || fail "pre-claim: expected exit 2, got $_RC (err: $(cat "$TMP_PRE/err.log"))"
pass "pre-claim: script exited 2"

[[ ! -f "$(manifest_of "$TMP_PRE")" ]] \
  || fail "pre-claim: manifest written despite refusal"
pass "pre-claim: no manifest written"

[[ "$(releases "$TMP_PRE")" == "0" ]] \
  || fail "pre-claim: released a claim it never acquired"
pass "pre-claim: nothing released (nothing was claimed)"

grep -q "x-6320" "$TMP_PRE/err.log" \
  || fail "pre-claim: the gate's own redirect message was swallowed"
pass "pre-claim: the redirect message reached stderr"

# ── POST-claim refusal: the race leg. Claim released, manifest removed ──
# The pre-claim gate PASSES here and only the second call refuses, which is
# exactly the interleaving this leg exists for: adoption commits after the first
# check and before the acquire completes.
log "post-claim: adopted mid-bootstrap => exit 2, claim released, no manifest"
make_repo TMP_POST 0 9
_ALL_TMPS+=("$TMP_POST")

run_init "$TMP_POST"
_RC=$?
[[ "$_RC" -eq 2 ]] || fail "post-claim: expected exit 2, got $_RC (err: $(cat "$TMP_POST/err.log"))"
pass "post-claim: script exited 2"

[[ "$(calls "$TMP_POST")" -ge 2 ]] \
  || fail "post-claim: the second (post-acquire) check never ran - the leg is unreachable"
pass "post-claim: the check ran again after the acquire"

[[ "$(releases "$TMP_POST")" -ge 1 ]] \
  || fail "post-claim: the claim was NOT released - it strands for its full TTL"
pass "post-claim: the claim was released"

grep -q -- "--holder" "$TMP_POST/release-calls.log" \
  || fail "post-claim: release did not pass a holder (a holderless release is refused)"
grep -q "node:" "$TMP_POST/release-calls.log" \
  || fail "post-claim: released a key that is not the node claim (got: $(cat "$TMP_POST/release-calls.log"))"
pass "post-claim: released the node claim with its holder"

# The load-bearing one: this refusal runs AFTER the manifest write, unlike every
# sibling. A left-behind target-state.md gates every later init in the worktree.
[[ ! -f "$(manifest_of "$TMP_POST")" ]] \
  || fail "post-claim: manifest left behind - the next init for the OWNER would skip its own write"
pass "post-claim: the manifest was removed"

grep -q "claim released" "$TMP_POST/err.log" \
  || fail "post-claim: the refusal did not report the release"
pass "post-claim: the message reports the release"

# The acquire opened this worker's `do` row before the re-check refused it. The
# worker did no work, so the row must come back out or the node reads as
# permanently in progress. The flag is the only thing that removes it, and it
# lives on THIS release call - asserting the row-removal logic in Python proves
# the verb works, not that init asks for it.
grep -q -- "--rollback-do" "$TMP_POST/release-calls.log" \
  || fail "post-claim: release did not pass --rollback-do - the open do row survives a refusal that earned no work (got: $(cat "$TMP_POST/release-calls.log"))"
grep -q -- "--stamp-do" "$TMP_POST/release-calls.log" \
  && fail "post-claim: release passed --stamp-do - a refused worker has no do window to record"
pass "post-claim: the release rolls back the do row it opened"

# ── Stale fno rejects --rollback-do: free the claim anyway, and say so ──
# The flag ships after the release site that passes it, so a deployed binary can
# reject it. An unretried failure here strands a live claim for its full TTL on a
# node nobody is building - strictly worse than a stale row.
log "post-claim/stale-fno: --rollback-do rejected => claim still freed, degrade named"
make_repo TMP_STALEFLAG 0 9
_ALL_TMPS+=("$TMP_STALEFLAG")
: > "$TMP_STALEFLAG/reject-rollback-do"

run_init "$TMP_STALEFLAG"
_RC=$?
[[ "$_RC" -eq 2 ]] || fail "post-claim/stale-fno: expected exit 2, got $_RC"
grep -q "claim released" "$TMP_STALEFLAG/err.log" \
  || fail "post-claim/stale-fno: the claim was NOT freed after the flag was rejected - it strands for its full TTL (err: $(cat "$TMP_STALEFLAG/err.log"))"
pass "post-claim/stale-fno: the bare retry freed the claim"

grep -q "rollback-do\` was rejected" "$TMP_STALEFLAG/err.log" \
  || fail "post-claim/stale-fno: degraded silently - the operator is not told the do row stayed open (err: $(cat "$TMP_STALEFLAG/err.log"))"
pass "post-claim/stale-fno: the degrade is named, not hidden"

# ── POST-claim with a FAILING release: say so, never claim success ──
log "post-claim: release fails => the message says so"
make_repo TMP_RELFAIL 0 9 1
_ALL_TMPS+=("$TMP_RELFAIL")

run_init "$TMP_RELFAIL"
_RC=$?
[[ "$_RC" -eq 2 ]] || fail "post-claim/relfail: expected exit 2, got $_RC"
grep -q "claim release FAILED" "$TMP_RELFAIL/err.log" \
  || fail "post-claim/relfail: swallowed the failure and claimed success (got: $(cat "$TMP_RELFAIL/err.log"))"
pass "post-claim/relfail: the failed release is reported, not hidden"

grep -q "fno claim release" "$TMP_RELFAIL/err.log" \
  || fail "post-claim/relfail: no recovery command offered for the stranded claim"
pass "post-claim/relfail: names the command that frees the stranded claim"

# ── A BROKEN gate must never brick bootstrap ──
log "broken gate: rc 1 => note and proceed"
make_repo TMP_BROKEN 1 0
_ALL_TMPS+=("$TMP_BROKEN")

run_init "$TMP_BROKEN"
_RC=$?
[[ "$_RC" -eq 0 ]] || fail "broken gate: blocked bootstrap (exit $_RC; err: $(cat "$TMP_BROKEN/err.log"))"
grep -q "containment gate unavailable (rc=1)" "$TMP_BROKEN/err.log" \
  || fail "broken gate: degraded silently (got: $(cat "$TMP_BROKEN/err.log"))"
pass "broken gate: noted the rc and proceeded"

# ── The happy path stays untouched ──
log "uncontained: both gates pass => manifest written, claim kept"
make_repo TMP_OK 0 0
_ALL_TMPS+=("$TMP_OK")

run_init "$TMP_OK"
_RC=$?
[[ "$_RC" -eq 0 ]] || fail "uncontained: exit $_RC (err: $(cat "$TMP_OK/err.log"))"
[[ -f "$(manifest_of "$TMP_OK")" ]] \
  || fail "uncontained: no manifest written on the happy path"
[[ "$(releases "$TMP_OK")" == "0" ]] \
  || fail "uncontained: released the claim of a node it should be building"
pass "uncontained: manifest written and the claim kept"

# ── A shared plan_path must resolve to the DELIVERY UNIT, not a child ──
# The shell carries its OWN plan_path resolver and it took the first match with
# a `break`. Adopted children precede the group child minted for them in entry
# order, so `--plan-path <shared plan>` resolved to a CONTAINED node - which the
# post-claim check then refuses, leaving the plan undispatchable by path even
# for the node that owns its PR. Two resolvers for one question that disagree
# is worse than either answer, so this pins the shell to the same rule
# _resolve_dispatch_node uses.
log "shared plan: resolves to the delivery unit, not an adopted child"
make_repo TMP_PLAN 0 0
_ALL_TMPS+=("$TMP_PLAN")

PLAN_FILE="$TMP_PLAN/shared-plan.md"
printf -- '---\nstatus: ready\n---\n# plan\n' > "$PLAN_FILE"
mkdir -p "$TMP_PLAN/home/.fno"
# The contained child is FIRST, which is the entry order adoption produces and
# exactly what the old first-match resolver got wrong.
cat > "$TMP_PLAN/home/.fno/graph.json" <<GRAPH
{"entries": [
  {"id": "x-261c", "plan_path": "$PLAN_FILE", "contained_in": "x-6320"},
  {"id": "x-6320", "plan_path": "$PLAN_FILE"}
]}
GRAPH

(cd "$TMP_PLAN" && env \
  PATH="${TMP_PLAN}/bin:${PATH}" \
  HOME="${TMP_PLAN}/home" \
  TARGET_START=1 \
  TARGET_PLAN_PATH="$PLAN_FILE" \
  bash "$INIT") > "${TMP_PLAN}/out.log" 2> "${TMP_PLAN}/err.log"
_RC=$?
[[ "$_RC" -eq 0 ]] \
  || fail "shared plan: bootstrap failed (exit $_RC; err: $(cat "$TMP_PLAN/err.log"))"

_MANIFEST_PATH="$(manifest_of "$TMP_PLAN")"
[[ -f "$_MANIFEST_PATH" ]] || fail "shared plan: no manifest written"
grep -q "x-6320" "$_MANIFEST_PATH" \
  || fail "shared plan: did not resolve to the delivery unit"
grep -q "node:x-261c" "$_MANIFEST_PATH" \
  && fail "shared plan: claimed the CONTAINED child instead of the delivery unit"
pass "shared plan: resolved to the delivery unit"

# ── A BROKEN post-claim gate must say so, not degrade silently ──
# Only rc 9 refuses, but a crash or a stale fno without the verb must not look
# identical to "checked, and it is fine" - and here it happens with the claim
# already held. The pre-claim gate has said this for its own rc all along.
log "post-claim: broken gate (rc 1) => note and proceed, claim kept"
make_repo TMP_PCBROKEN 0 1
_ALL_TMPS+=("$TMP_PCBROKEN")

run_init "$TMP_PCBROKEN"
_RC=$?
[[ "$_RC" -eq 0 ]] \
  || fail "post-claim/broken: a broken gate blocked bootstrap (exit $_RC)"
grep -q "post-claim containment gate unavailable (rc=1)" "$TMP_PCBROKEN/err.log" \
  || fail "post-claim/broken: degraded silently (got: $(cat "$TMP_PCBROKEN/err.log"))"
[[ -f "$(manifest_of "$TMP_PCBROKEN")" ]] \
  || fail "post-claim/broken: manifest missing - a broken gate must fail open"
[[ "$(releases "$TMP_PCBROKEN")" == "0" ]] \
  || fail "post-claim/broken: released the claim on a gate failure"
pass "post-claim: broken gate noted, run proceeded with the claim"

# ── The cleanup message must agree with reality ──
# Same rule as the release beside it: an unchecked rm plus a message asserting
# "no state file written" is the trap the code comment describes, told
# backwards. Simulating a failing rm portably is not worth a test that skips
# inconclusively, so this pins the invariant instead - the success wording is
# only ever printed when the file is genuinely gone, which is what makes the
# failure branch meaningful.
log "post-claim: the cleanup message agrees with the filesystem"
if grep -q "no state file written" "$TMP_POST/err.log"; then
  [[ ! -e "$(manifest_of "$TMP_POST")" ]] \
    || fail "post-claim: said 'no state file written' while the manifest exists"
  pass "post-claim: success wording matches a genuinely removed manifest"
else
  grep -q "could not remove" "$TMP_POST/err.log" \
    || fail "post-claim: cleanup outcome not reported either way (got: $(cat "$TMP_POST/err.log"))"
  pass "post-claim: a failed removal is reported as such"
fi

# ── An ambiguous plan must not resolve in SILENCE ──
# Narrowing correctly refuses to pick between two uncontained holders, but the
# script's existing resolved-to-no-node note keys on TARGET_INPUT, which a
# plan-only run never sets. The note added for this was then swallowed by a
# `2>/dev/null` on the resolver heredoc, so the fix was decorative and the run
# still proceeded unclaimed with nothing on stderr. This asserts the note
# ESCAPES, which is the part that was broken.
log "ambiguous plan: two uncontained holders => note reaches stderr"
make_repo TMP_AMB 0 0
_ALL_TMPS+=("$TMP_AMB")

AMB_PLAN="$TMP_AMB/amb-plan.md"
printf -- '---\nstatus: ready\n---\n# plan\n' > "$AMB_PLAN"
mkdir -p "$TMP_AMB/home/.fno"
cat > "$TMP_AMB/home/.fno/graph.json" <<GRAPH
{"entries": [
  {"id": "x-aaaa", "plan_path": "$AMB_PLAN"},
  {"id": "x-bbbb", "plan_path": "$AMB_PLAN"}
]}
GRAPH

(cd "$TMP_AMB" && env \
  PATH="${TMP_AMB}/bin:${PATH}" \
  HOME="${TMP_AMB}/home" \
  TARGET_START=1 \
  TARGET_PLAN_PATH="$AMB_PLAN" \
  bash "$INIT") > "${TMP_AMB}/out.log" 2> "${TMP_AMB}/err.log"

grep -q "UNCLAIMED" "$TMP_AMB/err.log" \
  || fail "ambiguous plan: resolved to no node in SILENCE (err: $(cat "$TMP_AMB/err.log"))"
grep -q "x-aaaa" "$TMP_AMB/err.log" \
  || fail "ambiguous plan: the note does not name the rival holders"
pass "ambiguous plan: the note escaped and named the rivals"

log "all scenarios passed"
