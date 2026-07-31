#!/usr/bin/env bash
# test_plan_location_guard.sh - both halves of the plan-save-location gate
# (x-5349): the positive guard (plan-location-guard.sh) and the plans-dir
# carve-out in the negative guard (worktree-write-protect.sh).
#
# Self-contained. A fake `fno` on PATH stands in for the real resolver so the
# tests pin behavior against a known plans dir instead of the developer's
# config; a case that needs the "unresolvable" branch drops it from PATH.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
GUARD="${REPO_ROOT}/hooks/plan-location-guard.sh"
WPROTECT="${REPO_ROOT}/hooks/worktree-write-protect.sh"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); printf '[plg] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[plg] FAIL: %s\n' "$*" >&2; }

[[ -f "$GUARD" ]] || { fail "guard not found at $GUARD"; exit 1; }
[[ -f "$WPROTECT" ]] || { fail "write-protect not found at $WPROTECT"; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# A plans dir on disk (physical), plus a symlink to it: the real deployment
# reaches the vault through internal/ and the prefix test must survive that.
PLANS="$TMP/vault/fno/plans"
mkdir -p "$PLANS" "$TMP/repo/docs" "$TMP/repo/cli/tests/fixtures/plans"
ln -s "$TMP/vault" "$TMP/repo/internal"
PLANS_PHYS="$(cd -P "$PLANS" && pwd -P)"

# Fake `fno`: only `plan path --slug X` is used by the resolver.
#
# The stub ASSERTS --slug because the real CLI requires it (`fno plan path` exits
# on `Missing option '--slug'`). A stub that ignored argv would let a caller drop
# the flag and still go green here, while in production the resolver would fail
# forever and both guards would degrade to fail-open with no test noticing.
FAKEBIN="$TMP/bin"
mkdir -p "$FAKEBIN"
write_fake_fno() {
    cat > "$FAKEBIN/fno" <<EOF
#!/usr/bin/env bash
if [[ "\$1" == "plan" && "\$2" == "path" ]]; then
  case " \$* " in
    *" --slug "*) ;;
    *) echo "Missing option '--slug'." >&2; exit 2 ;;
  esac
  echo "$1/20260730-x.md"
  exit 0
fi
exit 1
EOF
    chmod +x "$FAKEBIN/fno"
}
write_fake_fno "$PLANS"
FAKE_PATH="$FAKEBIN:$PATH"

PLAN_FM='---\nnode: x-5349\nslug: some-plan\ntype: feature\nstatus: ready\n---\n\n# Plan\n'

# decision_of GUARD PATH_ENV PAYLOAD -> "block" | "approve" | "MISSING"
decision_of() {
    local guard="$1" path_env="$2" payload="$3" out
    out=$(printf '%s' "$payload" | PATH="$path_env" bash "$guard" 2>/dev/null)
    if [[ "$out" == "{}" ]]; then echo approve
    elif printf '%s' "$out" | grep -q '"block"'; then echo block
    else echo MISSING; fi
}

# expect NAME WANT PAYLOAD  (positive guard, fake fno present)
expect() {
    local name="$1" want="$2" got
    got=$(decision_of "$GUARD" "$FAKE_PATH" "$3")
    if [[ "$got" == "$want" ]]; then pass "$name ($got)"; else fail "$name: want $want got $got"; fi
}

# expect_wp NAME WANT PAYLOAD  (negative guard carve-out)
expect_wp() {
    local name="$1" want="$2" got
    got=$(decision_of "$WPROTECT" "$FAKE_PATH" "$3")
    if [[ "$got" == "$want" ]]; then pass "$name ($got)"; else fail "$name: want $want got $got"; fi
}

# ── T0: syntax ────────────────────────────────────────────────────────────────
bash -n "$GUARD"    2>/dev/null && pass "T0: plan-location-guard syntax"    || fail "T0: guard syntax error"
bash -n "$WPROTECT" 2>/dev/null && pass "T0: worktree-write-protect syntax" || fail "T0: write-protect syntax error"
bash -n "${REPO_ROOT}/hooks/helpers/plans-dir.sh" 2>/dev/null && pass "T0: plans-dir helper syntax" || fail "T0: helper syntax error"

# ── A. positive guard: plan-shaped Write outside plans_dir is blocked ─────────
expect "A1: plan frontmatter into docs/ blocked" block \
  "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$TMP/repo/docs/my-plan.md\",\"content\":\"$PLAN_FM\"}}"

expect "A2: same plan into the plans dir approved" approve \
  "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$PLANS/20260730-my-plan.md\",\"content\":\"$PLAN_FM\"}}"

# The deployment path: plans dir reached through a symlink.
expect "A3: plans dir via symlink approved" approve \
  "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$TMP/repo/internal/fno/plans/20260730-my-plan.md\",\"content\":\"$PLAN_FM\"}}"

# ── B. plans-glob half: a plans path outside the configured dir ──────────────
expect "B1: docs/plans/ path blocked even without frontmatter" block \
  "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$TMP/repo/docs/plans/thing.md\",\"content\":\"# no frontmatter\\n\"}}"

expect "B2: apply_patch Add File on a plans path blocked" block \
  "{\"tool_name\":\"Write\",\"tool_input\":{\"command\":\"*** Begin Patch\\n*** Add File: docs/plans/thing.md\\n*** End Patch\"}}"

# ── C. no false positives ────────────────────────────────────────────────────
expect "C1: ordinary md write approved" approve \
  "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$TMP/repo/docs/readme.md\",\"content\":\"# hello\\n\"}}"

# Example frontmatter quoted in a doc BODY is not a leading frontmatter block.
expect "C2: doc quoting example frontmatter approved" approve \
  "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$TMP/repo/docs/guide.md\",\"content\":\"# Guide\\n\\nExample:\\n\\n---\\nnode: x-1\\nslug: s\\ntype: feature\\n---\\n\"}}"

expect "C3: test fixture plan approved" approve \
  "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$TMP/repo/cli/tests/fixtures/plans/sample.md\",\"content\":\"$PLAN_FM\"}}"

expect "C4: Edit is not gated (location is already history)" approve \
  "{\"tool_name\":\"Edit\",\"tool_input\":{\"file_path\":\"$TMP/repo/docs/my-plan.md\",\"old_string\":\"a\",\"new_string\":\"b\"}}"

# C4 alone does not prove the exclusion: its payload has no content AND its path
# is not a plans path, so it would pass even if Edit were gated. This one is on a
# path B1 blocks, so it goes red the moment the Write-only condition is dropped -
# load-bearing because codex wires this guard on `Edit|Write`.
expect "C4b: Edit on a plans path outside the plans dir still approved" approve \
  "{\"tool_name\":\"Edit\",\"tool_input\":{\"file_path\":\"$TMP/repo/docs/plans/thing.md\",\"old_string\":\"a\",\"new_string\":\"b\"}}"

expect "C5: non-md write approved" approve \
  "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$TMP/repo/src/main.py\",\"content\":\"print(1)\"}}"

# ── C6/C7: relative targets anchor to the PAYLOAD cwd, not the hook's own ────
# Chosen so the two anchorings disagree: relative to the payload cwd this lands
# INSIDE the plans dir, while against any other cwd it lands outside and the
# frontmatter condition fires. A guard reading its own `pwd` goes red on C6.
expect "C6: relative path under the payload cwd approved" approve \
  "{\"tool_name\":\"Write\",\"cwd\":\"$TMP/vault\",\"tool_input\":{\"file_path\":\"fno/plans/rel.md\",\"content\":\"$PLAN_FM\"}}"

expect "C7: same relative path from another cwd blocked" block \
  "{\"tool_name\":\"Write\",\"cwd\":\"$TMP/repo\",\"tool_input\":{\"file_path\":\"fno/plans/rel.md\",\"content\":\"$PLAN_FM\"}}"

# ── D. fails open when the plans dir cannot be resolved ──────────────────────
# Without `fno` the convention cannot be stated, so the guard must not block.
NOFNO_PATH="/usr/bin:/bin:/usr/sbin:/sbin"
got=$(decision_of "$GUARD" "$NOFNO_PATH" \
  "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$TMP/repo/docs/my-plan.md\",\"content\":\"$PLAN_FM\"}}")
[[ "$got" == "approve" ]] && pass "D1: unresolvable plans dir fails open ($got)" || fail "D1: want approve got $got"

# ── E. the carve-out: the reported bug ───────────────────────────────────────
# A session on canonical main writing INTO the plans dir was denied; that denial
# is what drove the .fno/drafts-then-mv workaround.
#
# Built hermetically on a throwaway canonical repo pinned to `main`, NOT on this
# checkout: the suite runs from a feature worktree, so keying on the real
# checkout would skip exactly the cases that carry the fix.
CANON="$TMP/canon"
mkdir -p "$CANON/src"
git init -q -b main "$CANON" 2>/dev/null
git -C "$CANON" -c user.email=t@t -c user.name=t commit -q --allow-empty -m init 2>/dev/null
CANON_BRANCH="$(git -C "$CANON" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")"

if [[ "$CANON_BRANCH" != "main" ]]; then
    fail "E: fixture repo is on '${CANON_BRANCH:-unknown}', expected main"
else
    pass "E0: fixture canonical repo on main"

    # Every case below uses cwd=$CANON. An earlier version ran E1/E2 from a
    # non-git temp dir, where _block_if_canonical returns before reaching any
    # verdict - so both passed with the carve-out deleted and proved nothing.
    expect_wp "E1: canonical-main plan write approved (the bug)" approve \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"command\":\"*** Begin Patch\\n*** Add File: $PLANS/20260730-p.md\\n*** End Patch\"}}"

    # The file_path half of the carve-out. Distinct from E1: that payload shape
    # was not parsed at all before this change, so nothing else covers it.
    expect_wp "E2: canonical-main plan write via file_path approved" approve \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"file_path\":\"$PLANS/20260730-p.md\"}}"

    # Positive control: without the carve-out this cwd blocks everything, so a
    # blocked source write proves the gate is live and E1/E2 are real changes.
    expect_wp "E3: canonical-main source write still blocked" block \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"command\":\"*** Begin Patch\\n*** Add File: src/thing.py\\n*** End Patch\"}}"

    # The carve-out must not become a general bypass: a mixed patch that also
    # touches source still faces the location gate.
    expect_wp "E4: mixed plan+source patch still blocked" block \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"command\":\"*** Begin Patch\\n*** Add File: $PLANS/20260730-p.md\\n*** Add File: src/thing.py\\n*** End Patch\"}}"

    # An unparseable payload keeps the old blunt behavior rather than opening a
    # hole: no known targets means no carve-out.
    expect_wp "E5: no parseable target keeps the blunt block" block \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"command\":\"echo hi\"}}"

    # All four apply_patch header kinds feed the carve-out, not just Add File.
    # Pinned in both directions so a header-set change cannot drift unnoticed.
    expect_wp "E6: Update File inside the plans dir approved" approve \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"command\":\"*** Begin Patch\\n*** Update File: $PLANS/20260730-p.md\\n*** End Patch\"}}"

    expect_wp "E7: Update File on canonical source still blocked" block \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"command\":\"*** Begin Patch\\n*** Update File: src/thing.py\\n*** End Patch\"}}"

    # Parsing file_path widened this guard: a write whose TARGET lands in a
    # canonical checkout is now judged even from a safe cwd. Previously only
    # apply_patch paths were, so this is new behavior and is pinned here.
    expect_wp "E8: file_path into a canonical checkout blocked from a safe cwd" block \
      "{\"cwd\":\"$TMP/repo\",\"tool_input\":{\"file_path\":\"$CANON/src/thing.py\"}}"

    # ── F. the carve-out cannot disable the gate it carves out of ────────────
    # A plans dir that is an ancestor of the checkout ("." is legal config) would
    # otherwise exempt every write, and a TRACKED plans dir inside the checkout
    # would let plan writes land on the shared branch like any source file.
    write_fake_fno "$CANON"
    expect_wp "F1: plans dir == repo root does not bypass the gate" block \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"command\":\"*** Begin Patch\\n*** Add File: src/thing.py\\n*** End Patch\"}}"

    mkdir -p "$CANON/docs/plans"
    : > "$CANON/docs/plans/keep.md"
    git -C "$CANON" add -A >/dev/null 2>&1
    git -C "$CANON" -c user.email=t@t -c user.name=t commit -q -m plans >/dev/null 2>&1
    write_fake_fno "$CANON/docs/plans"
    expect_wp "F2: tracked in-repo plans dir does not carve out" block \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"file_path\":\"$CANON/docs/plans/20260730-p.md\"}}"

    # ── G. a plans dir that does not exist yet ───────────────────────────────
    # The first plan saved in any fresh clone or worktree. Resolving a path to
    # its deepest EXISTING ancestor lands on the plans dir's PARENT, which made
    # the guard call the one correct destination "outside" itself and made the
    # carve-out silently never fire.
    ABSENT="$TMP/never-created/fno/plans"
    write_fake_fno "$ABSENT"
    expect "G1: correct save into an absent plans dir approved" approve \
      "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$ABSENT/20260730-my-plan.md\",\"content\":\"$PLAN_FM\"}}"

    expect_wp "G2: carve-out still fires for an absent plans dir" approve \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"file_path\":\"$ABSENT/20260730-p.md\"}}"

    write_fake_fno "$PLANS"
fi

printf '\n[plg] %d passed, %d failed\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]]
