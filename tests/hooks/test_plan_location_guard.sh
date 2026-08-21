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

# Fake `fno`: only `do plan path --slug X` is used by the resolver.
#
# The stub ASSERTS --slug because the real CLI requires it (`fno do plan path` exits
# on `Missing option '--slug'`). A stub that ignored argv would let a caller drop
# the flag and still go green here, while in production the resolver would fail
# forever and both guards would degrade to fail-open with no test noticing.
FAKEBIN="$TMP/bin"
mkdir -p "$FAKEBIN"
write_fake_fno() {
    cat > "$FAKEBIN/fno" <<EOF
#!/usr/bin/env bash
[[ "\$1" == "do" ]] || exit 1
shift
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

# The frontmatter grammar is node + slug + one of type/deliverable_type/status.
# C1/C2/C3 never reach that third arm, so without this a guard that dropped it
# and blocked every node+slug doc would still look clean.
expect "C8: node+slug without type or status is not a plan" approve \
  "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$TMP/repo/docs/note.md\",\"content\":\"---\\nnode: x-1\\nslug: s\\n---\\n# note\\n\"}}"

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
# A linked worktree of the SAME repo. The guard protects one root - the canonical
# worktree of the session's own git common dir - so this is the cwd that is safe
# to sit in while still being answerable for $CANON's tracked content.
CANON_WT="$TMP/canon-linked"
git -C "$CANON" worktree add -q "$CANON_WT" -b feature/linked >/dev/null 2>&1

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

    # Parsing file_path widened this guard: a write whose TARGET lands in the
    # session's own canonical checkout is judged even from a safe cwd. Only
    # apply_patch paths were before, so this is pinned here.
    expect_wp "E8: file_path into this project's canonical checkout blocked from a safe cwd" block \
      "{\"cwd\":\"$CANON_WT\",\"tool_input\":{\"file_path\":\"$CANON/src/thing.py\"}}"

    # The deliberate boundary on the other side. The protected root is resolved
    # from the SESSION's git common dir, so an unrelated project's checkout is
    # not this guard's object - that project's own hook owns it. Pinned because
    # it is a real narrowing, not an oversight.
    expect_wp "E8b: another project's canonical checkout is not this guard's object" approve \
      "{\"cwd\":\"$TMP/repo\",\"tool_input\":{\"file_path\":\"$CANON/src/thing.py\"}}"

    # ── F. the carve-out cannot disable the gate it carves out of ────────────
    # A plans dir that is an ancestor of the checkout ("." is legal config) would
    # otherwise exempt every write, and a TRACKED plans dir inside the checkout
    # would let plan writes land on the shared branch like any source file.
    # The target MUST be `.md`: the carve-out short-circuits on a non-markdown
    # target before the safety predicate is ever consulted, so a `.py` target
    # here would pass no matter what the predicate said.
    write_fake_fno "$CANON"
    expect_wp "F1: plans dir == repo root does not bypass the gate" block \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"file_path\":\"$CANON/src/thing.md\"}}"

    # ...and a plans dir that is a strict ANCESTOR of the checkout.
    write_fake_fno "$TMP"
    expect_wp "F1b: plans dir above the checkout does not bypass the gate" block \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"file_path\":\"$CANON/src/thing.md\"}}"

    mkdir -p "$CANON/docs/plans" "$CANON/sub"
    : > "$CANON/docs/plans/keep.md"
    git -C "$CANON" add -A >/dev/null 2>&1
    git -C "$CANON" -c user.email=t@t -c user.name=t commit -q -m plans >/dev/null 2>&1
    write_fake_fno "$CANON/docs/plans"
    expect_wp "F2: tracked in-repo plans dir does not carve out" block \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"file_path\":\"$CANON/docs/plans/20260730-p.md\"}}"

    # Same repo, cwd one directory deep. Keyed on the cwd instead of the
    # checkout root, the tracked plans dir reads as "outside the checkout" and
    # the hole reopens - F2 alone cannot see that, since it runs at the root.
    expect_wp "F2b: tracked plans dir still blocked from a subdirectory cwd" block \
      "{\"cwd\":\"$CANON/sub\",\"tool_input\":{\"file_path\":\"$CANON/docs/plans/20260730-p.md\"}}"

    # The DEFAULT config shape: plans dir inside the checkout but git-ignored.
    # Nothing else reaches the check-ignore success branch, so without this a
    # guard that never carved out for default installs would look green.
    mkdir -p "$CANON/.fno/plans"
    printf '.fno/\nfno/\n' > "$CANON/.gitignore"
    git -C "$CANON" add .gitignore >/dev/null 2>&1
    git -C "$CANON" -c user.email=t@t -c user.name=t commit -q -m ignore >/dev/null 2>&1
    write_fake_fno "$CANON/.fno/plans"
    expect_wp "F3: git-ignored in-repo plans dir still carves out" approve \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"file_path\":\"$CANON/.fno/plans/20260730-p.md\"}}"

    # Extension-blind, deliberately. The old `.md` gate existed only to keep a
    # plans-dir lookup off the hot path; the ignore proof that replaced it asks
    # git about the target and has no reason to care what it is called.
    expect_wp "F4: non-md write into an ignored plans dir is allowed" approve \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"file_path\":\"$CANON/.fno/plans/notes.txt\"}}"

    # `/` as a plans dir must never carve out. Belt-and-braces: the explicit
    # refusal in the safety predicate and the containment test (which compares
    # against the literal prefix `//`) each reject it independently, so this
    # pins the OUTCOME rather than either mechanism.
    write_fake_fno ""
    expect_wp "F5: plans dir of / does not carve out" block \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"file_path\":\"$CANON/src/thing.md\"}}"

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

    # ── H. containment cannot be talked around ──────────────────────────────
    write_fake_fno "$PLANS"

    # A `..` component sits under the prefix textually while resolving outside.
    # Without the guard against it, one `..` walks straight out of the plans dir.
    expect "H1: a .. path out of the plans dir is blocked" block \
      "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$PLANS/../../repo/docs/evil.md\",\"content\":\"$PLAN_FM\"}}"

    expect_wp "H1b: a .. path out of the plans dir does not carve out" block \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"file_path\":\"$PLANS/../../../$(basename "$CANON")/src/thing.md\"}}"

    # H1/H1b run where every intermediate directory exists, so `cd -P` folds the
    # `..` correctly and they pass either way. The refusal is load-bearing only
    # when the `..` sits in the MISSING tail: there is nothing on disk to resolve
    # it against, so it is carried into the compared string and the path reads as
    # inside the plans dir while resolving one level above it.
    write_fake_fno "$ABSENT"
    expect "H1c: a .. inside an absent plans dir tail is blocked" block \
      "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$ABSENT/../escape.md\",\"content\":\"$PLAN_FM\"}}"

    expect_wp "H1d: the same .. tail does not carve out" block \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"file_path\":\"$ABSENT/../escape.md\"}}"

    write_fake_fno "$PLANS"

    # A symlink LEADING OUT of an existing plans dir must not carve itself out:
    # the physical resolver must follow the directory link OUT of the plans dir.
    ln -s "$CANON/src" "$PLANS/sneak" 2>/dev/null
    expect_wp "H2: symlink out of the plans dir does not carve out" block \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"file_path\":\"$PLANS/sneak/evil.md\"}}"

    # H2 only covers a DIRECTORY link. A link to a FILE fails the `-d` test the
    # walk-up keys on, so without an explicit dereference it is carried along
    # unresolved and the target reads as inside the plans dir. A vault full of
    # symlinked notes makes this the ordinary shape, not an exotic one.
    : > "$CANON/src/thing.md"
    ln -s "$CANON/src/thing.md" "$PLANS/decoy.md" 2>/dev/null
    expect_wp "H2b: leaf file symlink out of the plans dir does not carve out" block \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"file_path\":\"$PLANS/decoy.md\"}}"

    expect "H2c: leaf file symlink is judged at its destination" block \
      "{\"tool_name\":\"Write\",\"cwd\":\"$CANON\",\"tool_input\":{\"file_path\":\"$PLANS/decoy.md\",\"content\":\"$PLAN_FM\"}}"

    # A sibling sharing the prefix is not inside it (the trailing-slash rule).
    mkdir -p "${PLANS}-archive"
    expect "H3: a prefix-sharing sibling dir is still outside" block \
      "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"${PLANS}-archive/p.md\",\"content\":\"$PLAN_FM\"}}"

    # ── I. the plans dir is resolved from the SESSION cwd ────────────────────
    # The default stub is cwd-blind, so nothing above can tell "resolved from
    # the payload cwd" from "resolved from wherever the hook was spawned" - the
    # same class of hole as an argv-blind stub. This one answers by $PWD.
    cat > "$FAKEBIN/fno" <<'STUBEOF'
#!/usr/bin/env bash
[[ "$1" == "do" ]] || exit 1
shift
if [[ "$1" == "plan" && "$2" == "path" ]]; then
  case " $* " in
    *" --slug "*) ;;
    *) echo "Missing option '--slug'." >&2; exit 2 ;;
  esac
  echo "$PWD/fno/plans/20260730-x.md"
  exit 0
fi
exit 1
STUBEOF
    chmod +x "$FAKEBIN/fno"
    mkdir -p "$TMP/projB/fno/plans"

    # Correct save for projB. Resolved from the payload cwd this is inside the
    # plans dir; resolved from the harness cwd it is outside and gets denied.
    expect "I1: positive guard resolves from the payload cwd" approve \
      "{\"tool_name\":\"Write\",\"cwd\":\"$TMP/projB\",\"tool_input\":{\"file_path\":\"$TMP/projB/fno/plans/p.md\",\"content\":\"$PLAN_FM\"}}"

    mkdir -p "$CANON/fno/plans"
    expect_wp "I2: carve-out resolves from the payload cwd" approve \
      "{\"cwd\":\"$CANON\",\"tool_input\":{\"file_path\":\"$CANON/fno/plans/p.md\"}}"

    # ── J. a half-sourced helper degrades, never opens ───────────────────────
    # bash defines functions as it parses, so a truncated helper leaves the
    # first defined and the rest missing. The guard must fall back to the blunt
    # behavior, not lose its per-target check.
    HALF="$TMP/half"
    mkdir -p "$HALF/helpers"
    cp "$REPO_ROOT/hooks/worktree-write-protect.sh" "$HALF/"
    cp "$REPO_ROOT/hooks/helpers/check-impl-location.sh" "$HALF/helpers/"
    sed -n '/^fno_plans_dir()/,/^}/p' "$REPO_ROOT/hooks/helpers/plans-dir.sh" > "$HALF/helpers/plans-dir.sh"
    out=$(printf '%s' "{\"cwd\":\"$CANON\",\"tool_input\":{\"file_path\":\"$PLANS/20260730-p.md\"}}" \
        | PATH="$FAKE_PATH" bash "$HALF/worktree-write-protect.sh" 2>/dev/null)
    if printf '%s' "$out" | grep -q '"block"'; then
        pass "J1: half-sourced helper falls back to the blunt block"
    else
        fail "J1: half-sourced helper approved: $out"
    fi

    # J1 blocks on the cwd gate alone, so it cannot see whether the PER-TARGET
    # check survived. From a SAFE cwd in the same repo only the per-target check
    # can block, so this is what proves the degraded mode is not weaker than the
    # guard was before the helper existed - without the physical resolver the
    # guard cannot see through a symlink, so it must deny, never skip.
    out=$(printf '%s' "{\"cwd\":\"$CANON_WT\",\"tool_input\":{\"file_path\":\"$CANON/src/thing.py\"}}" \
        | PATH="$FAKE_PATH" bash "$HALF/worktree-write-protect.sh" 2>/dev/null)
    if printf '%s' "$out" | grep -q '"block"'; then
        pass "J2: half-sourced helper keeps the per-target check"
    else
        fail "J2: per-target check lost with a half-sourced helper: $out"
    fi

    # The positive guard must fail OPEN on the same input, per its own contract.
    # A missing containment test reads as "not under the plans dir", so without a
    # completeness check it would DENY the correct save while naming that very
    # directory as the right destination.
    cp "$REPO_ROOT/hooks/plan-location-guard.sh" "$HALF/"
    out=$(printf '%s' "{\"tool_name\":\"Write\",\"cwd\":\"$TMP/repo\",\"tool_input\":{\"file_path\":\"$PLANS/20260730-p.md\",\"content\":\"$PLAN_FM\"}}" \
        | PATH="$FAKE_PATH" bash "$HALF/plan-location-guard.sh" 2>/dev/null)
    if [[ "$out" == "{}" ]]; then
        pass "J3: half-sourced helper leaves the positive guard fail-open"
    else
        fail "J3: positive guard denied a correct save on a half-sourced helper: $out"
    fi

    write_fake_fno "$PLANS"
fi

printf '\n[plg] %d passed, %d failed\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]]
