#!/usr/bin/env bash
# test_init_contested_steal_guard.sh
#
# init-target-state.sh steal guard (x-ba4b). A prior session's manifest is
# archived+reclaimed only when its node claim is free/stale AND the worktree
# shows NO fresh activity. The bug (x-e780) was a LIVE session under a dead
# supervisor pid whose claim read non-live, then got archived and its node
# stolen. Contested liveness must degrade to a BLOCKED refusal, never a steal.
#
# Covers:
#   (A) Contested: a free/stale claim + a freshly-modified tracked file =>
#       init emits `RESULT: BLOCKED` reason=contested and PRESERVES the prior
#       manifest (no target-state.terminal.* archive).
#   (B) Abandoned: a free/stale claim + no fresh activity => init archives the
#       prior manifest exactly as before (a target-state.terminal.* appears).
#
# Exit codes: 0 pass / 1 assertion failed / 77 skipped (missing deps)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INIT="${REPO_ROOT}/hooks/helpers/init-target-state.sh"

log()  { printf '[steal-guard] %s\n' "$*"; }
fail() { printf '[steal-guard] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[steal-guard] PASS: %s\n' "$*"; }
skip() { printf '[steal-guard] SKIP: %s\n' "$*" >&2; exit 77; }

command -v git >/dev/null 2>&1 || skip "git not on PATH"
command -v fno >/dev/null 2>&1 || skip "fno not on PATH (claim status required)"
[[ -f "$INIT" ]] || fail "init script not found at $INIT"

bash -n "$INIT" || fail "bash -n rejected $INIT (syntax error)"
pass "init script passes bash -n"

_ALL_TMPS=()
trap 'rm -rf "${_ALL_TMPS[@]}"' EXIT

NODE="tst-steala"

# Isolate the node:<id> claim under a per-repo FNO_CLAIMS_ROOT (node: keys route
# there ahead of $HOME) so `fno claim status` reads `free` without touching the
# real home. HOME is left REAL on purpose: faking it makes `fno` reprovision
# from scratch (slow, and needs disk), whereas the claim root is all we must
# isolate for this guard.
make_repo() {
  local _varname="$1" _dir
  _dir="$(mktemp -d -t steal-guard.XXXXXX)" || fail "mktemp failed"
  eval "${_varname}=\"\${_dir}\""
  (
    cd "$_dir" || exit 1
    git init -q
    git config user.email t@t && git config user.name t
    mkdir -p .fno claims-root
    printf '# isolated\n' > .fno/config.toml
    printf 'work\n' > work.txt
    git add work.txt && git commit -qm init
    # A valid prior-session manifest (NO terminal status so the reap decision
    # falls through to the claim/activity check). The claim fields are APPENDED
    # AFTER the closing frontmatter marker - init's real shape (x-ba4b), which
    # the archive block reads by scanning the whole file, not the frontmatter.
    cat > .fno/target-state.md <<EOF
---
session_id: prior-session-0000
created_at: 2026-07-03T00:00:00Z
attended: false
---
Immutable session manifest.
target_claim_key: "node:${NODE}"
target_claim_holder: "target-session:prior-session-0000"
target_claim_ttl: "2h"
EOF
  ) || fail "repo setup failed in $_dir"
}

run_init() {  # $1 = repo dir. stdout+stderr -> $2 file
  local dir="$1" outfile="$2"
  ( cd "$dir" && \
    FNO_CLAIMS_ROOT="${dir}/claims-root" \
    TARGET_START=1 \
    TARGET_INPUT="$NODE" \
    TARGET_LOCATION_OK="main-acknowledged" \
    bash "$INIT" ) >"$outfile" 2>&1 || true
}

# ── (A) contested: fresh activity => BLOCKED, manifest preserved ──────
log "(A): free claim + fresh tracked-file activity => contested BLOCKED, no archive"
make_repo TMP_A
_ALL_TMPS+=("$TMP_A")
# Modify the tracked file so `git diff --name-only HEAD` lists it with a
# now-fresh mtime (well inside the default 900s window).
printf 'edited by a live session\n' >> "${TMP_A}/work.txt"

OUT_A="${TMP_A}/out.txt"
run_init "$TMP_A" "$OUT_A"

grep -q '^RESULT: BLOCKED' "$OUT_A" \
  || fail "(A): expected 'RESULT: BLOCKED' on a contested worktree; got: $(cat "$OUT_A")"
grep -qi 'contested' "$OUT_A" \
  || fail "(A): BLOCKED reason did not mention 'contested'"
pass "(A): init refused with RESULT: BLOCKED reason=contested"

[[ -f "${TMP_A}/.fno/target-state.md" ]] \
  || fail "(A): prior manifest was removed (must be preserved on refusal)"
if compgen -G "${TMP_A}/.fno/target-state.terminal.*.md" >/dev/null; then
  fail "(A): prior manifest was archived (steal) despite fresh activity"
fi
pass "(A): prior manifest preserved, not archived (no steal)"

# ── (B) abandoned: no fresh activity => reap (archive) ────────────────
log "(B): free claim + no fresh activity => prior manifest archived"
make_repo TMP_B
_ALL_TMPS+=("$TMP_B")
# No tracked file modified, no scratchpad => newest mtime is 0 => not fresh
# under any window, so the default 15m is enough to prove the abandoned path.
OUT_B="${TMP_B}/out.txt"
run_init "$TMP_B" "$OUT_B"

if grep -q '^RESULT: BLOCKED' "$OUT_B"; then
  fail "(B): init refused as contested on an abandoned worktree; got: $(cat "$OUT_B")"
fi
if ! compgen -G "${TMP_B}/.fno/target-state.terminal.*.md" >/dev/null; then
  fail "(B): prior manifest was NOT archived on an abandoned worktree (stranded)"
fi
pass "(B): abandoned prior manifest reclaimed exactly as before (archived)"

log "All steal-guard scenarios passed"
