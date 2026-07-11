#!/usr/bin/env bash
# test_agent_receipt.sh - verify skills/agent/scripts/spawn.sh
# (genuine spawn + honest-receipt reporting + collision guards, tasks 1.3/1.4).
#
# Hermetic: a mock `fno` on PATH (controlled by MOCK_* env vars) stands in for
# the real agents/claim verbs, so no real bg worker launches.
#
# Covers:
#   AC1-HP   ask returns a real 8-hex short-id -> result=launched short_id=<hex>.
#   AC5-FR   ask exits nonzero  -> result=failed + the real captured stderr,
#            NO short_id, NEVER a fabricated uuid.
#   AC5-FR   ask returns NO short-id (resumed/empty) -> result=failed, no uuid.
#   AC6-FR   node:<id> claim live -> result=already-running, ask NEVER called.
#   AC6-FR   a live same-name agent -> already-running, ask NEVER called.
#   plus: dead agent row removed then a fresh spawn; claim probe failure ->
#         fail closed; corrupted claim -> fail; never emits short_id on non-launch.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SPAWN="$REPO_ROOT/skills/agent/scripts/spawn.sh"

command -v jq >/dev/null 2>&1 || { echo "jq required for this test" >&2; exit 1; }

TMP=$(mktemp -d -t dispatch-receipt.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

[[ -f "$SPAWN" ]] || { echo "spawn script missing: $SPAWN" >&2; exit 1; }
bash -n "$SPAWN" || { echo "bash -n rejected $SPAWN" >&2; exit 1; }

# ---- mock fno -------------------------------------------------------------
# Behavior driven by MOCK_* env. Records `agents ask` invocations to $ASK_LOG
# (ask must NEVER be called by spawn.sh after Group 1 ab-8b3e4fe0 - ask never
# creates) and `agents spawn|host` to $SPAWN_LOG so a test can assert nothing
# launched on a collision/already-running path.
BIN="$TMP/bin"; mkdir -p "$BIN"
ASK_LOG="$TMP/ask.log"
cat > "$BIN/fno" <<'MOCK'
#!/usr/bin/env bash
sub="$1 ${2:-}"
case "$sub" in
  "agents spawn-guard")
    # x-73cc: spawn.sh now calls this for the --node path instead of `claim
    # status` + `claim acquire`. Synthesize the verdict from the same MOCK_* env
    # (MOCK_CLAIM_RC / MOCK_CLAIM_STATE / MOCK_CLAIM_HOLDER / MOCK_ACQ_RC) so the
    # existing Guard 1 + Guard 2 scenarios keep exercising spawn.sh's mapping.
    node="${3:-}"
    [[ "${MOCK_CLAIM_RC:-0}" -ne 0 ]] && { printf '{"verdict":"error","detail":"claim probe failed (mock rc=%s); not dispatching to avoid a double-launch"}\n' "${MOCK_CLAIM_RC}"; exit 3; }
    if [[ "${MOCK_CLAIM_STATE:-free}" == "_noparse_" ]]; then printf '{"verdict":"error","detail":"claim status returned no parseable state; not dispatching"}\n'; exit 3; fi
    case "${MOCK_CLAIM_STATE:-free}" in
      live)      printf '{"verdict":"already-running","reason":"live-claim","holder":"%s"}\n' "${MOCK_CLAIM_HOLDER:-peer}"; exit 0 ;;
      corrupted) printf '{"verdict":"corrupted","detail":"node:%s claim is corrupted; force-release or repair before dispatching"}\n' "$node"; exit 0 ;;
    esac
    # free / stale -> dispatchable candidate; Guard 2 reservation (MOCK_ACQ_RC=1 models a racing peer).
    [[ "${MOCK_ACQ_RC:-0}" -eq 1 ]] && { printf '{"verdict":"already-running","reason":"reservation-held"}\n'; exit 0; }
    [[ "${MOCK_ACQ_RC:-0}" -ne 0 ]] && { printf '{"verdict":"error","detail":"could not acquire dispatch reservation dispatch:%s (mock rc=%s)"}\n' "$node" "${MOCK_ACQ_RC}"; exit 3; }
    printf '{"verdict":"dispatchable","reservation_key":"dispatch:%s","reservation_holder":"dispatch-skill:mock"}\n' "$node"; exit 0 ;;
  "claim status")
    [[ "${MOCK_CLAIM_RC:-0}" -ne 0 ]] && exit "${MOCK_CLAIM_RC}"
    if [[ "${MOCK_CLAIM_STATE:-free}" == "_noparse_" ]]; then echo '{}'; exit 0; fi
    printf '{"state":"%s","holder":"%s"}\n' "${MOCK_CLAIM_STATE:-free}" "${MOCK_CLAIM_HOLDER:-peer}"
    ;;
  "claim acquire")
    # rc 0 acquired (default), rc 1 held-by-peer (race), other = error.
    exit "${MOCK_ACQ_RC:-0}" ;;
  "claim release") exit 0 ;;
  "agents list")
    # MOCK_LIST_RC simulates a crashed probe (daemon down); MOCK_LIST_GARBAGE
    # simulates unparseable output on exit 0. Both must fail the dispatch CLOSED.
    [[ -n "${MOCK_LIST_RC:-}" && "${MOCK_LIST_RC}" -ne 0 ]] && exit "${MOCK_LIST_RC}"
    if [[ "${MOCK_LIST_GARBAGE:-}" == "1" ]]; then echo "<<not json>>"; exit 0; fi
    if [[ -n "${MOCK_AGENT_STATUS:-}" ]]; then
      printf '{"agents":[{"name":"%s","status":"%s"}]}\n' "${MOCK_AGENT_NAME:-tgt-x}" "${MOCK_AGENT_STATUS}"
    else
      printf '{"agents":[]}\n'
    fi
    ;;
  "agents rm") echo "removed ${3:-}" ;;
  "agents ask")
    echo "ask $*" >> "$ASK_LOG"
    [[ -n "${MOCK_ASK_ERR:-}" ]] && printf '%s' "$MOCK_ASK_ERR" >&2
    printf '%s' "${MOCK_ASK_OUT:-}"
    exit "${MOCK_ASK_RC:-0}"
    ;;
  "agents spawn"|"agents host")
    # spawn/host emit PRETTY (multi-line) JSON on captured stdout, never a bare
    # line; MOCK_SPAWN_OUT carries that JSON. Recorded to SPAWN_LOG so a test can
    # assert the verb/provider/--yolo/--cwd argv and that ask was NOT called.
    echo "$*" >> "$SPAWN_LOG"
    [[ -n "${MOCK_SPAWN_ERR:-}" ]] && printf '%s' "$MOCK_SPAWN_ERR" >&2
    printf '%s' "${MOCK_SPAWN_OUT:-}"
    exit "${MOCK_SPAWN_RC:-0}"
    ;;
  *) echo "mock fno: unhandled '$*'" >&2; exit 99 ;;
esac
MOCK
chmod +x "$BIN/fno"
SPAWN_LOG="$TMP/spawn.log"
export ASK_LOG SPAWN_LOG
run_spawn() { PATH="$BIN:$PATH" bash "$SPAWN" "$@"; }
reset_log() { : > "$ASK_LOG"; : > "$SPAWN_LOG"; }
# The client-side claude spawn receipt (Group 1 ab-8b3e4fe0): ONE compact JSON
# line, byte-parity across the Python and Rust runtimes.
claude_receipt() { printf '{"name": "%s", "short_id": "%s", "provider": "claude", "status": "live"}\n' "${2:-tgt-x}" "$1"; }

# ---- AC1-HP: real short-id -> launched ------------------------------------
reset_log
OUT="$(MOCK_SPAWN_OUT="$(claude_receipt a1b2c3d4 tgt-deadbeef)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-deadbeef --provider claude --message "/target ab-deadbeef no-merge" --node ab-deadbeef)"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=a1b2c3d4"* ]] \
   && [[ "$OUT" == *"fno agents logs tgt-deadbeef"* ]] && [[ -s "$SPAWN_LOG" ]] && [[ ! -s "$ASK_LOG" ]]; then
  pass "AC1-HP real short-id -> launched + observability handles"
else
  fail "AC1-HP: $OUT"
fi

# ---- ab-77b691dc: --fresh / --here are PASS-THROUGH to fno agents spawn ----
# spawn.sh forwards the flags but never defaults them, so a target-class
# dispatcher can request canonical cwd while plain interactive ask/host/spawn
# keep the caller cwd (AC3).
reset_log
MOCK_SPAWN_OUT="$(claude_receipt a1b2c3d4 tgt-fresh)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
  run_spawn --name tgt-fresh --provider claude --message "/target ab-fresh" --node ab-fresh --fresh >/dev/null
grep -q -- "--fresh" "$SPAWN_LOG" \
  && pass "ab-77b691dc: --fresh forwarded to fno agents spawn" \
  || fail "ab-77b691dc: --fresh not forwarded: $(cat "$SPAWN_LOG")"

reset_log
MOCK_SPAWN_OUT="$(claude_receipt a1b2c3d4 tgt-here)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
  run_spawn --name tgt-here --provider claude --message "/target ab-here" --node ab-here --here >/dev/null
grep -q -- "--here" "$SPAWN_LOG" \
  && pass "ab-77b691dc: --here forwarded to fno agents spawn" \
  || fail "ab-77b691dc: --here not forwarded: $(cat "$SPAWN_LOG")"

reset_log
MOCK_SPAWN_OUT="$(claude_receipt a1b2c3d4 tgt-plain)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
  run_spawn --name tgt-plain --provider claude --message "/target ab-plain" --node ab-plain >/dev/null
if ! grep -q -- "--fresh" "$SPAWN_LOG" && ! grep -q -- "--here" "$SPAWN_LOG"; then
  pass "ab-77b691dc/AC3: plain spawn forwards neither --fresh nor --here (no default)"
else
  fail "ab-77b691dc/AC3: plain spawn leaked a cwd flag: $(cat "$SPAWN_LOG")"
fi

# ---- AC5-FR: ask nonzero -> failed + real captured stderr, no short_id/uuid ---
reset_log
OUT="$(MOCK_SPAWN_ERR="error: claude bg daemon not running" MOCK_SPAWN_RC=1 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-deadbeef --provider claude --message "/target ab-deadbeef" --node ab-deadbeef)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" == *"claude bg daemon not running"* ]] \
   && [[ "$OUT" != *"short_id="* ]] && [[ "$OUT" != *"result=launched"* ]]; then
  pass "AC5-FR spawn nonzero -> failed + real stderr, no short_id"
else
  fail "AC5-FR nonzero: $OUT"
fi

# ---- AC5-FR: a resume reply LINE that starts with hex must NOT leak as a
# short-id (whole-line match closes the `deadbeef was reverted` fabrication). ---
reset_log
OUT="$(MOCK_SPAWN_OUT=$'deadbeef was the commit I reverted\nfixed it\n' MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-deadbeef --provider claude --message "/target ab-deadbeef" --node ab-deadbeef)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" != *"short_id="* ]] && [[ "$OUT" != *"result=launched"* ]]; then
  pass "AC5-FR hex-leading reply line -> failed, no fabricated short_id"
else
  fail "AC5-FR hex-leading line leak: $OUT"
fi

# ---- AC5-FR: a stderr warning line that starts with hex must NOT be read as
# the receipt; a genuine 8-hex stdout line still launches. ---
reset_log
OUT="$(MOCK_SPAWN_ERR=$'abcdef99 deprecation notice\n' MOCK_SPAWN_OUT="$(claude_receipt cafe1234 tgt-deadbeef)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-deadbeef --provider claude --message "/target ab-deadbeef" --node ab-deadbeef)"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=cafe1234"* ]]; then
  pass "AC5-FR stderr hex warning ignored; real stdout receipt launches"
else
  fail "AC5-FR stderr-vs-stdout: $OUT"
fi

# ---- AC5-FR: no short-id in receipt -> failed, never a fabricated uuid -----
reset_log
OUT="$(MOCK_SPAWN_OUT=$'resumed session for tgt-deadbeef (follow-up)\n' MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-deadbeef --provider claude --message "/target ab-deadbeef" --node ab-deadbeef)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" != *"short_id="* ]] && [[ "$OUT" != *"result=launched"* ]]; then
  pass "AC5-FR no short-id -> failed, no fabricated uuid"
else
  fail "AC5-FR no short-id: $OUT"
fi

# ---- AC6-FR: live node claim -> already-running, ask NEVER called ----------
reset_log
OUT="$(MOCK_CLAIM_STATE=live MOCK_CLAIM_HOLDER="target-session:abc" \
       run_spawn --name tgt-deadbeef --provider claude --message "/target ab-deadbeef" --node ab-deadbeef)"
if [[ "$OUT" == *"result=already-running"* ]] && [[ "$OUT" != *"short_id="* ]] && [[ ! -s "$SPAWN_LOG" ]] && [[ ! -s "$ASK_LOG" ]]; then
  pass "AC6-FR live node claim -> already-running, no spawn"
else
  fail "AC6-FR live claim: $OUT (ask_log: $(cat "$ASK_LOG" 2>/dev/null))"
fi

# ---- AC6-FR: live same-name agent -> already-running, ask NEVER called -----
reset_log
OUT="$(MOCK_CLAIM_STATE=free MOCK_AGENT_NAME=tgt-deadbeef MOCK_AGENT_STATUS=live \
       run_spawn --name tgt-deadbeef --provider claude --message "/target ab-deadbeef" --node ab-deadbeef)"
if [[ "$OUT" == *"result=already-running"* ]] && [[ ! -s "$SPAWN_LOG" ]] && [[ ! -s "$ASK_LOG" ]]; then
  pass "AC6-FR live same-name agent -> already-running, no spawn"
else
  fail "AC6-FR live agent: $OUT"
fi

# ---- dead agent row removed, then a fresh spawn proceeds -------------------
reset_log
OUT="$(MOCK_CLAIM_STATE=free MOCK_AGENT_NAME=tgt-deadbeef MOCK_AGENT_STATUS=dead \
       MOCK_SPAWN_OUT="$(claude_receipt feed1234 tgt-deadbeef)" MOCK_SPAWN_RC=0 \
       run_spawn --name tgt-deadbeef --provider claude --message "/target ab-deadbeef" --node ab-deadbeef)"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=feed1234"* ]] && [[ -s "$SPAWN_LOG" ]]; then
  pass "dead agent row removed -> fresh spawn launches"
else
  fail "dead row recovery: $OUT"
fi

# ---- claim probe failure -> fail closed (no spawn) -------------------------
reset_log
OUT="$(MOCK_CLAIM_RC=3 run_spawn --name tgt-deadbeef --provider claude --message "/target ab-deadbeef" --node ab-deadbeef)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" == *"claim probe failed"* ]] && [[ ! -s "$SPAWN_LOG" ]]; then
  pass "claim probe failure -> fail closed, no spawn"
else
  fail "claim probe fail-closed: $OUT"
fi

# ---- unparseable claim state -> fail closed --------------------------------
reset_log
OUT="$(MOCK_CLAIM_STATE=_noparse_ run_spawn --name tgt-deadbeef --provider claude --message "/target ab-deadbeef" --node ab-deadbeef)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" == *"no parseable state"* ]] && [[ ! -s "$SPAWN_LOG" ]]; then
  pass "unparseable claim state -> fail closed"
else
  fail "claim noparse: $OUT"
fi

# ---- corrupted claim -> fail ----------------------------------------------
reset_log
OUT="$(MOCK_CLAIM_STATE=corrupted run_spawn --name tgt-deadbeef --provider claude --message "/target ab-deadbeef" --node ab-deadbeef)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" == *"corrupted"* ]] && [[ ! -s "$SPAWN_LOG" ]]; then
  pass "corrupted claim -> fail, no spawn"
else
  fail "corrupted claim: $OUT"
fi

# ---- stale claim (dead holder) -> recoverable, proceeds to launch ----------
reset_log
OUT="$(MOCK_CLAIM_STATE=stale MOCK_SPAWN_OUT="$(claude_receipt 5ade1234 tgt-deadbeef)" MOCK_SPAWN_RC=0 \
       run_spawn --name tgt-deadbeef --provider claude --message "/target ab-deadbeef" --node ab-deadbeef)"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=5ade1234"* ]] && [[ -s "$SPAWN_LOG" ]]; then
  pass "stale claim (dead holder) -> launches (recovery)"
else
  fail "stale claim recovery: $OUT"
fi

# ---- --cwd is threaded into the genuine spawn (cross-project dispatch) ------
reset_log
OUT="$(MOCK_SPAWN_OUT="$(claude_receipt c0dec0de tgt-fix-login)" MOCK_SPAWN_RC=0 \
       run_spawn --name tgt-fix-login --provider claude --message "/target fix login" --cwd /tmp/some-repo)"
if [[ "$OUT" == *"result=launched"* ]] && grep -q -- "--cwd /tmp/some-repo" "$SPAWN_LOG"; then
  pass "--cwd threaded into fno agents spawn"
else
  fail "--cwd threading: $OUT (spawn_log: $(cat "$SPAWN_LOG"))"
fi

# ---- free-form (no --node) skips the claim probe, launches -----------------
reset_log
OUT="$(MOCK_SPAWN_OUT="$(claude_receipt beef0001 tgt-fix-login)" MOCK_SPAWN_RC=0 \
       run_spawn --name tgt-fix-login --provider claude --message "/target fix login no-merge")"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=beef0001"* ]]; then
  pass "free-form (no --node) -> launched (no claim probe)"
else
  fail "free-form launch: $OUT"
fi

# ---- Guard 2 reservation: a racing peer dispatcher -> already-running -------
reset_log
OUT="$(MOCK_CLAIM_STATE=free MOCK_ACQ_RC=1 \
       run_spawn --name tgt-deadbeef --provider claude --message "/target ab-deadbeef" --node ab-deadbeef)"
if [[ "$OUT" == *"result=already-running"* ]] && [[ "$OUT" == *"peer dispatcher"* ]] && [[ ! -s "$SPAWN_LOG" ]]; then
  pass "Guard2 reservation held by peer -> already-running, no spawn"
else
  fail "Guard2 reservation race: $OUT"
fi

# ---- Guard 3: a non-terminal status (ready/busy) -> already-running ---------
# A drive-eligible worker (ready/idle/busy) must NOT be rm'd and respawned.
reset_log
OUT="$(MOCK_CLAIM_STATE=free MOCK_AGENT_NAME=tgt-deadbeef MOCK_AGENT_STATUS=ready \
       run_spawn --name tgt-deadbeef --provider claude --message "/target ab-deadbeef" --node ab-deadbeef)"
if [[ "$OUT" == *"result=already-running"* ]] && [[ "$OUT" == *"status=ready"* ]] && [[ ! -s "$SPAWN_LOG" ]]; then
  pass "Guard3 non-terminal status (ready) -> already-running, no rm/spawn"
else
  fail "Guard3 non-terminal status: $OUT"
fi

# ---- AC5-FR: a multi-line reply containing a standalone 8-hex line must NOT
# be misread as a launch (receipt is EXACTLY one 8-hex line). ----
reset_log
OUT="$(MOCK_SPAWN_OUT=$'here is your build\nabcd1234\n' MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-deadbeef --provider claude --message "/target ab-deadbeef" --node ab-deadbeef)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" != *"short_id="* ]] && [[ "$OUT" != *"result=launched"* ]]; then
  pass "AC5-FR multi-line reply w/ standalone hex line -> failed, no fabricated short_id"
else
  fail "AC5-FR multi-line receipt: $OUT"
fi

# ===========================================================================
# US1 - codex/gemini build dispatch -> `fno agents spawn` (exec) + JSON receipt
# ===========================================================================
# Pretty (multi-line) JSON is what `serde_json::to_string_pretty` emits for the
# spawn/host daemon Ok payload {"short_id","provider","status"}; the bare-8-hex
# grep would find NOTHING here, so a JSON branch (jq .short_id) is mandatory.
spawn_json() { printf '{\n  "short_id": "%s",\n  "provider": "%s",\n  "status": "live"\n}\n' "$1" "$2"; }

# ---- AC1-HP: codex exec spawn launches; ask NEVER called -------------------
reset_log
OUT="$(MOCK_SPAWN_OUT="$(spawn_json a1b2c3d4 codex)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-x --provider codex --message "Implement ab-deadbeef" --node ab-deadbeef --mode exec)"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=a1b2c3d4"* ]] \
   && [[ "$OUT" == *"mode=exec"* ]] && [[ "$OUT" == *"fno agents logs tgt-x"* ]] \
   && [[ -s "$SPAWN_LOG" ]] && grep -q "spawn" "$SPAWN_LOG" && [[ ! -s "$ASK_LOG" ]]; then
  pass "AC1-HP codex exec -> spawn launched (JSON .short_id), ask not called"
else
  fail "AC1-HP codex spawn: $OUT (spawn_log: $(cat "$SPAWN_LOG"))"
fi

# ---- AC1-ERR: codex binary missing (spawn rc!=0) -> failed + stderr --------
reset_log
OUT="$(MOCK_SPAWN_ERR="error: codex: command not found" MOCK_SPAWN_RC=1 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-x --provider codex --message "Implement ab-deadbeef" --node ab-deadbeef --mode exec)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" == *"codex: command not found"* ]] \
   && [[ "$OUT" != *"short_id="* ]] && [[ "$OUT" != *"result=launched"* ]]; then
  pass "AC1-ERR codex binary missing -> failed + real stderr, no short_id"
else
  fail "AC1-ERR codex spawn rc!=0: $OUT"
fi

# ---- AC1-UI: multi-line JSON parsed via jq (bare-line grep would miss it) --
# The AC1-HP vector already uses multi-line pretty JSON; assert the bare-line
# grep alone could not have produced this launch (defends against a regression
# to grep -xE on the spawn path).
reset_log
PRETTY="$(spawn_json c0ffee12 codex)"
if printf '%s' "$PRETTY" | grep -qxE '[0-9a-f]{8}'; then
  fail "AC1-UI test premise broken: bare-line grep matched pretty JSON"
else
  OUT="$(MOCK_SPAWN_OUT="$PRETTY" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
         run_spawn --name tgt-x --provider codex --message "build it" --node ab-deadbeef --mode exec)"
  [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=c0ffee12"* ]] \
    && pass "AC1-UI multi-line JSON -> jq parse launches (bare grep alone would fail)" \
    || fail "AC1-UI jq parse: $OUT"
fi

# ---- AC1-FR: spawn exits 0 but short_id empty -> failed, never fabricated ---
reset_log
OUT="$(MOCK_SPAWN_OUT='{"short_id": "", "provider": "codex", "status": "live"}' MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-x --provider codex --message "build it" --node ab-deadbeef --mode exec)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" == *"no valid short-id"* ]] \
   && [[ "$OUT" != *"short_id="* ]] && [[ "$OUT" != *"result=launched"* ]]; then
  pass "AC1-FR empty short_id (exit 0) -> failed, no fabricated handle"
else
  fail "AC1-FR empty short_id: $OUT"
fi

# ---- AC1-FR companion (x-61b7): substrate decides the short_id SHAPE --------
# The default/pane substrate is the owned-PTY daemon worker, whose short_id is a
# NAME-SLUG from derive_short_id() (daemon.rs) - not 8-hex. The receipt guard
# must accept it (it previously reported a false `failed` for a live worker).
reset_log
OUT="$(MOCK_SPAWN_OUT="$(spawn_json spawngoa codex)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-x --provider codex --message "build it" --node ab-deadbeef --mode exec)"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=spawngoa"* ]]; then
  pass "AC1-FR pane name-slug short_id -> launched"
else
  fail "AC1-FR pane name-slug short_id: $OUT"
fi

# ...but the bg lane (client-side claude --bg) really returns an 8-hex id, so its
# strict shape is preserved: a name-slug there still fails.
reset_log
OUT="$(MOCK_SPAWN_OUT="$(claude_receipt spawngoa tgt-x)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-x --provider claude --message "/target ab-deadbeef no-merge" --node ab-deadbeef --substrate bg)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" != *"result=launched"* ]]; then
  pass "AC1-FR bg lane keeps strict 8-hex (name-slug -> failed)"
else
  fail "AC1-FR bg lane strict 8-hex: $OUT"
fi

# ---- AC1-FR (sigma-review finding 1): a multi-line .short_id must NOT pass --
# A JSON value with an embedded newline whose 2nd line is 8-hex would slip past a
# line-anchored `grep -qx`; the whole-string `[[ =~ ^...$ ]]` guard rejects it.
reset_log
OUT="$(MOCK_SPAWN_OUT='{"short_id": "junk\ndeadbeef", "provider": "codex", "status": "live"}' MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-x --provider codex --message "build it" --node ab-deadbeef --mode exec)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" != *"result=launched"* ]] \
   && [[ "$OUT" != *"short_id=deadbeef"* ]]; then
  pass "AC1-FR multi-line .short_id (embedded newline) -> failed, no leaked hex line"
else
  fail "AC1-FR multi-line short_id leak: $OUT"
fi

# ---- AC1-EDGE: gemini parity (only --provider differs) ---------------------
reset_log
OUT="$(MOCK_SPAWN_OUT="$(spawn_json beadfeed gemini)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-x --provider gemini --message "Implement ab-deadbeef" --node ab-deadbeef --mode exec)"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=beadfeed"* ]] \
   && grep -q -- "--provider gemini" "$SPAWN_LOG"; then
  pass "AC1-EDGE gemini exec -> same spawn path + JSON receipt (--provider gemini)"
else
  fail "AC1-EDGE gemini parity: $OUT (spawn_log: $(cat "$SPAWN_LOG"))"
fi

# ---- ab-ca822421: prose handoff uses autonomous spawn, never host/once ------
reset_log
handoff_seed='Read /tmp/handoff.md and continue from where it left off. GUARDRAIL: stop before outward actions.'
OUT="$(MOCK_SPAWN_OUT="$(spawn_json handoff1 codex)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name handoff-doc --provider codex --payload-mode handoff --message "$handoff_seed" --mode exec)"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=handoff1"* ]] \
   && [[ "$OUT" == *"mode=spawn"* ]] && grep -q -- "spawn --provider codex" "$SPAWN_LOG" \
   && grep -qF -- "$handoff_seed" "$SPAWN_LOG" && ! grep -q -- "agents host" "$SPAWN_LOG" \
   && ! grep -q -- "--once" "$SPAWN_LOG"; then
  pass "ab-ca822421: codex handoff -> autonomous spawn with seed verbatim"
else
  fail "ab-ca822421 codex handoff route: $OUT (spawn_log: $(cat "$SPAWN_LOG"))"
fi

# Python's pane receipt has no worker-socket short_id. Its real addressable
# handle is the unique registry name, backed by mux_session + pane_id evidence.
reset_log
PANE_RECEIPT='{"name":"handoff-pane","short_id":"","provider":"codex","status":"live","mux_session":"fno-agent-handoff-pane","pane_id":"%7"}'
OUT="$(MOCK_SPAWN_OUT="$PANE_RECEIPT" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name handoff-pane --provider codex --payload-mode handoff --message "$handoff_seed" --mode exec)"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=handoff-pane"* ]] \
   && [[ "$OUT" == *"mode=spawn"* ]]; then
  pass "ab-ca822421: empty-id pane receipt -> verified registry-name handle"
else
  fail "ab-ca822421 empty-id pane receipt: $OUT"
fi

# An empty short_id without matching pane evidence remains a hard failure.
reset_log
BAD_PANE_RECEIPT='{"name":"other-worker","short_id":"","provider":"codex","status":"live","mux_session":"fno-agent-other","pane_id":"%8"}'
OUT="$(MOCK_SPAWN_OUT="$BAD_PANE_RECEIPT" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name handoff-pane --provider codex --payload-mode handoff --message "$handoff_seed" --mode exec)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" != *"result=launched"* ]]; then
  pass "ab-ca822421: mismatched empty-id pane receipt -> failed closed"
else
  fail "ab-ca822421 mismatched pane receipt accepted: $OUT"
fi

reset_log
OUT="$(MOCK_SPAWN_OUT="$(spawn_json handoff2 gemini)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name handoff-doc-gemini --provider gemini --payload-mode handoff --message "$handoff_seed" --mode exec)"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=handoff2"* ]] \
   && [[ "$OUT" == *"mode=spawn"* ]] && grep -q -- "spawn --provider gemini" "$SPAWN_LOG" && ! grep -q -- "agents host" "$SPAWN_LOG" \
   && ! grep -q -- "--once" "$SPAWN_LOG"; then
  pass "ab-ca822421: gemini handoff -> autonomous spawn parity"
else
  fail "ab-ca822421 gemini handoff route: $OUT (spawn_log: $(cat "$SPAWN_LOG"))"
fi

# ---- provider-neutral discuss is a seeded, running pane -------------------
reset_log
discuss_seed='Compare the retry designs and wait for my follow-up.'
OUT="$(MOCK_SPAWN_OUT="$(spawn_json discuss1 codex)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name discuss-retries --provider codex --payload-mode discuss --message "$discuss_seed" --mode exec)"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=discuss1"* ]] \
   && [[ "$OUT" == *"mode=discuss"* ]] && grep -q -- "spawn --provider codex" "$SPAWN_LOG" \
   && grep -qF -- "$discuss_seed" "$SPAWN_LOG" && ! grep -q -- "agents host" "$SPAWN_LOG" \
   && ! grep -q -- "--once" "$SPAWN_LOG"; then
  pass "discuss codex -> seeded interactive pane"
else
  fail "discuss codex route: $OUT (spawn_log: $(cat "$SPAWN_LOG"))"
fi

reset_log
OUT="$(MOCK_SPAWN_OUT="$(spawn_json discuss2 gemini)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name discuss-retries-gemini --provider gemini --payload-mode discuss --message "$discuss_seed" --mode exec)"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"mode=discuss"* ]] \
   && grep -q -- "spawn --provider gemini" "$SPAWN_LOG" && ! grep -q -- "agents host" "$SPAWN_LOG" \
   && ! grep -q -- "--once" "$SPAWN_LOG"; then
  pass "discuss gemini -> seeded interactive pane parity"
else
  fail "discuss gemini route: $OUT (spawn_log: $(cat "$SPAWN_LOG"))"
fi

# ===========================================================================
# US2 - interactive `-i` dispatch -> `fno agents host` (staged, drivable)
# ===========================================================================
# ---- AC2-HP: `-i` (mode=interactive) routes codex -> host ------------------
reset_log
OUT="$(MOCK_SPAWN_OUT="$(spawn_json a1b2c3d4 codex)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-x --provider codex --message "build it" --node ab-deadbeef --mode interactive)"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=a1b2c3d4"* ]] \
   && [[ "$OUT" == *"mode=interactive"* ]] && [[ "$OUT" == *"fno agents grid tgt-x"* ]] \
   && grep -q "host --provider codex" "$SPAWN_LOG" && [[ ! -s "$ASK_LOG" ]]; then
  pass "AC2-HP -i -> host launched (mode=interactive + grid handle), ask not called"
else
  fail "AC2-HP host route: $OUT (spawn_log: $(cat "$SPAWN_LOG"))"
fi

# ---- AC2-UI: staged, not running yet --------------------------------------
reset_log
OUT="$(MOCK_SPAWN_OUT="$(spawn_json feedface codex)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-x --provider codex --message "build it" --node ab-deadbeef --mode interactive)"
if [[ "$OUT" == *"staged="* ]] && [[ "$OUT" == *"not running"* ]]; then
  pass "AC2-UI interactive launch reports staged / not running yet"
else
  fail "AC2-UI staged: $OUT"
fi

# ---- AC2-ERR: host spawn-failed in the readiness window -> failed + bytes --
reset_log
OUT="$(MOCK_SPAWN_ERR="error: auth failed during host settle" MOCK_SPAWN_RC=1 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-x --provider codex --message "build it" --node ab-deadbeef --mode interactive)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" == *"auth failed during host settle"* ]] \
   && [[ "$OUT" != *"short_id="* ]] && [[ "$OUT" != *"result=launched"* ]]; then
  pass "AC2-ERR host spawn-failed -> failed + captured bytes, no short_id"
else
  fail "AC2-ERR host failed: $OUT"
fi

# ---- AC2-EDGE: bare interactive session (no task) is valid -----------------
reset_log
OUT="$(MOCK_SPAWN_OUT="$(spawn_json ba5eba11 codex)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-bare --provider codex --mode interactive)"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=ba5eba11"* ]] \
   && grep -q "host --provider codex" "$SPAWN_LOG"; then
  pass "AC2-EDGE bare interactive host (empty message) -> launched, not a validation error"
else
  fail "AC2-EDGE bare host: $OUT (spawn_log: $(cat "$SPAWN_LOG"))"
fi

# ---- AC2-FR: interactive receipt parsed from JSON; empty short_id never faked
reset_log
OUT="$(MOCK_SPAWN_OUT='{"short_id": "", "provider": "codex", "status": "live"}' MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-x --provider codex --message "build it" --node ab-deadbeef --mode interactive)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" == *"no valid short-id"* ]] \
   && [[ "$OUT" != *"short_id="* ]] && [[ "$OUT" != *"result=launched"* ]]; then
  pass "AC2-FR host empty short_id (exit 0) -> failed, no fabricated handle (parity with AC1-FR)"
else
  fail "AC2-FR host empty short_id: $OUT"
fi

# ===========================================================================
# US3 - --yolo opt-in: appended to the spawn/host argv only when explicit
# ===========================================================================
# ---- AC3-HP: no --yolo -> no --yolo in the spawn argv (sandboxed default) --
reset_log
OUT="$(MOCK_SPAWN_OUT="$(spawn_json a1b2c3d4 codex)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-x --provider codex --message "build it" --node ab-deadbeef --mode exec)"
if [[ "$OUT" == *"result=launched"* ]] && ! grep -q -- "--yolo" "$SPAWN_LOG"; then
  pass "AC3-HP no --yolo -> sandboxed (no --yolo in spawn argv)"
else
  fail "AC3-HP sandboxed default: $OUT (spawn_log: $(cat "$SPAWN_LOG"))"
fi

# ---- AC3-UI/HP: --yolo passed -> appended to the spawn argv ----------------
reset_log
OUT="$(MOCK_SPAWN_OUT="$(spawn_json a1b2c3d4 codex)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-x --provider codex --message "build it" --node ab-deadbeef --mode exec --yolo)"
if [[ "$OUT" == *"result=launched"* ]] && grep -q -- "--yolo" "$SPAWN_LOG"; then
  pass "AC3-HP explicit --yolo -> appended to spawn argv"
else
  fail "AC3-HP --yolo appended: $OUT (spawn_log: $(cat "$SPAWN_LOG"))"
fi

# ---- --yolo also threads into an interactive host launch ------------------
reset_log
OUT="$(MOCK_SPAWN_OUT="$(spawn_json feedface codex)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-x --provider codex --message "build it" --node ab-deadbeef --mode interactive --yolo)"
if [[ "$OUT" == *"result=launched"* ]] && grep -q "host --provider codex" "$SPAWN_LOG" \
   && grep -q -- "--yolo" "$SPAWN_LOG"; then
  pass "AC3 --yolo appended to host argv too"
else
  fail "AC3 --yolo host: $OUT (spawn_log: $(cat "$SPAWN_LOG"))"
fi

# ===========================================================================
# US4 - ask mode -> `fno agents spawn --once` (Group 1 ab-8b3e4fe0)
# ===========================================================================
# `ask` never creates after Group 1, so the one-shot exchange the dispatch
# skill's ask-mode performs lives on `spawn --once`: a client-side
# `codex exec`/`gemini -p` whose stdout is the REPLY verbatim (the teardown
# receipt rides stderr), NOT a short-id. claude has no --once (persistent bg
# threads); claude ask-mode routes to plain `spawn` with the JSON receipt.

# ---- AC4-HP: codex ask-mode -> reply receipt (the reply is the deliverable) -
reset_log
OUT="$(MOCK_SPAWN_OUT=$'Bubble sort is O(n^2) worst/average case,\nO(n) best case on a pre-sorted input.\n' MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-q --provider codex --payload-mode ask --message "what is bubble sort" --mode exec)"
if [[ "$OUT" == *"result=replied"* ]] && [[ "$OUT" == *"Bubble sort is O(n^2)"* ]] \
   && [[ "$OUT" != *"short_id="* ]] && grep -q -- "spawn --provider codex" "$SPAWN_LOG" \
   && grep -q -- "--once" "$SPAWN_LOG" && [[ ! -s "$ASK_LOG" ]]; then
  pass "AC4-HP codex ask-mode -> spawn --once reply receipt, no short-id, ask not called"
else
  fail "AC4-HP codex ask reply: $OUT (spawn_log: $(cat "$SPAWN_LOG"))"
fi

# ---- AC1-EDGE parity: gemini ask -> same reply receipt --------------------
reset_log
OUT="$(MOCK_SPAWN_OUT=$'The retry loop converges after K dry rounds.\n' MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-q --provider gemini --payload-mode ask --message "how does retry converge" --mode exec)"
if [[ "$OUT" == *"result=replied"* ]] && [[ "$OUT" == *"converges after K dry rounds"* ]] \
   && grep -q -- "spawn --provider gemini" "$SPAWN_LOG" && grep -q -- "--once" "$SPAWN_LOG"; then
  pass "gemini ask-mode -> spawn --once reply receipt parity"
else
  fail "gemini ask reply: $OUT"
fi

# ---- AC4-FR: codex ask exits 0 with an empty reply -> failed, never faked --
reset_log
OUT="$(MOCK_SPAWN_OUT="" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-q --provider codex --payload-mode ask --message "what is bubble sort" --mode exec)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" == *"empty reply"* ]] \
   && [[ "$OUT" != *"result=replied"* ]] && [[ "$OUT" != *"result=launched"* ]]; then
  pass "AC4-FR codex ask empty reply (exit 0) -> failed, no fabricated answer"
else
  fail "AC4-FR codex ask empty: $OUT"
fi

# ---- codex ask whitespace-only reply -> failed (not a real answer) ---------
reset_log
OUT="$(MOCK_SPAWN_OUT=$'   \n  \t\n' MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-q --provider codex --payload-mode ask --message "q" --mode exec)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" == *"empty reply"* ]] && [[ "$OUT" != *"result=replied"* ]]; then
  pass "codex ask whitespace-only reply -> failed"
else
  fail "codex ask whitespace reply: $OUT"
fi

# ---- codex ask nonzero exit -> failed + stderr (binary missing) ------------
reset_log
OUT="$(MOCK_SPAWN_ERR="codex: command not found" MOCK_SPAWN_RC=1 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-q --provider codex --payload-mode ask --message "q" --mode exec)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" == *"command not found"* ]] && [[ "$OUT" != *"result=replied"* ]]; then
  pass "codex ask nonzero exit -> failed + stderr"
else
  fail "codex ask nonzero: $OUT"
fi

# ---- claude ask-mode routes to plain spawn (persistent bg; JSON receipt) ----
reset_log
OUT="$(MOCK_SPAWN_OUT="$(claude_receipt c1a0de77 tgt-q)" MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-q --provider claude --payload-mode ask --message "what is bubble sort" --mode exec)"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=c1a0de77"* ]] \
   && [[ "$OUT" != *"result=replied"* ]] && grep -q -- "spawn --provider claude" "$SPAWN_LOG" \
   && ! grep -q -- "--once" "$SPAWN_LOG" && [[ ! -s "$ASK_LOG" ]]; then
  pass "claude ask-mode -> plain spawn (backgrounds, JSON receipt), never --once"
else
  fail "claude ask-mode: $OUT (spawn_log: $(cat "$SPAWN_LOG"))"
fi

# ---- claude spawn non-JSON / non-8-hex output -> failed ---------------------
reset_log
OUT="$(MOCK_SPAWN_OUT=$'deadbeefxyz\n' MOCK_SPAWN_RC=0 MOCK_CLAIM_STATE=free \
       run_spawn --name tgt-q --provider claude --payload-mode ask --message "what is X" --mode exec)"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" != *"short_id="* ]] && [[ "$OUT" != *"result=launched"* ]]; then
  pass "claude spawn non-JSON receipt -> failed"
else
  fail "claude spawn non-JSON receipt: $OUT"
fi

# ===========================================================================
# Guard 3 fail-closed (cv-dddd8ae5): a crashed/garbage `agents list` must NOT
# collapse to "no agent row" and double-launch.
# ===========================================================================
# ---- agents-list crash (rc!=0) -> fail closed, no spawn (free-form/no-node) -
reset_log
OUT="$(MOCK_LIST_RC=7 MOCK_SPAWN_OUT="$(claude_receipt beef0001 tgt-fix-login)" MOCK_SPAWN_RC=0 \
       run_spawn --name tgt-fix-login --provider claude --message "/target fix login no-merge")"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" == *"agents-list probe failed"* ]] \
   && [[ ! -s "$SPAWN_LOG" ]] && [[ "$OUT" != *"result=launched"* ]]; then
  pass "Guard3 agents-list crash (rc!=0) -> fail closed, spawn never called"
else
  fail "Guard3 list crash: $OUT (spawn_log: $(cat "$SPAWN_LOG"))"
fi

# ---- agents-list garbage (unparseable, rc=0) -> fail closed ----------------
reset_log
OUT="$(MOCK_LIST_GARBAGE=1 MOCK_SPAWN_OUT="$(claude_receipt beef0002 tgt-fix-login)" MOCK_SPAWN_RC=0 \
       run_spawn --name tgt-fix-login --provider claude --message "/target fix login no-merge")"
if [[ "$OUT" == *"result=failed"* ]] && [[ "$OUT" == *"no parseable"* ]] \
   && [[ ! -s "$SPAWN_LOG" ]] && [[ "$OUT" != *"result=launched"* ]]; then
  pass "Guard3 agents-list garbage (rc=0, not JSON) -> fail closed, spawn never called"
else
  fail "Guard3 list garbage: $OUT (spawn_log: $(cat "$SPAWN_LOG"))"
fi

# ---- healthy agents-list (empty) still launches (no regression) ------------
reset_log
OUT="$(MOCK_SPAWN_OUT="$(claude_receipt beef0003 tgt-fix-login)" MOCK_SPAWN_RC=0 \
       run_spawn --name tgt-fix-login --provider claude --message "/target fix login no-merge")"
if [[ "$OUT" == *"result=launched"* ]] && [[ "$OUT" == *"short_id=beef0003"* ]]; then
  pass "Guard3 healthy empty list -> launches (no regression)"
else
  fail "Guard3 healthy list regression: $OUT"
fi

echo ""
echo "test_agent_receipt: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
