#!/usr/bin/env bash
# tests/ci/test_preflight.sh
#
# Exercises scripts/ci/preflight.sh orchestration in a throwaway git repo with
# stub smoke.sh/cargo/rustup/fno on PATH, so no real 45-step suite or cargo
# build runs. Covers AC2-HP (catches a CI-red commit locally), AC2-ERR (dirty
# tree refused), AC2-EDGE (concurrent -> exit 3 + holder), AC1-FR (interrupt
# recovery: stale lock is stealable), plus the shared-worktree safety net:
# exactly one winner when racers steal the same dead lock, and a VOID (never a
# GREEN/RED) when either the worktree or the lock changes hands mid-run.
#
# Also covers the SHA+host-bound attestation reuse: a second call on an attested
# SHA exits 0 without the lock (AC1-HP/AC1-FR), --force discards it (AC2-FR), a
# dirty tree still refuses (AC1-EDGE), a foreign host and a corrupt/empty file
# degrade to a full run (AC3-EDGE/AC2-EDGE), a subset pass mints nothing
# (AC1-ERR), a RED deletes a matching one (AC3-ERR), and a VOID leaves a prior
# one untouched (AC2-ERR). The carrier is one slot per candidate SHA: a green on
# another SHA never erases this one's, and aged slots plus the legacy
# single-file carrier are reaped at write time.

set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
PREFLIGHT_SRC="$REPO_ROOT/scripts/ci/preflight.sh"
# The fixture's preflight falls back to bare python3, whose only route to the
# fno package is this path; the smoke runner exports it for its steps, so a
# bare `bash tests/ci/test_preflight.sh` must export it itself or every
# receipt write dies with ModuleNotFoundError.
export PYTHONPATH="$REPO_ROOT/cli/src${PYTHONPATH:+:$PYTHONPATH}"
# pwd -P: resolve macOS /var -> /private/var so `git worktree list` paths match.
TMP="$(cd "$(mktemp -d)" && pwd -P)"
trap 'rm -rf "$TMP"' EXIT

FAILS=0
ok()   { echo "  ok: $1"; }
fail() { echo "  FAIL: $1"; FAILS=$((FAILS+1)); }

# --- build stub tool dir ----------------------------------------------------
BIN="$TMP/bin"; mkdir -p "$BIN"
WT_BASE="$TMP/wtbase"; mkdir -p "$WT_BASE"
GLOBAL_EVENTS="$TMP/global-events.jsonl"
: > "$GLOBAL_EVENTS"

cat > "$BIN/fno" <<EOF
#!/usr/bin/env bash
# STUB_FNO_SLOW (committed only by the FIFO test) widens the window between a
# waiter acquiring the lock and finishing its run, so ticket order is observable.
[[ -f STUB_FNO_SLOW ]] && sleep 4
if [[ "\$*" == *"paths.worktrees_base"* ]]; then
    echo "$WT_BASE"
    exit 0
fi
if [[ "\${1:-} \${2:-}" == "pr next-receipt-generation" ]]; then
    shift 2
    sha=''
    while [[ \$# -gt 0 ]]; do
        case "\$1" in
            --candidate-sha) sha="\$2"; shift 2 ;;
            *) shift ;;
        esac
    done
    if [[ -s "$GLOBAL_EVENTS" ]]; then
        jq -sr --arg sha "\$sha" \
            '[.[] | select(.type == "verification_receipt" and .data.candidate_sha == \$sha) | .data.generation] | (max // 0) + 1' \
            "$GLOBAL_EVENTS"
    else
        echo 1
    fi
    exit 0
fi
if [[ "\${1:-} \${2:-}" == "pr global-receipt-events-path" ]]; then
    echo "$GLOBAL_EVENTS"
    exit 0
fi
if [[ "\${1:-} \${2:-}" == "pr evidence-check" ]]; then
    sha="\$(git rev-parse HEAD)"
    events="$GLOBAL_EVENTS"
    [[ -s "\$events" ]] || exit 1
    jq -se --arg sha "\$sha" \
        '[.[] | select(.type == "verification_receipt" and .data.candidate_sha == \$sha)] | sort_by(.ts) | last | .data.mode == "full" and .data.result == "passed"' \
        "\$events" >/dev/null
    exit \$?
fi
exit 0
EOF
cat > "$BIN/cargo" <<'EOF'
#!/usr/bin/env bash
# stub cargo: drop a leading +toolchain, succeed on fmt/test
[[ "${1:-}" == +* ]] && shift
if [[ "${1:-}" == "audit" ]]; then
    [[ -n "${PREFLIGHT_AUDIT_LOG:-}" ]] && printf '%s\n' "$PWD" >> "$PREFLIGHT_AUDIT_LOG"
    if [[ "${PREFLIGHT_TEST_FAIL_AUDIT:-0}" == "1" && "$PWD" == */fno-agents ]]; then
        exit 1
    fi
fi
if [[ "${PREFLIGHT_TEST_FAIL_FNO_TEST:-0}" == "1" && "${1:-}" == "test" && "$PWD" == */fno ]]; then
    echo "stub: cargo test (fno) forced red" >&2
    exit 1
fi
exit 0
EOF
cat > "$BIN/cargo-audit" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
cat > "$BIN/rustup" <<'EOF'
#!/usr/bin/env bash
[[ "$*" == "toolchain list"* ]] && { echo "1.94.1-x86_64-apple-darwin (default)"; exit 0; }
exit 0
EOF
cat > "$BIN/mkdir" <<'EOF'
#!/usr/bin/env bash
/bin/mkdir "$@"
rc=$?
last="${!#}"
if [[ $rc -eq 0 && "${PREFLIGHT_TEST_SIGNAL_LOCK:-0}" == "1" \
      && "$last" == */.preflight-receipt-locks/*.d ]]; then
    kill -TERM "$PPID"
fi
exit "$rc"
EOF
# preflight calls `uv run --project cli fno-py test smoke [flags]`; stub uv to
# behave like the retired smoke.sh stub (red iff POISON is checked out).
# The changed packet is a distinct invocation (--changed) and must be stubbable
# independently of the full run: several cases below turn one red while the
# other stays green, which is the whole ordering contract.
cat > "$BIN/uv" <<'EOF'
#!/usr/bin/env bash
case " $* " in
  *" --changed "*)
    [[ -f CHANGED_PREREQ ]] && { echo "smoke: missing prerequisite: uv"; exit 22; }
    [[ -f CHANGED_UNEVAL ]] && { echo "smoke: CHANGED SUBSET UNEVALUATED"; exit 21; }
    [[ -f CHANGED_NONE ]]   && { echo "smoke: CHANGED SUBSET selected NOTHING"; exit 20; }
    [[ -f CHANGED_POISON ]] && { echo "smoke: CHANGED SUBSET verdict=red"; exit 1; }
    echo "smoke: CHANGED SUBSET verdict=green"; exit 0 ;;
esac
if [[ -f POISON ]]; then echo "smoke: POISON step failed"; exit 1; fi
echo "smoke: all green (stub)"; exit 0
EOF
chmod +x "$BIN/fno" "$BIN/cargo" "$BIN/cargo-audit" "$BIN/rustup" "$BIN/mkdir" "$BIN/uv"
export PATH="$BIN:$PATH"

# --- build the fixture repo -------------------------------------------------
FIX="$TMP/repo"; mkdir -p "$FIX/scripts/ci" "$FIX/scripts/lib" "$FIX/cli/src/fno/events"
git -C "$FIX" init -q
git -C "$FIX" config user.email t@t.t; git -C "$FIX" config user.name t
cp "$PREFLIGHT_SRC" "$FIX/scripts/ci/preflight.sh"
cp "$REPO_ROOT/scripts/lib/events-validate.sh" "$FIX/scripts/lib/events-validate.sh"
cp "$REPO_ROOT/cli/src/fno/events/schema.yaml" "$FIX/cli/src/fno/events/schema.yaml"
echo '.fno/' > "$FIX/.gitignore"
# crate dirs so preflight's `cd crates/fno*` legs run (cargo is stubbed).
mkdir -p "$FIX/crates/fno-agents" "$FIX/crates/fno"
echo x > "$FIX/crates/fno-agents/.keep"; echo x > "$FIX/crates/fno/.keep"
git -C "$FIX" add -A; git -C "$FIX" commit -qm "green base"
GREEN_SHA="$(git -C "$FIX" rev-parse --short HEAD)"
GREEN_FULL="$(git -C "$FIX" rev-parse HEAD)"
# The changed packet resolves its base as merge-base origin/main; without this
# ref preflight skips the leg, so the fixture would never exercise it.
git -C "$FIX" update-ref refs/remotes/origin/main "$GREEN_FULL"
# The fixture is a plain repo, so its git-common-dir is $FIX/.git; the lock and
# the attestation slots are siblings under it.
LOCKDIR="$FIX/.git/.preflight.lock.d"
ATT_DIR="$FIX/.git/.preflight-attestations.d"
EVENTS="$FIX/.fno/events.jsonl"
export PREFLIGHT_AUDIT_LOG="$TMP/audit.log"
HOST="$(hostname 2>/dev/null || echo unknown)"
# One slot per candidate SHA; the fixture HEAD moves across sections, so resolve
# the current slot at use time, never once up front.
cur_att() { printf '%s/%s\n' "$ATT_DIR" "$(git -C "$FIX" rev-parse HEAD)"; }
# Plant an attestation line for a given full SHA (defaulting to this host).
write_attest() { printf 'sha=%s mode=FULL verdict=green at=%s iso=now host=%s pid=4242\n' "$1" "$(date +%s)" "${2:-$HOST}" > "$ATT_DIR/$1"; }

run_pf() { ( cd "$FIX" && bash scripts/ci/preflight.sh "$@" ); }

echo "== AC2-HP-green: clean HEAD, smoke green, rust stubs green -> exit 0 =="
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "exit 0 on green" || fail "expected 0 got $rc: $out"
grep <<<"$out" -q "GREEN - safe to push" && ok "reports GREEN" || fail "no GREEN line"
grep <<<"$out" -q "cargo fmt --check (fno-agents" && ok "fmt leg in summary (AC3-HP)" || fail "no fmt leg"
grep <<<"$out" -q "cargo test --all-targets (fno-agents)" && ok "cargo test leg in summary (AC3-HP)" || fail "no test leg"
grep <<<"$out" -q "ADVISORY" && ok "audit ADVISORY row present" || fail "no ADVISORY row"
jq -se --arg sha "$GREEN_FULL" \
    '[.[] | select(.type == "verification_receipt" and .data.candidate_sha == $sha)] | last | .data.mode == "full" and .data.result == "passed" and .data.generation >= 1' \
    "$EVENTS" >/dev/null \
    && ok "full green emits exact-SHA full/passed evidence" \
    || fail "missing exact-SHA full/passed event receipt"
jq -se --arg sha "$GREEN_FULL" \
    '[.[] | select(.type == "verification_receipt" and .data.candidate_sha == $sha)] | last | .data.result == "passed"' \
    "$GLOBAL_EVENTS" >/dev/null \
    && ok "full green mirrors evidence to the global journal" \
    || fail "missing global exact-SHA receipt"
jq -se --arg sha "$GREEN_FULL" \
    '[.[] | select(.type == "verification_receipt" and .data.candidate_sha == $sha)] as $r
     | any($r[]; .data.result == "pending")
       and (($r | map(.data.generation) | max) == ($r | last | .data.generation))' \
    "$GLOBAL_EVENTS" >/dev/null \
    && ok "canonical pending receipt precedes the final verdict" \
    || fail "missing canonical pending-to-final transition"

echo "== attestation: a FULL GREEN records one (sha + host pinned) =="
[[ -f "$(cur_att)" ]] && ok "attestation written on full green" || fail "no attestation file after green"
grep -q "^sha=$GREEN_FULL " "$(cur_att)" && ok "attestation pins the full candidate SHA" || fail "attestation sha wrong: $(cat "$(cur_att)")"
grep -q " host=$HOST" "$(cur_att)" && ok "attestation pins this host" || fail "attestation host wrong: $(cat "$(cur_att)")"

echo "== global authority: a failed delivery-root mirror stays GREEN everywhere =="
mv "$EVENTS" "$EVENTS.saved"
mkdir "$EVENTS"
out="$(run_pf --force 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "global commit remains authoritative" || fail "mirror failure changed verdict rc=$rc: $out"
grep <<<"$out" -q "delivery-root mirror unavailable" \
    && ok "mirror failure is observable" || fail "mirror failure was silent"
( cd "$FIX" && fno pr evidence-check >/dev/null 2>&1 ) \
    && ok "canonical global receipt satisfies the producing checkout" \
    || fail "producing checkout rejected canonical evidence"
rm -rf "$EVENTS"
mv "$EVENTS.saved" "$EVENTS"

echo "== AC1-HP: a second call on the attested SHA reuses (exit 0, no lock) =="
rm -rf "$LOCKDIR"   # a cache hit must create no lock
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "reuse exits 0" || fail "expected 0 got $rc: $out"
grep <<<"$out" -q "reused attestation" && ok "prints reuse receipt" || fail "no reuse receipt: $out"
grep <<<"$out" -q "candidate=$GREEN_SHA" && ok "receipt names the matched SHA" || fail "receipt omits candidate"
grep <<<"$out" -qE "earned=.*ago" && ok "receipt reports the attestation age" || fail "receipt omits age: $out"
grep <<<"$out" -q "host=$HOST" && ok "receipt reports the earning host" || fail "receipt omits host"
[[ ! -d "$LOCKDIR" ]] && ok "a cache hit created no lock directory" || fail "reuse took the lock"

echo "== AC1-FR: a cache hit does not contend for a held lock =="
mkdir -p "$LOCKDIR"; printf 'pid=%s started=NOW host=x sha=deadbee\n' "$$" > "$LOCKDIR/holder"  # live pid
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "reuse exit 0 despite a live lock holder" || fail "expected 0 got $rc: $out"
grep <<<"$out" -q "reused attestation" && ok "satisfied by the cache, not the lock" || fail "did not reuse under a held lock"
grep -q "pid=$$" "$LOCKDIR/holder" && ok "the held lock was left untouched" || fail "reuse clobbered the live lock"
rm -rf "$LOCKDIR"

echo "== per-SHA carriers: a green on another SHA never erases this SHA's slot =="
# The regression this file guards: under a single shared carrier, worktree B's
# record_attestation overwrote worktree A's green, so the fleet-wide hit rate
# degraded to 1/N across concurrently active SHAs.
( cd "$FIX" && git commit -q --allow-empty -m "second candidate SHA" )
SHA_B="$(git -C "$FIX" rev-parse HEAD)"
git -C "$FIX" update-ref refs/remotes/origin/main "$SHA_B"
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "the second SHA runs green" || fail "second SHA red rc=$rc: $out"
[[ -f "$ATT_DIR/$GREEN_FULL" && -f "$ATT_DIR/$SHA_B" ]] \
    && ok "both SHA slots coexist" || fail "slots missing after two greens on different SHAs"
# Back on the first SHA: its green must still be reusable (the shared carrier
# erased it; the per-SHA slot keeps it).
git -C "$FIX" reset -q --hard "$GREEN_FULL"
git -C "$FIX" update-ref refs/remotes/origin/main "$GREEN_FULL"
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "the first SHA reuses after another SHA's green" || fail "expected 0 got $rc: $out"
grep <<<"$out" -q "reused attestation" && ok "reuse receipt on the first SHA" || fail "the first SHA missed its own slot: $out"

echo "== cache bound: write-time reap removes aged slots and the legacy carrier =="
# Reap is by mtime at record time: content is irrelevant to the reaper, so the
# planted slot carries a dummy sha and an ancient timestamp.
mkdir -p "$ATT_DIR"
printf 'sha=dead mode=FULL verdict=green at=0 iso=old host=%s pid=1\n' "$HOST" \
    > "$ATT_DIR/deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
touch -t 202001010000 "$ATT_DIR/deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
printf 'sha=legacy mode=FULL verdict=green at=0 iso=old host=%s pid=1\n' "$HOST" \
    > "$FIX/.git/.preflight-attestation"
out="$(run_pf --force 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "green run after planting aged residue" || fail "expected 0 got $rc: $out"
[[ ! -e "$ATT_DIR/deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef" ]] \
    && ok "the aged slot was reaped at write time" || fail "aged slot survived the reap"
[[ ! -e "$FIX/.git/.preflight-attestation" ]] \
    && ok "the legacy single-file carrier was removed" || fail "legacy carrier survived"
[[ -f "$(cur_att)" ]] && ok "the fresh slot survived its own reap" || fail "reap deleted the slot it just wrote"
rm -f "$FIX/.git/.preflight-attestation"

echo "== AC2-FR: --force always discards the attestation and re-runs =="
out="$(run_pf --force 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "--force re-runs to GREEN" || fail "expected 0 got $rc: $out"
grep <<<"$out" -q "reused attestation" && fail "--force printed a reuse receipt" || ok "--force did not reuse"

echo "== AC1-EDGE: a dirty tree still refuses, attestation or not =="
( cd "$FIX" && echo dirt > dirty.txt )
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 4 ]] && ok "exit 4 on dirty even with a valid attestation" || fail "expected 4 got $rc"
grep <<<"$out" -q "reused attestation" && fail "dirty tree printed a reuse GREEN" || ok "dirty tree never reuses"
( cd "$FIX" && rm -f dirty.txt )

echo "== advisory evidence: both audit scopes execute even when the first fails =="
: > "$PREFLIGHT_AUDIT_LOG"
out="$(PREFLIGHT_TEST_FAIL_AUDIT=1 run_pf --force 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "advisory audit failure stays non-blocking" || fail "expected green got $rc: $out"
[[ "$(wc -l < "$PREFLIGHT_AUDIT_LOG" | tr -d ' ')" == "2" ]] \
    && ok "both audit commands executed" || fail "audit short-circuited: $(cat "$PREFLIGHT_AUDIT_LOG")"
jq -se --arg sha "$GREEN_FULL" \
    '[.[] | select(.type == "verification_receipt" and .data.candidate_sha == $sha and .data.mode == "advisory")] | last | .data.result == "failed" and .data.steps_executed == 2' \
    "$EVENTS" >/dev/null \
    && ok "advisory receipt records failed with two actual executions" \
    || fail "advisory receipt execution count/result wrong"

echo "== AC3-EDGE: an attestation from another host is rejected =="
write_attest "$GREEN_FULL" foreign-box
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "still GREEN after rejecting the foreign attestation" || fail "expected 0 got $rc: $out"
grep <<<"$out" -q "foreign host" && ok "receipt states the attestation was rejected as foreign" || fail "no foreign-host line: $out"
grep <<<"$out" -q "reused attestation" && fail "reused a foreign attestation" || ok "did not reuse the foreign attestation"

echo "== AC2-EDGE: a corrupt / empty attestation degrades to a full run =="
printf 'sha=not-even-a-sha garbage\n' > "$(cur_att)"
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "unparseable attestation -> full run -> GREEN" || fail "expected 0 got $rc"
grep <<<"$out" -q "reused attestation" && fail "trusted an unparseable attestation" || ok "unparseable attestation not trusted"
: > "$(cur_att)"   # empty file
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "empty attestation -> full run -> GREEN" || fail "expected 0 got $rc"
grep <<<"$out" -q "reused attestation" && fail "trusted an empty attestation" || ok "empty attestation not trusted"

echo "== AC2-EDGEb: a non-FULL / non-green attestation degrades to a full run =="
printf 'sha=%s mode=FULL verdict=red at=%s iso=now host=%s pid=4242\n' "$GREEN_FULL" "$(date +%s)" "$HOST" > "$(cur_att)"
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "non-green attestation -> full run -> GREEN" || fail "expected 0 got $rc"
grep <<<"$out" -q "reused attestation" && fail "trusted a non-green attestation" || ok "non-green attestation not trusted"

echo "== AC1-ERR: a --retry-failed (subset) pass mints no FULL attestation; reuse then full-runs =="
rm -f "$(cur_att)"
LEGREC="$FIX/.fno/preflight-last-failed-legs.txt"
printf 'rustfmt:fno\n' > "$LEGREC"
out="$(run_pf --retry-failed 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "retry-failed subset passes" || fail "expected 0 got $rc: $out"
[[ ! -f "$(cur_att)" ]] && ok "subset run wrote no attestation" || fail "subset run minted a full-run attestation"
jq -se --arg sha "$GREEN_FULL" \
    '[.[] | select(.type == "verification_receipt" and .data.candidate_sha == $sha)] | last | .data.mode == "subset" and .data.result == "passed"' \
    "$EVENTS" >/dev/null \
    && ok "subset run records subset evidence distinctly" \
    || fail "subset run did not emit subset/passed evidence"
# The load-bearing half: a subsequent caller on the same SHA finds no FULL
# attestation, so reuse MISSES and it full-runs (a subset green can never
# satisfy the gate).
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "subsequent full run passes" || fail "expected 0 got $rc: $out"
grep <<<"$out" -q "reused attestation" && fail "reused a subset-only green" || ok "no FULL attestation to reuse -> full run"

echo "== leg record: a cargo-only RED records its leg scope alone =="
rm -f "$(cur_att)"
out="$(PREFLIGHT_TEST_FAIL_FNO_TEST=1 run_pf --force 2>&1)"; rc=$?
[[ $rc -ne 0 ]] && ok "cargo-only red exits non-zero" || fail "expected red got $rc: $out"
[[ -f "$LEGREC" ]] && ok "RED wrote the leg record" || fail "no leg record after RED"
[[ "$(cat "$LEGREC")" == "cargo-test:fno" ]] && ok "record names exactly cargo-test:fno" \
    || fail "record wrong: $(cat "$LEGREC")"

echo "== --retry-failed honors the leg record: smoke skipped, only the red leg re-runs =="
rm -f "$(cur_att)"
out="$(run_pf --retry-failed 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "leg-scoped retry passes" || fail "expected 0 got $rc: $out"
grep <<<"$out" -q "smoke suite (skipped - not in the retry leg record)" \
    && ok "smoke leg skipped" || fail "smoke leg ran: $out"
grep <<<"$out" -q "cargo test --all-targets (fno-agents) (skipped - not in the retry leg record)" \
    && ok "untouched cargo leg skipped" || fail "fno-agents leg ran: $out"
grep <<<"$out" -q "=== cargo test --all-targets (fno) ===" \
    && ok "the failed leg re-ran" || fail "failed leg did not run: $out"
[[ ! -f "$(cur_att)" ]] && ok "leg-scoped subset mints no attestation" || fail "subset minted an attestation"
jq -se --arg sha "$(git -C "$FIX" rev-parse HEAD)" \
    '[.[] | select(.type == "verification_receipt" and .data.candidate_sha == $sha)] | last | .data.mode == "subset" and .data.steps_executed < .data.steps_expected' \
    "$EVENTS" >/dev/null \
    && ok "leg-scoped retry records subset with partial coverage" \
    || fail "leg-scoped retry receipt is not a partial-coverage subset"
[[ ! -s "$LEGREC" ]] && ok "green retry truncated the record" || fail "record not truncated: $(cat "$LEGREC")"

echo "== fallback: a missing leg record runs every leg and earns FULL =="
rm -f "$(cur_att)" "$LEGREC"
out="$(run_pf --retry-failed 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "fallback retry passes" || fail "expected 0 got $rc: $out"
[[ -f "$(cur_att)" ]] && ok "fallback retry minted the FULL attestation it earned" || fail "no attestation after full-coverage retry"
jq -se --arg sha "$(git -C "$FIX" rev-parse HEAD)" \
    '[.[] | select(.type == "verification_receipt" and .data.candidate_sha == $sha)] | last | .data.mode == "full" and .data.steps_executed == .data.steps_expected' \
    "$EVENTS" >/dev/null \
    && ok "fallback retry receipt is full with equal coverage" \
    || fail "fallback retry receipt is not full"

echo "== corrupt record: unrecognized names run every leg =="
printf 'not-a-leg\n' > "$LEGREC"
rm -f "$(cur_att)"
out="$(run_pf --retry-failed 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "corrupt record falls back to every leg" || fail "expected 0 got $rc: $out"
grep <<<"$out" -q "=== smoke suite" && ok "smoke ran under a corrupt record" || fail "smoke skipped on a corrupt record"
rm -f "$LEGREC"

echo "== AC3-ERR: a RED run deletes a matching attestation =="
( cd "$FIX" && touch POISON && git add -A && git commit -qm "poison for AC3-ERR" )
write_attest "$(git -C "$FIX" rev-parse HEAD)"   # plant a stale green for the RED sha
out="$(run_pf --force 2>&1)"; rc=$?
[[ $rc -ne 0 ]] && ok "RED run exits non-zero" || fail "expected red got $rc"
grep <<<"$out" -q "RED - fix" && ok "reports RED" || fail "no RED line: $out"
[[ ! -f "$(cur_att)" ]] && ok "RED deleted the matching attestation" || fail "RED left a stale green attestation"
( cd "$FIX" && git rm -q POISON && git commit -qm "unpoison AC3-ERR" )
rm -f "$(cur_att)"

echo "== AC2-HP-red: a POISON commit is caught locally, exit non-zero, no push =="
( cd "$FIX" && touch POISON && git add -A && git commit -qm "poisoned" )
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -ne 0 ]] && ok "exit non-zero on red" || fail "expected non-zero got $rc"
grep <<<"$out" -q "RED - fix" && ok "reports RED" || fail "no RED line"
grep <<<"$out" -q "fail.*smoke suite" && ok "smoke suite marked fail" || fail "smoke not failed in summary"
# back to green for remaining tests
( cd "$FIX" && git rm -q POISON && git commit -qm "unpoison" )

echo "== changed packet: a red packet stops the run BEFORE the full gate =="
rm -f "$(cur_att)"
( cd "$FIX" && touch CHANGED_POISON && git add -A && git commit -qm "changed-packet red" )
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 1 ]] && ok "exit 1 on a red changed packet" || fail "expected 1 got $rc: $out"
grep <<<"$out" -q "RED (changed packet)" && ok "names the changed packet as the cause" || fail "no changed-packet RED line: $out"
grep <<<"$out" -q "all green (stub)" && fail "full smoke ran after a red changed packet" || ok "full gate not started (earliest signal)"
grep <<<"$out" -q "the full gate has NOT run" && ok "says the full gate did not run" || fail "no full-gate caveat"
[[ ! -f "$(cur_att)" ]] && ok "a red changed packet mints no attestation" || fail "changed packet wrote an attestation"
( cd "$FIX" && git rm -q CHANGED_POISON && git commit -qm "unpoison changed" )

echo "== AC7: a green changed packet cannot rescue a red full gate =="
rm -f "$(cur_att)"
( cd "$FIX" && touch POISON && git add -A && git commit -qm "full red, changed green" )
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -ne 0 ]] && ok "exit non-zero when an unselected full step fails" || fail "expected non-zero got $rc"
grep <<<"$out" -q "pass.*changed packet (CHANGED SUBSET)" && ok "changed packet passed" || fail "changed leg not green: $out"
grep <<<"$out" -q "fail.*smoke suite" && ok "full smoke marked fail" || fail "full smoke not failed"
[[ ! -f "$(cur_att)" ]] && ok "no FULL attestation minted (AC7)" || fail "minted a full attestation on a red run"
( cd "$FIX" && git rm -q POISON && git commit -qm "unpoison full" )

echo "== AC4/AC5: unselected and unevaluated packets fall through to the full gate =="
for sentinel in CHANGED_NONE CHANGED_UNEVAL; do
    rm -f "$(cur_att)"
    ( cd "$FIX" && touch "$sentinel" && git add -A && git commit -qm "$sentinel" )
    out="$(run_pf 2>&1)"; rc=$?
    [[ $rc -eq 0 ]] && ok "$sentinel: full gate ran and passed" || fail "$sentinel: expected 0 got $rc: $out"
    grep <<<"$out" -q "all green (stub)" && ok "$sentinel: full smoke still ran" || fail "$sentinel: full smoke skipped"
    grep <<<"$out" -q "GREEN - safe to push" && ok "$sentinel: verdict came from the full gate" || fail "$sentinel: no GREEN"
    ( cd "$FIX" && git rm -q "$sentinel" && git commit -qm "drop $sentinel" )
done
grep <<<"$out" -q "UNEVALUATED" && ok "unevaluated state is stated, not swallowed" || fail "no UNEVALUATED note"

echo "== a missing prerequisite keeps preflight's documented exit 2 =="
rm -f "$(cur_att)"
( cd "$FIX" && touch CHANGED_PREREQ && git add -A && git commit -qm "changed prereq missing" )
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 2 ]] && ok "exit 2 (not 1) when the packet cannot run" || fail "expected 2 got $rc: $out"
grep <<<"$out" -q "RED (changed packet)" && fail "reported a test failure for a prerequisite gap" \
    || ok "not reported as a suite failure"
( cd "$FIX" && git rm -q CHANGED_PREREQ && git commit -qm "drop prereq sentinel" )

echo "== --retry-failed skips the changed packet (a different subset mode) =="
rm -f "$(cur_att)"
out="$(run_pf --retry-failed 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "retry-failed still passes" || fail "expected 0 got $rc"
grep <<<"$out" -q "changed packet" && fail "retry-failed ran the changed packet" || ok "changed packet skipped"
rm -f "$(cur_att)"

echo "== AC2-ERR: dirty invoking tree refused (exit 4), nothing touched =="
( cd "$FIX" && echo dirt > dirty.txt )
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 4 ]] && ok "exit 4 on dirty" || fail "expected 4 got $rc"
grep <<<"$out" -q "dirty.txt" && ok "lists the dirty file" || fail "did not list dirty file"
[[ ! -d "$WT_BASE/repo/preflight" ]] || { [[ -z "$(ls -A "$WT_BASE/repo/preflight" 2>/dev/null)" ]] && ok "no worktree materialized on refusal" || ok "worktree pre-existed (from green run) - refusal touched nothing"; }
( cd "$FIX" && rm -f dirty.txt )

echo "== AC2-EDGE: concurrent invocation (--wait-timeout 0) -> exit 3 with holder =="
mkdir -p "$LOCKDIR"; printf 'pid=%s started=NOW host=x sha=deadbee\n' "$$" > "$LOCKDIR/holder"  # $$ is alive
out="$(run_pf --wait-timeout 0 2>&1)"; rc=$?
[[ $rc -eq 3 ]] && ok "exit 3 when lock held by a live pid" || fail "expected 3 got $rc"
grep <<<"$out" -q "lock held" && ok "prints holder info" || fail "no holder info"
grep <<<"$out" -q "FNO_SKIP_PREFLIGHT" && ok "immediate fail carries the skip hint" || fail "no skip hint"
rm -rf "$LOCKDIR"

echo "== FIFO queue: waiters are served in arrival order, not by chance =="
rm -f "$(cur_att)"
( cd "$FIX" && touch STUB_FNO_SLOW && git add -A && git commit -qm "slow stub sentinel" )
mkdir -p "$LOCKDIR"
sleep 600 & fifo_holder=$!
printf 'pid=%s started=%s host=x sha=deadbee\n' "$fifo_holder" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$LOCKDIR/holder"
run_pf --wait-timeout 90 >/dev/null 2>&1 & w1=$!
t1=""
for _i in $(seq 1 100); do
    t1="$(ls "$LOCKDIR.queue.d" 2>/dev/null | sed -n '1p')"
    [[ -n "$t1" ]] && break
    sleep 0.2
done
[[ -n "$t1" ]] && ok "first waiter queued a ticket" || fail "first waiter never queued"
run_pf --wait-timeout 90 >/dev/null 2>&1 & w2=$!
t2=""
for _i in $(seq 1 100); do
    [[ "$(ls "$LOCKDIR.queue.d" 2>/dev/null | wc -l | tr -d ' ')" -ge 2 ]] && break
    sleep 0.2
done
t2="$(ls "$LOCKDIR.queue.d" 2>/dev/null | sed -n '2p')"
[[ -n "$t2" ]] && ok "second waiter queued behind the first" || fail "second waiter never queued"
rm -rf "$LOCKDIR"   # release: only the front ticket may take it next
w1_served_first=0
for _i in $(seq 1 100); do
    if [[ ! -d "$LOCKDIR.queue.d/$t1" ]]; then w1_served_first=1; break; fi
    sleep 0.1
done
if [[ $w1_served_first -eq 1 && -d "$LOCKDIR.queue.d/$t2" && -d "$LOCKDIR" ]]; then
    ok "arrival order held: first waiter owns the lock while the second still queues"
else
    fail "FIFO violated (w1_served=$w1_served_first t2_queued=$([[ -d "$LOCKDIR.queue.d/$t2" ]] && echo yes || echo no))"
fi
wait "$w1"; r1=$?
wait "$w2"; r2=$?
[[ $r1 -eq 0 && $r2 -eq 0 ]] && ok "both waiters ran to GREEN after queueing" || fail "waiter outcomes r1=$r1 r2=$r2"
[[ -z "$(ls -A "$LOCKDIR.queue.d" 2>/dev/null)" ]] && ok "queue drained clean" || fail "queue leftovers: $(ls -A "$LOCKDIR.queue.d" 2>/dev/null)"
kill "$fifo_holder" 2>/dev/null; wait "$fifo_holder" 2>/dev/null
rm -rf "$LOCKDIR" "$LOCKDIR.queue.d"
( cd "$FIX" && git rm -q STUB_FNO_SLOW && git commit -qm "drop slow stub sentinel" )

echo "== FIFO queue: --wait-timeout expiry exits 3 and cleans up its ticket =="
mkdir -p "$LOCKDIR"; printf 'pid=%s started=NOW host=x sha=deadbee\n' "$$" > "$LOCKDIR/holder"
out="$(run_pf --wait-timeout 4 2>&1)"; rc=$?
[[ $rc -eq 3 ]] && ok "expired wait exits 3" || fail "expected 3 got $rc: $out"
grep <<<"$out" -q "gave up waiting after 4s" && ok "names the give-up" || fail "no give-up line: $out"
grep <<<"$out" -q "FNO_SKIP_PREFLIGHT" && ok "waiting output carries the skip hint" || fail "no skip hint: $out"
[[ -z "$(ls -A "$LOCKDIR.queue.d" 2>/dev/null)" ]] && ok "ticket removed on give-up" || fail "ticket left behind: $(ls -A "$LOCKDIR.queue.d" 2>/dev/null)"
rm -rf "$LOCKDIR" "$LOCKDIR.queue.d"

echo "== FIFO queue: a newcomer allocates above surviving tickets, never a dequeued hole =="
mkdir -p "$LOCKDIR" "$LOCKDIR.queue.d/000002"
printf 'pid=%s started=NOW host=x sha=deadbee\n' "$$" > "$LOCKDIR/holder"
printf 'pid=%s started=NOW host=x sha=deadbee\n' "$$" > "$LOCKDIR.queue.d/000002/holder"
run_pf --wait-timeout 6 >/dev/null 2>&1 & mono_w=$!
new_ticket=""
for _i in $(seq 1 60); do
    new_ticket="$(ls "$LOCKDIR.queue.d" 2>/dev/null | sed -n '2p')"
    [[ -n "$new_ticket" ]] && break
    sleep 0.2
done
[[ "$new_ticket" == "000003" ]] && ok "newcomer allocated 000003 above the surviving 000002" || fail "allocation wrong: $new_ticket"
[[ ! -d "$LOCKDIR.queue.d/000001" ]] && ok "the dequeued 000001 hole was not reused" || fail "000001 reused ahead of 000002"
wait "$mono_w"; rc=$?
[[ $rc -eq 3 ]] && ok "the newcomer gave up cleanly" || fail "expected 3 got $rc"
rm -rf "$LOCKDIR" "$LOCKDIR.queue.d"

echo "== stall steal: an alive-but-idle holder past the age ceiling is stolen =="
rm -f "$(cur_att)"   # the prior section minted an attestation for this SHA; reuse would skip the lock
# The stamp must sit within the recycle slop of the holder process's real age
# (a genuinely stalled holder wrote its own stamp): 12s old, STALL_MIN_AGE=10.
recent="$(date -u -v-12S +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '12 seconds ago' +%Y-%m-%dT%H:%M:%SZ)"
mkdir -p "$LOCKDIR"
sleep 600 & stall_holder=$!
printf 'pid=%s started=%s host=x sha=deadbee\n' "$stall_holder" "$recent" > "$LOCKDIR/holder"
out="$(PREFLIGHT_STALL_MIN_AGE=10 PREFLIGHT_STALL_PROBE_SPACING=2 run_pf --wait-timeout 30 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "stole from a stalled holder and ran to GREEN" || fail "stall steal failed rc=$rc: $out"
grep <<<"$out" -q "stalled holder" && ok "the steal is reported as a recorded exception" || fail "no exception line: $out"
if ! kill -0 "$stall_holder" 2>/dev/null; then
    ok "the stalled holder's tree was TERMed on the steal"
else
    fail "the stalled holder survived the steal"; kill "$stall_holder" 2>/dev/null
fi
wait "$stall_holder" 2>/dev/null
rm -rf "$LOCKDIR" "$LOCKDIR.queue.d"

echo "== stall guard: a live, non-stale holder is never stolen =="
# Floor 0 makes the stall branch unreachable by arithmetic (delta >= 0 can
# never be < 0), and the holder is the suite's own pid: it cannot die or be
# recycled mid-test, so neither dead path can fire either. The old fixture
# used a freshly forked busy-spin holder with a stamp computed a section
# earlier: under host load the section's runtime exceeded the 60s recycle
# slop, the stamp then claimed an age the young process could not have, and
# the (correct) recycled-pid steal fired - failing the code for the fixture's
# own lie. Progress-vs-stall semantics are pinned by the unit section below.
rm -f "$(cur_att)"   # the stall-steal run just minted one for this same SHA
mkdir -p "$LOCKDIR"
guard_recent="$(date -u -v-12S +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '12 seconds ago' +%Y-%m-%dT%H:%M:%SZ)"
printf 'pid=%s started=%s host=x sha=deadbee\n' "$$" "$guard_recent" > "$LOCKDIR/holder"
holder_stamp="$(cat "$LOCKDIR/holder")"
out="$(PREFLIGHT_STALL_MIN_AGE=10 PREFLIGHT_STALL_PROBE_SPACING=2 PREFLIGHT_STALL_CPU_FLOOR=0 run_pf --wait-timeout 6 2>&1)"; rc=$?
[[ $rc -eq 3 ]] && ok "a healthy holder is waited on, not stolen" || fail "expected 3 got $rc: $out"
grep <<<"$out" -q "stalled holder" && fail "stole from a healthy holder" || ok "no false stall verdict"
[[ "$(cat "$LOCKDIR/holder" 2>/dev/null)" == "$holder_stamp" ]] && ok "the healthy holder kept its lock" || fail "holder stamp changed"
rm -rf "$LOCKDIR" "$LOCKDIR.queue.d"

echo "== orphan steal: a reparented holder is stolen before the stall floor =="
# Host probe first: a double-fork must reparent to pid 1 for the orphan
# predicate to be observable. Under a subreaper (systemd --user, some CI
# wrappers) reparenting lands on the subreaper instead; the code then
# correctly fails toward waiting, so the steal cannot be asserted here and
# the orphan lanes skip rather than fail the suite for the host's shape
# (the Darwin signal-lane skip is the prior idiom for exactly this).
probe_orphan="$(bash -c 'sleep 600 >/dev/null 2>&1 & echo $!')"
for _i in $(seq 1 40); do
    [[ "$(ps -o ppid= -p "$probe_orphan" 2>/dev/null | tr -d ' ')" == "1" ]] && break
    sleep 0.2
done
HOST_SEES_ORPHANS=1
[[ "$(ps -o ppid= -p "$probe_orphan" 2>/dev/null | tr -d ' ')" == "1" ]] || HOST_SEES_ORPHANS=0
kill "$probe_orphan" 2>/dev/null
if [[ "$HOST_SEES_ORPHANS" -eq 0 ]]; then
    echo "  ok: orphan lanes skipped on this host (double-fork did not land on pid 1; the code correctly waits)"
fi
if [[ "$HOST_SEES_ORPHANS" -eq 1 ]]; then
rm -f "$ATT"   # every steal section clears the attestation or reuse skips the lock
orphan_pid="$(bash -c 'sleep 600 >/dev/null 2>&1 & echo $!')"
for _i in $(seq 1 40); do
    [[ "$(ps -o ppid= -p "$orphan_pid" 2>/dev/null | tr -d ' ')" == "1" ]] && break
    sleep 0.2
done
# Stamp 12s old against a 600s floor: only the orphan bypass can reach the
# CPU probe. The stamp sits within the recycle slop of the young process.
orphan_recent="$(date -u -v-12S +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '12 seconds ago' +%Y-%m-%dT%H:%M:%SZ)"
mkdir -p "$LOCKDIR"
printf 'pid=%s started=%s host=x sha=deadbee\n' "$orphan_pid" "$orphan_recent" > "$LOCKDIR/holder"
out="$(PREFLIGHT_STALL_MIN_AGE=600 PREFLIGHT_STALL_PROBE_SPACING=2 run_pf --wait-timeout 30 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "stole from an orphaned holder before the 600s floor and ran to GREEN" || fail "orphan steal failed rc=$rc: $out"
grep <<<"$out" -q "orphaned holder" && ok "the steal names the orphan condemnation" || fail "no orphan exception line: $out"
if ! kill -0 "$orphan_pid" 2>/dev/null; then
    ok "the orphaned holder's tree was TERMed on the steal"
else
    fail "the orphan survived the steal"; kill "$orphan_pid" 2>/dev/null
fi
rm -rf "$LOCKDIR" "$LOCKDIR.queue.d"
fi

echo "== orphan guard: an orphan that is computing keeps its lock =="
# LD3 pinned: the bypass removes the age floor and never the CPU probe. Floor 0
# makes condemnation unreachable by arithmetic, so an orphan that spins must be
# waited on to the timeout like any healthy holder.
if [[ "$HOST_SEES_ORPHANS" -eq 0 ]]; then
    echo "  ok: orphan guard skipped on this host (no observable orphan to guard)"
else
spin_orphan="$(bash -c 'while :; do :; done >/dev/null 2>&1 & echo $!')"
for _i in $(seq 1 40); do
    [[ "$(ps -o ppid= -p "$spin_orphan" 2>/dev/null | tr -d ' ')" == "1" ]] && break
    sleep 0.2
done
rm -f "$ATT"   # the orphan-steal run just minted one for this same SHA
mkdir -p "$LOCKDIR"
spin_recent="$(date -u -v-12S +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '12 seconds ago' +%Y-%m-%dT%H:%M:%SZ)"
printf 'pid=%s started=%s host=x sha=deadbee\n' "$spin_orphan" "$spin_recent" > "$LOCKDIR/holder"
spin_stamp="$(cat "$LOCKDIR/holder")"
out="$(PREFLIGHT_STALL_MIN_AGE=600 PREFLIGHT_STALL_PROBE_SPACING=2 PREFLIGHT_STALL_CPU_FLOOR=0 run_pf --wait-timeout 6 2>&1)"; rc=$?
[[ $rc -eq 3 ]] && ok "an orphan under floor 0 is waited on, not stolen" || fail "expected 3 got $rc: $out"
grep <<<"$out" -q "orphaned holder" && fail "stole from a computing orphan (the bypass skipped the probe)" || ok "no orphan condemnation under floor 0"
[[ "$(cat "$LOCKDIR/holder" 2>/dev/null)" == "$spin_stamp" ]] && ok "the computing orphan kept its lock" || fail "holder stamp changed"
kill "$spin_orphan" 2>/dev/null
rm -rf "$LOCKDIR" "$LOCKDIR.queue.d"
fi

echo "== recycled pid: a stamp whose pid is a younger live process reads as dead =="
old="$(date -u -v-25M +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '25 minutes ago' +%Y-%m-%dT%H:%M:%SZ)"
rm -f "$(cur_att)"
mkdir -p "$LOCKDIR"
sleep 600 & recycled_holder=$!
printf 'pid=%s started=%s host=x sha=deadbee\n' "$recycled_holder" "$old" > "$LOCKDIR/holder"
out="$(run_pf --wait-timeout 30 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "stole the recycled holder's lock and ran to GREEN" || fail "recycled steal failed rc=$rc: $out"
# The steal fires at first acquire (before queueing), which prints nothing;
# what matters is the path taken: dead-style, never the stall/TERM verdict.
grep <<<"$out" -q "stalled holder" && fail "recycled pid took the stall path" || ok "recycled pid took the dead path, not the stall path"
kill -0 "$recycled_holder" 2>/dev/null && ok "the innocent recycled process was NOT signaled" || fail "an innocent process was TERMed"
kill "$recycled_holder" 2>/dev/null; wait "$recycled_holder" 2>/dev/null
rm -rf "$LOCKDIR" "$LOCKDIR.queue.d"

echo "== phantom ticket: a queued ticket stamped by a recycled pid is reaped =="
rm -f "$(cur_att)"   # else attestation reuse skips the lock and the phantom is never walked
mkdir -p "$LOCKDIR.queue.d/000001"
sleep 600 & phantom_pid=$!
printf 'pid=%s started=%s host=x sha=deadbee\n' "$phantom_pid" "$old" > "$LOCKDIR.queue.d/000001/holder"
out="$(run_pf --wait-timeout 30 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "a phantom front ticket did not wedge a free lock" || fail "phantom wedged the queue rc=$rc: $out"
[[ ! -d "$LOCKDIR.queue.d/000001" ]] && ok "the phantom ticket was reaped" || fail "phantom ticket survived"
kill "$phantom_pid" 2>/dev/null; wait "$phantom_pid" 2>/dev/null
rm -rf "$LOCKDIR" "$LOCKDIR.queue.d"

echo "== cancel: the preflight-cancel sentinel stops a queued waiter and clears its ticket =="
# Signals are NOT the asserted path: macOS bash 3.2 does not run INT/TERM
# traps while waiting on a child (verified against foreground sleep,
# sleep+wait, and a builtin read), so a SIGINT-asserting test fails on this
# platform while passing on CI's Linux bash 5. The polled sentinel is the
# cancellation contract that holds everywhere; the traps remain best-effort.
rm -f "$(cur_att)"   # else reuse exits 0 before the waiter ever queues
rm -rf "$FIX/.fno"   # fresh clone: no state dir exists until the queued run ensures it
mkdir -p "$LOCKDIR"
sleep 600 & cancel_holder=$!
printf 'pid=%s started=%s host=x sha=deadbee\n' "$cancel_holder" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$LOCKDIR/holder"
( cd "$FIX" && exec bash scripts/ci/preflight.sh --wait-timeout 60 >/dev/null 2>&1 ) & cancel_w=$!
kill -0 "$cancel_w" 2>/dev/null || fail "fixture: the waiter failed to fork under load"
cancel_ticket=""
cancel_deadline=$(( $(date +%s) + 90 ))
while [[ "$(date +%s)" -lt "$cancel_deadline" ]]; do
    cancel_ticket="$(ls "$LOCKDIR.queue.d" 2>/dev/null | sed -n '1p')"
    [[ -n "$cancel_ticket" ]] && break
    sleep 1
done
[[ -n "$cancel_ticket" ]] && ok "the waiter queued a ticket" || fail "waiter never queued (startup starved)"
[[ -d "$FIX/.fno" ]] && ok "the queued run ensured the sentinel's parent exists" \
    || fail "fresh checkout: the advertised touch target has no parent"
touch "$FIX/.fno/preflight-cancel"
wait "$cancel_w"; rc=$?
[[ $rc -eq 130 ]] && ok "the sentinel stops a queued waiter with exit 130" || fail "expected 130 got $rc"
[[ ! -d "$LOCKDIR.queue.d/$cancel_ticket" ]] && ok "the cancelled waiter's ticket was removed" || fail "ticket left behind"
[[ ! -e "$FIX/.fno/preflight-cancel" ]] && ok "the sentinel is consumed (one-shot)" || fail "sentinel left behind for the next run"
kill "$cancel_holder" 2>/dev/null; wait "$cancel_holder" 2>/dev/null
rm -rf "$LOCKDIR" "$LOCKDIR.queue.d"

echo "== cancel guard: a STALE sentinel never cancels a later, innocent waiter =="
# A sentinel nobody consumed (its wait already ended) must not sit waiting to
# kill the next queued run: past the one-hour grace the next waiter discards
# it and keeps waiting.
mkdir -p "$LOCKDIR"
sleep 600 & stale_holder=$!
printf 'pid=%s started=%s host=x sha=deadbee\n' "$stale_holder" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$LOCKDIR/holder"
touch -t 202001010000 "$FIX/.fno/preflight-cancel"   # mtime years old
out="$(run_pf --wait-timeout 4 2>&1)"; rc=$?
[[ $rc -eq 3 ]] && ok "a stale sentinel did not cancel the waiter" || fail "stale sentinel cancelled rc=$rc: $out"
echo "$out" | grep -q "cancelled while queued" && fail "reported a cancellation nobody requested" || ok "no false cancel report"
[[ ! -e "$FIX/.fno/preflight-cancel" ]] && ok "the stale sentinel was discarded by the next waiter" || fail "stale sentinel left behind"
kill "$stale_holder" 2>/dev/null; wait "$stale_holder" 2>/dev/null
rm -rf "$LOCKDIR" "$LOCKDIR.queue.d"

echo "== cancel (signal lane): INT still works where bash runs traps =="
# macOS bash 3.2 never delivers trapped INT while the shell waits on a child,
# so the signal branch is asserted only on platforms where it can fire (CI's
# Linux bash 5); the sentinel test above carries the contract everywhere.
if [[ "$(uname)" != "Darwin" ]]; then
    rm -f "$(cur_att)"
    mkdir -p "$LOCKDIR"
    sleep 600 & sig_holder=$!
    printf 'pid=%s started=%s host=x sha=deadbee\n' "$sig_holder" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$LOCKDIR/holder"
    ( cd "$FIX" && exec bash scripts/ci/preflight.sh --wait-timeout 60 >/dev/null 2>&1 ) & sig_w=$!
    sig_ticket=""
    sig_deadline=$(( $(date +%s) + 90 ))
    while [[ "$(date +%s)" -lt "$sig_deadline" ]]; do
        sig_ticket="$(ls "$LOCKDIR.queue.d" 2>/dev/null | sed -n '1p')"
        [[ -n "$sig_ticket" ]] && break
        sleep 1
    done
    [[ -n "$sig_ticket" ]] || fail "signal lane: waiter never queued"
    kill -INT "$sig_w" 2>/dev/null
    wait "$sig_w"; rc=$?
    [[ $rc -eq 130 ]] && ok "SIGINT exits 130 where traps are delivered" || fail "signal lane expected 130 got $rc"
    [[ ! -d "$LOCKDIR.queue.d/$sig_ticket" ]] && ok "signal-cancelled ticket removed" || fail "signal lane ticket left behind"
    kill "$sig_holder" 2>/dev/null; wait "$sig_holder" 2>/dev/null
    rm -rf "$LOCKDIR" "$LOCKDIR.queue.d"
else
    echo "  ok: signal lane skipped on Darwin (bash 3.2 does not deliver trapped INT while waiting; sentinel lane covers the contract)"
fi

echo "== cputime parse: octal-looking fields sum in base 10 =="
eval "$(sed -n '/^cputime_to_s() {/,/^}/p' "$PREFLIGHT_SRC")"
[[ "$(cputime_to_s "08:09.40")" == "489" ]] && ok "08:09.40 parses as 489s" || fail "octal parse: $(cputime_to_s "08:09.40")"
[[ "$(cputime_to_s "1-02:03:04")" == "93784" ]] && ok "day-prefixed durations parse" || fail "day parse: $(cputime_to_s "1-02:03:04")"
unset -f cputime_to_s

echo "== stall predicate: floor and turnover are arithmetic, not host load =="
# The progress-vs-stall semantics are pinned here, deterministically: a
# sleeping holder accumulates no CPU on any load, and floor 0 makes the
# condemnation branch unreachable by arithmetic (delta >= 0 can never be < 0).
eval "$(sed -n -e '/^cputime_to_s() {/,/^}/p' -e '/^holder_tree_pids() {/,/^}/p' \
    -e '/^holder_tree_cpu() {/,/^}/p' -e '/^process_age_s() {/,/^}/p' \
    -e '/^holder_is_orphaned() {/,/^}/p' -e '/^holder_is_stalled() {/,/^}/p' "$PREFLIGHT_SRC")"
STALL_MIN_AGE=10; STALL_PROBE_SPACING=1; STALL_CPU_FLOOR=0
unit_recent="$(date -u -v-12S +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '12 seconds ago' +%Y-%m-%dT%H:%M:%SZ)"
sleep 600 & unit_a=$!
line_a="pid=$unit_a started=$unit_recent host=x sha=deadbee"
holder_is_stalled "$line_a" && fail "condemned before a spaced second sample" \
    || ok "first sample baselines instead of condemning"
sleep 1.3
holder_is_stalled "$line_a" && fail "floor 0 condemned a live holder" \
    || ok "floor 0 can never condemn (delta >= 0 is not < 0)"
STALL_CPU_FLOOR=1
sleep 600 & unit_b=$!
line_b="pid=$unit_b started=$unit_recent host=x sha=deadbee"
holder_is_stalled "$line_b" || true   # baseline for the second sleeper
sleep 1.3
holder_is_stalled "$line_b" && ok "a zero-CPU holder is condemned after the probe window" \
    || fail "no-CPU holder not condemned"
kill "$unit_a" "$unit_b" 2>/dev/null; wait "$unit_a" 2>/dev/null; wait "$unit_b" 2>/dev/null
# Pin the sampler itself: before its pid-argument fix every sample summed 0
# (an unbound local under set -u), which floor arithmetic cannot distinguish
# from a truly idle tree - so assert REAL positive CPU from a busy tree.
# Four spinners over 15s yield >= 1 integer CPU-second even at load ~400
# (measured ~1s per spinner per 20s at load 380); fewer or shorter would
# truncate to zero on a starved box.
unit_spin_pids=""
for _s in 1 2 3 4; do ( while :; do :; done ) & unit_spin_pids="$unit_spin_pids $!"; done
unit_sum_start=0
for _p in $unit_spin_pids; do unit_sum_start=$(( unit_sum_start + $(holder_tree_cpu "$_p") )); done
sleep 15
unit_sum_end=0
for _p in $unit_spin_pids; do unit_sum_end=$(( unit_sum_end + $(holder_tree_cpu "$_p") )); done
for _p in $unit_spin_pids; do kill "$_p" 2>/dev/null; wait "$_p" 2>/dev/null; done
[[ $(( unit_sum_end - unit_sum_start )) -ge 1 ]] \
    && ok "the tree-CPU sampler measures real progress (delta=$(( unit_sum_end - unit_sum_start ))s)" \
    || fail "sampler read no progress from a busy tree (delta=$(( unit_sum_end - unit_sum_start ))s)"
unset -f cputime_to_s holder_tree_pids holder_tree_cpu process_age_s holder_is_orphaned holder_is_stalled

echo "== orphan predicate: parentage, not existence =="
# holder_is_orphaned must read WHO launched the pid, not whether the pid is
# alive: $$ has a live parent, an unreadable pid must bias toward keeping the
# lock (never steal on what cannot be measured), and a real orphan reads true.
eval "$(sed -n '/^holder_is_orphaned() {/,/^}/p' "$PREFLIGHT_SRC")"
holder_is_orphaned "$$" && fail "condemned the suite itself (it has a live parent)" \
    || ok "a parented pid is not an orphan"
holder_is_orphaned "99999999" && fail "condemned a pid nothing can read" \
    || ok "an unreadable pid reads as not-orphan (waited on, never stolen)"
if [[ "${HOST_SEES_ORPHANS:-1}" -eq 1 ]]; then
    unit_orphan="$(bash -c 'sleep 600 >/dev/null 2>&1 & echo $!')"
    for _i in $(seq 1 40); do
        [[ "$(ps -o ppid= -p "$unit_orphan" 2>/dev/null | tr -d ' ')" == "1" ]] && break
        sleep 0.2
    done
    holder_is_orphaned "$unit_orphan" && ok "a reparented pid reads as orphaned" \
        || fail "failed to see a real orphan (ppid=$(ps -o ppid= -p "$unit_orphan" 2>/dev/null | tr -d ' '))"
    kill "$unit_orphan" 2>/dev/null
else
    echo "  ok: orphan unit assert skipped on this host (no observable orphan)"
fi
unset -f holder_is_orphaned

echo "== stale base: HEAD behind origin/main refuses (exit 6) before any lock work =="
for _c in sb1 sb2 sb3; do ( cd "$FIX" && git commit -q --allow-empty -m "$_c" ); done
git -C "$FIX" update-ref refs/remotes/origin/main "$(git -C "$FIX" rev-parse HEAD)"
( cd "$FIX" && git reset -q --hard HEAD~3 )
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 6 ]] && ok "exit 6 on a stale base" || fail "expected 6 got $rc: $out"
grep <<<"$out" -q "3 commit(s) behind origin/main" && ok "names the behind count" || fail "no behind count: $out"
grep <<<"$out" -q "rebase" && ok "tells the caller to rebase" || fail "no rebase guidance: $out"
[[ ! -d "$LOCKDIR" && ! -d "$LOCKDIR.queue.d" ]] && ok "refused before creating any lock or queue artifact" || fail "lock artifacts left behind"
git -C "$FIX" update-ref refs/remotes/origin/main "$(git -C "$FIX" rev-parse HEAD)"

echo "== canonical concurrency: a second clone cannot append pending for the same SHA =="
lock_sha="$(git -C "$FIX" rev-parse HEAD)"
GLOBAL_RECEIPT_LOCKDIR="$TMP/.preflight-receipt-locks/$lock_sha.d"
mkdir -p "$GLOBAL_RECEIPT_LOCKDIR"
printf 'pid=%s started=NOW host=x sha=%s\n' "$$" "$lock_sha" > "$GLOBAL_RECEIPT_LOCKDIR/holder"
before="$(jq -s --arg sha "$lock_sha" '[.[] | select(.type == "verification_receipt" and .data.candidate_sha == $sha)] | length' "$GLOBAL_EVENTS")"
out="$(run_pf --force --wait-timeout 0 2>&1)"; rc=$?
after="$(jq -s --arg sha "$lock_sha" '[.[] | select(.type == "verification_receipt" and .data.candidate_sha == $sha)] | length' "$GLOBAL_EVENTS")"
[[ $rc -eq 3 ]] && ok "same-candidate global lock refuses the second run" || fail "expected 3 got $rc: $out"
[[ "$after" == "$before" ]] && ok "refused run appended no unmatched pending" || fail "receipt count changed: $before -> $after"
rm -rf "$GLOBAL_RECEIPT_LOCKDIR"

echo "== canonical lock window: a loser preserves the winner's unstamped lock =="
mkdir -p "$GLOBAL_RECEIPT_LOCKDIR"
out="$(run_pf --force 2>&1)"; rc=$?
[[ $rc -eq 3 ]] && ok "unstamped canonical lock refuses the loser" || fail "expected 3 got $rc: $out"
[[ -d "$GLOBAL_RECEIPT_LOCKDIR" ]] && ok "loser cleanup preserves the winner's lock" || fail "loser deleted the winner's lock"
rm -rf "$GLOBAL_RECEIPT_LOCKDIR"

echo "== canonical lock signal: cancellation after mkdir cleans both owned locks =="
before="$(jq -s --arg sha "$lock_sha" '[.[] | select(.type == "verification_receipt" and .data.candidate_sha == $sha)] | length' "$GLOBAL_EVENTS")"
export PREFLIGHT_TEST_SIGNAL_LOCK=1
out="$(run_pf --force 2>&1)"; rc=$?
unset PREFLIGHT_TEST_SIGNAL_LOCK
after="$(jq -s --arg sha "$lock_sha" '[.[] | select(.type == "verification_receipt" and .data.candidate_sha == $sha)] | length' "$GLOBAL_EVENTS")"
[[ $rc -eq 130 ]] && ok "deferred signal exits 130 after ownership is complete" || fail "expected 130 got $rc: $out"
[[ ! -d "$LOCKDIR" && ! -d "$GLOBAL_RECEIPT_LOCKDIR" ]] \
    && ok "signal cleanup releases both owned locks" || fail "signal left a lock behind"
[[ "$after" == "$before" ]] && ok "cancelled run appended no pending receipt" || fail "receipt count changed: $before -> $after"

echo "== AC1-FR: a stale lock (dead holder) is stolen, run proceeds =="
mkdir -p "$LOCKDIR"; printf 'pid=%s started=OLD host=x sha=deadbee\n' 999999 > "$LOCKDIR/holder"  # dead pid
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "stole stale lock and ran to GREEN" || fail "stale-lock steal failed rc=$rc: $out"

echo "== steal-race: concurrent steal of one dead holder -> exactly one winner =="
# Measured: against the pre-fix rm-rf-then-mkdir steal this catches a double
# win in 106 of 120 rounds (88% each), so 5 rounds is ~1e-5 of missing it.
# Against the fixed steal it is deterministic - 0 double wins in 120 rounds -
# so it does not flake. An intermediate version of the fix (rename without the
# steal-then-verify check) did fail this about 1 run in 20; that was a true
# positive for a residual race, not flakiness, and is the reason the check exists.
steal_winners=0
for _round in 1 2 3 4 5; do
    rm -rf "$LOCKDIR"
    rm -f "$(cur_att)"   # else the prior green attestation satisfies the gate and no one contends
    mkdir -p "$LOCKDIR"; printf 'pid=%s started=OLD host=x sha=deadbee\n' 999999 > "$LOCKDIR/holder"
    run_pf --wait-timeout 0 >/dev/null 2>&1 & p1=$!
    run_pf --wait-timeout 0 >/dev/null 2>&1 & p2=$!
    wait $p1; r1=$?
    wait $p2; r2=$?
    # winners exit 0 (ran) - losers exit 3 (lock held). Never two winners.
    [[ $r1 -eq 0 ]] && steal_winners=$((steal_winners+1))
    [[ $r2 -eq 0 ]] && steal_winners=$((steal_winners+1))
    [[ $r1 -eq 0 && $r2 -eq 0 ]] && { fail "round $_round: BOTH racers stole the same dead lock"; break; }
done
[[ $steal_winners -ge 1 ]] && ok "steal still works under contention ($steal_winners/5 rounds had a winner)" \
    || fail "no racer ever acquired the stolen lock (steal is now dead, not just serialized)"
rm -rf "$LOCKDIR"

echo "== tripwire: a stolen LOCK also VOIDs, and the stealer's lock survives =="
# The tripwire's other arm. The worktree stays put here; only the holder changes,
# so this pins the lock comparison rather than the sha one, and proves cleanup
# does not delete a lock that now belongs to the stealer.
cat > "$BIN/uv" <<EOF
#!/usr/bin/env bash
printf 'pid=424242 started=NOW host=x sha=cafe123\n' > "$LOCKDIR/holder"
echo "smoke: all green (stub, stole the lock)"; exit 0
EOF
chmod +x "$BIN/uv"
( cd "$FIX" && git commit -q --allow-empty -m "lock-stealing smoke stub" )
write_attest "$(git -C "$FIX" rev-parse HEAD)"   # AC2-ERR: a prior attestation for this SHA
# --force bypasses reuse so the planted attestation does not short-circuit; the
# run must actually execute to reach the VOID tripwire.
out="$(run_pf --force 2>&1)"; rc=$?
[[ $rc -eq 5 ]] && ok "exit 5 (VOID) when the lock changed hands" || fail "expected 5 got $rc: $out"
grep <<<"$out" -q "VOID - another preflight took our lock" && ok "names the lock, not the worktree" || fail "wrong VOID cause: $out"
grep -q "pid=424242" "$LOCKDIR/holder" 2>/dev/null && ok "the stealer's lock survived our exit" \
    || fail "cleanup deleted a lock owned by the stealer"
[[ -f "$(cur_att)" ]] && ok "VOID left the prior attestation untouched (AC2-ERR)" || fail "VOID wrote or deleted the attestation"
rm -rf "$LOCKDIR"; rm -f "$(cur_att)"

# NOTE: keep the worktree-hijack leg LAST. Its stub permanently resets the
# fixture's preflight worktree, so any test appended after it inherits a
# hijacked tree and fails for reasons that have nothing to do with it.
echo "== tripwire: a hijacked worktree VOIDs the verdict instead of reporting it =="
# Move the shared worktree off our candidate mid-run, as a second preflight's
# `reset --hard` would. The stub smoke.sh is the hook: it fires inside the run.
PF_WT="$WT_BASE/repo/preflight"
cat > "$BIN/uv" <<EOF
#!/usr/bin/env bash
git -C "$PF_WT" reset --hard HEAD~1 >/dev/null 2>&1
echo "smoke: all green (stub, hijacked the worktree)"; exit 0
EOF
chmod +x "$BIN/uv"
( cd "$FIX" && git commit -q --allow-empty -m "hijacking smoke stub" )
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 5 ]] && ok "exit 5 (VOID), distinct from RED's 1" || fail "expected 5 got $rc: $out"
grep <<<"$out" -q "VOID - worktree moved off our candidate" && ok "names the cause" || fail "no VOID line: $out"
grep <<<"$out" -q "not a code failure" && ok "tells the caller it is not RED" || fail "no re-run hint: $out"
grep <<<"$out" -qE "GREEN - safe to push|RED - fix" && fail "printed a verdict for a hijacked tree" || ok "printed neither GREEN nor RED"

echo ""
if [[ $FAILS -eq 0 ]]; then echo "test_preflight: ALL PASS"; exit 0
else echo "test_preflight: $FAILS FAILED"; exit 1; fi
