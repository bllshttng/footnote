#!/usr/bin/env bash
# test_init_foreign_manifest_refusal.sh -- target init refuses a foreign
# manifest instead of silently skipping the claim (x-7040).
#
# Covers:
#   T1 (AC1-HP, AC5-FR)  a manifest whose fno_id is not this run's -> refuse,
#                        name both ids, name the repair, mutate nothing
#   T2 (AC2-HP)          a manifest carrying THIS run's fno_id -> today's
#                        resume path: exit 0, "leaving unchanged", unchanged file
#   T3 (AC4-EDGE)        a manifest with no readable fno_id -> refuse by that
#                        reason; an unreadable id is never a match
#   T4 (AC6-FR)          a fresh worktree (no manifest) -> today's behavior:
#                        manifest written, exit 0
#   T5 (AC3-HP)          follow the refusal: state archive, re-init -> the new
#                        run id owns the manifest
#   T6 (resume rescue)   a re-invocation from the SAME harness session that
#                        wrote the manifest keeps today's resume path; a
#                        different harness session refuses
#
# Exit codes: 0 all passed, 1 assertion failed, 77 skipped (missing deps).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INIT="${REPO_ROOT}/hooks/helpers/init-target-state.sh"

log()  { printf '[foreign-manifest] %s\n' "$*"; }
fail() { printf '[foreign-manifest] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[foreign-manifest] PASS: %s\n' "$*"; }
skip() { printf '[foreign-manifest] SKIP: %s\n' "$*" >&2; exit 77; }

command -v git &>/dev/null || skip "git not on PATH"
[[ -f "$INIT" ]] || fail "init script not found at $INIT"

# The state CLI for the one verb the suite runs (T5's archive repair). The
# binary's name differs by environment: a developer PATH has `fno`; the smoke
# env pins cli/.venv/bin, which ships `fno-py`. Same command tree either way.
if command -v fno &>/dev/null; then
  STATE_CLI=fno
elif command -v fno-py &>/dev/null; then
  STATE_CLI=fno-py
else
  STATE_CLI=""
fi

bash -n "$INIT" || fail "bash -n rejected $INIT (syntax error)"
pass "init script passes bash -n"

_ALL_TMPS=()
trap 'rm -rf "${_ALL_TMPS[@]}"' EXIT

# Scrub inherited harness markers so each scenario's env is exactly what it
# sets (same convention as test_init_target_session_id.sh).
unset CLAUDE_CODE_SESSION_ID CLAUDECODE_SESSION_ID CODEX_THREAD_ID \
      CODEX_SESSION_ID GEMINI_SESSION_ID OPENCODE_SESSION_ID TARGET_TRANSCRIPT_ID \
      TARGET_SESSION_ID 2>/dev/null || true

export FNO_TARGET_INIT_GATED=1

make_repo() {
  local _varname="$1"
  local _dir
  _dir="$(mktemp -d -t init-foreign-manifest.XXXXXX)" || fail "mktemp failed"
  eval "${_varname}=\"\${_dir}\""
  _ALL_TMPS+=("$_dir")
  (cd "$_dir" && git init -q && mkdir -p .fno) || fail "repo setup failed in $_dir"
  printf '# isolated\n' > "${_dir}/.fno/config.toml"
  mkdir -p "${_dir}/home/.fno"
  printf '# isolated global\n' > "${_dir}/home/.fno/config.toml"
}

manifest_path() { # $1 repo dir - the resolver init itself uses, with the same
  # legacy fallback the init script uses when no binary resolves it
  local p
  p="$(cd "$1" && FNO_SPACES_DIR="$1/spaces" fno-agents state path target-state 2>/dev/null || true)"
  printf '%s\n' "${p:-$1/.fno/target-state.md}"
}

run_init() { # $1 repo dir, $2 stdout capture file, rest: K=V env for this run
  local dir="$1" out="$2"
  shift 2
  (cd "$dir" && \
    HOME="$dir/home" \
    FNO_SPACES_DIR="$dir/spaces" \
    TARGET_START=1 \
    TARGET_INPUT="x-7040-foreign-manifest" \
    TARGET_LOCATION_OK="main-acknowledged" \
    env "$@" \
    bash "$INIT" >"$out" 2>"$out.err")
  echo $?
}

# ── T1 (AC1-HP + AC5-FR): foreign fno_id refuses, names both ids + repair ──
log "T1: differing fno_id refuses, names both ids and the repair, mutates nothing"

make_repo TMP1
STATE1="$(manifest_path "$TMP1")"
[[ -n "$STATE1" ]] || fail "T1: could not resolve manifest path"

run_init "$TMP1" "$TMP1/init-a.out" \
  TARGET_SESSION_ID="runA-20260901T000000Z-cl111-aaaaaa" >/dev/null
[[ -f "$STATE1" ]] || fail "T1: first init did not write the manifest"

BEFORE_SHA="$(shasum "$STATE1" | awk '{print $1}')"
RC_B="$(run_init "$TMP1" "$TMP1/init-b.out" TARGET_SESSION_ID="runB-20260906T000000Z-cl222-bbbbbb")"
AFTER_SHA="$(shasum "$STATE1" | awk '{print $1}')"

[[ "$RC_B" != "0" ]] || fail "T1: foreign-manifest init exited 0; wanted a refusal (output: $(cat "$TMP1/init-b.out"))"
pass "T1a: foreign manifest refused (exit $RC_B)"

OUT_B="$(cat "$TMP1/init-b.out" "$TMP1/init-b.out.err")"
echo "$OUT_B" | grep -q "runA-20260901T000000Z-cl111-aaaaaa" \
  || fail "T1b: refusal does not name the manifest's fno_id"
echo "$OUT_B" | grep -q "runB-20260906T000000Z-cl222-bbbbbb" \
  || fail "T1c: refusal does not name this run's id"
echo "$OUT_B" | grep -q "state archive" \
  || fail "T1d: refusal does not name 'state archive' as the repair"
pass "T1b-d: refusal names both ids and the repair"

[[ "$BEFORE_SHA" == "$AFTER_SHA" ]] \
  || fail "T1e: refused init mutated the manifest"
grep -q "^target_claim_key:" "$STATE1" \
  && fail "T1f: refused init acquired a claim (target_claim_key appeared)" \
  || pass "T1e-f: refusal mutated nothing, acquired no claim"

# ── T2 (AC2-HP): matching fno_id takes today's resume path unchanged ────
log "T2: matching fno_id resumes with today's message and no rewrite"

make_repo TMP2
STATE2="$(manifest_path "$TMP2")"
run_init "$TMP2" "$TMP2/init-a.out" \
  TARGET_SESSION_ID="runR-20260905T000000Z-cl333-cccccc" >/dev/null
[[ -f "$STATE2" ]] || fail "T2: first init did not write the manifest"
BEFORE2="$(shasum "$STATE2" | awk '{print $1}')"
RC_R="$(run_init "$TMP2" "$TMP2/init-resume.out" TARGET_SESSION_ID="runR-20260905T000000Z-cl333-cccccc")"
AFTER2="$(shasum "$STATE2" | awk '{print $1}')"

[[ "$RC_R" == "0" ]] || fail "T2: matching-id resume exited $RC_R; wanted 0"
grep -q "leaving unchanged" "$TMP2/init-resume.out" \
  || fail "T2: resume message changed; wanted 'leaving unchanged' (got: $(cat "$TMP2/init-resume.out"))"
[[ "$BEFORE2" == "$AFTER2" ]] || fail "T2: resume rewrote the manifest"
pass "T2: matching id resumes byte-for-byte as today"

# ── T3 (AC4-EDGE): a manifest with no readable fno_id but a NAMEABLE owner ──
log "T3: id-less manifest naming an unmatched harness session refuses"

make_repo TMP3
STATE3="$(manifest_path "$TMP3")"
mkdir -p "$(dirname "$STATE3")"
printf -- '---\ninput: "legacy manifest predating fno_id"\ncreated_at: 2026-01-01T00:00:00Z\nharness_session_id: transcript-of-someone-else\n---\n\nbody\n' > "$STATE3"
RC_L="$(run_init "$TMP3" "$TMP3/init-legacy.out" TARGET_SESSION_ID="runL-20260906T000000Z-cl444-dddddd")"
OUT_L="$(cat "$TMP3/init-legacy.out" "$TMP3/init-legacy.out.err")"

[[ "$RC_L" != "0" ]] || fail "T3: unmatched named owner was treated as a match (exit 0)"
echo "$OUT_L" | grep -qi "fno_id" \
  || fail "T3: refusal does not name the unreadable fno_id as its reason"
pass "T3: id-less manifest with a named owner refuses (exit $RC_L)"

# ── T4 (AC6-FR): fresh worktree behaves byte-for-byte as today ──────────
log "T4: fresh worktree writes the manifest and exits 0"

make_repo TMP4
STATE4="$(manifest_path "$TMP4")"
RC_F="$(run_init "$TMP4" "$TMP4/init-fresh.out" TARGET_SESSION_ID="runF-20260906T000000Z-cl555-eeeeee")"

[[ "$RC_F" == "0" ]] || fail "T4: fresh init exited $RC_F"
grep -q "session manifest written" "$TMP4/init-fresh.out" \
  || fail "T4: fresh init did not print the write line (got: $(cat "$TMP4/init-fresh.out"))"
[[ -f "$STATE4" ]] || fail "T4: fresh init did not write the manifest"
pass "T4: fresh worktree unchanged by the fix"

# ── T5 (AC3-HP): refusal -> archive -> re-init -> new run owns it ────────
log "T5: following the refusal, archive + re-init hands the slot to the new run"

make_repo TMP5
STATE5="$(manifest_path "$TMP5")"
run_init "$TMP5" "$TMP5/init-a.out" \
  TARGET_SESSION_ID="runA-20260901T000000Z-cl111-aaaaaa" >/dev/null
RC_B5="$(run_init "$TMP5" "$TMP5/init-b.out" \
  TARGET_SESSION_ID="runB-20260906T000000Z-cl222-bbbbbb")"
[[ "$RC_B5" != "0" ]] \
  || fail "T5 precondition: expected the foreign-manifest refusal first (got exit 0; $(cat "$TMP5/init-b.out"))"

[[ -n "$STATE_CLI" ]] || fail "T5a: no fno/fno-py on PATH to run the state archive repair"
ARCH_OUT="$(FNO_SPACES_DIR="$TMP5/spaces" "$STATE_CLI" do state archive --path "$STATE5" 2>&1)" \
  || fail "T5a: state archive failed: $ARCH_OUT"
[[ -f "$STATE5" ]] || pass "T5a: archived manifest moved out of the way"
compgen -G "${STATE5%.md}.archived.*.md" >/dev/null \
  || fail "T5a: no timestamped archive file was created"

RC_C="$(run_init "$TMP5" "$TMP5/init-c.out" TARGET_SESSION_ID="runC-20260906T120000Z-cl666-ffffff")"
[[ "$RC_C" == "0" ]] || fail "T5b: re-init after archive exited $RC_C"
STATE5_NEW="$(manifest_path "$TMP5")"
GOT_ID="$(sed -n 's/^fno_id:[[:space:]]*//p' "$STATE5_NEW" 2>/dev/null | head -1)"
[[ "$GOT_ID" == "runC-20260906T120000Z-cl666-ffffff" ]] \
  || fail "T5c: re-init manifest fno_id is '$GOT_ID'; wanted the new run's id"
pass "T5: archive + re-init gives the new run the slot (fno_id=${GOT_ID})"

# ── T6: same-harness-session rescue; a different harness session refuses ─
log "T6: same transcript re-invocation resumes; a stranger's transcript refuses"

make_repo TMP6
STATE6="$(manifest_path "$TMP6")"
run_init "$TMP6" "$TMP6/init-a.out" \
  CLAUDE_CODE_SESSION_ID="transcript-alpha" >/dev/null
[[ -f "$STATE6" ]] || fail "T6: first init did not write the manifest"

RC_S="$(run_init "$TMP6" "$TMP6/init-same.out" CLAUDE_CODE_SESSION_ID="transcript-alpha")"
[[ "$RC_S" == "0" ]] || fail "T6a: same-transcript re-init exited $RC_S; wanted today's resume (0)"
grep -q "leaving unchanged" "$TMP6/init-same.out" \
  || fail "T6a: same-transcript re-init message changed"
pass "T6a: same-transcript re-invocation keeps the resume path"

RC_X="$(run_init "$TMP6" "$TMP6/init-stranger.out" CLAUDE_CODE_SESSION_ID="transcript-beta")"
[[ "$RC_X" != "0" ]] || fail "T6b: a different transcript was waved through (exit 0)"
pass "T6b: a different harness session is refused (exit $RC_X)"

# ── T7: a CORRUPT manifest keeps today's self-healing ───────────────────
# Only a parseable manifest can prove or refuse ownership. Garbage bytes
# refuse nothing: the corrupt-archive path archives them and writes fresh.
log "T7: corrupt manifest auto-archives and writes fresh (today's behavior)"

make_repo TMP7
STATE7="$(manifest_path "$TMP7")"
mkdir -p "$(dirname "$STATE7")"
printf 'not frontmatter at all\n' > "$STATE7"
RC_G="$(run_init "$TMP7" "$TMP7/init-garbage.out" TARGET_SESSION_ID="runG-20260906T000000Z-cl777-ggggggg")"

[[ "$RC_G" == "0" ]] || fail "T7: corrupt manifest exited $RC_G; wanted today's self-healing (0)"
compgen -G "${STATE7%.md}.corrupt.*.md" >/dev/null \
  || fail "T7: corrupt manifest was not archived to the corrupt path"
GOT_G="$(sed -n 's/^fno_id:[[:space:]]*//p' "$STATE7" 2>/dev/null | head -1)"
[[ "$GOT_G" == "runG-20260906T000000Z-cl777-ggggggg" ]] \
  || fail "T7: fresh manifest fno_id is '$GOT_G'; wanted the new run's id"
pass "T7: corrupt file self-heals; fresh manifest names the new run"

# ── T8: anonymous manifest + anonymous caller keeps today's resume ──────
# A manifest that names no id and a caller that carries none prove nothing in
# either direction; that pair preserves (the standing resume contract), and a
# caller WITH identity still refuses (T3).
log "T8: anonymous manifest, anonymous caller -> resume"

make_repo TMP8
STATE8="$(manifest_path "$TMP8")"
mkdir -p "$(dirname "$STATE8")"
printf -- '---\nsession_id: preexisting\ninput: "anonymous manifest"\n---\n\nbody\n' > "$STATE8"
RC_A="$(run_init "$TMP8" "$TMP8/init-anon.out")"
[[ "$RC_A" == "0" ]] || fail "T8: anonymous pair exited $RC_A; wanted today's resume (0)"
grep -q "leaving unchanged" "$TMP8/init-anon.out" \
  || fail "T8: anonymous pair message changed; got: $(cat "$TMP8/init-anon.out")"
pass "T8: anonymous manifest and caller preserve today's resume"

log "All foreign-manifest scenarios passed"
