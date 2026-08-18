#!/usr/bin/env bash
# Tests for the destination-gating pre-push hook and its installer.
# Exercises the REPO copies of hooks/pre-push.sh and
# scripts/install-pre-push-hook.sh with a sandboxed HOME and temp git repos;
# the host's real hooks are never touched. The pre-push contract is entirely
# stdin plus exit code, so hook cases pipe refspec lines directly and need no
# remote and no real push.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/pre-push.sh"
INSTALLER="$REPO_ROOT/scripts/install-pre-push-hook.sh"
PASS=0
FAIL=0
Z="0000000000000000000000000000000000000000"

# ---- prereq checks ----

if [[ ! -f "$HOOK" || ! -f "$INSTALLER" ]]; then
    echo "SKIP: $HOOK or $INSTALLER not found"
    exit 0
fi

# ---- test helpers ----

pass() { echo "  PASS: $1"; ((PASS++)); }
fail() { echo "  FAIL: $1"; ((FAIL++)); }

# run_hook <stdin-lines...>: feed the hook like git does. Any extra args are
# passed through as the $1/$2 (remote, url) git supplies.
run_hook() {
    local stdin="$1"; shift
    printf '%s' "$stdin" | bash "$HOOK" "$@" 2>&1
}

# ---- sandbox setup ----

TMP=$(mktemp -d)
trap "rm -rf '$TMP'" EXIT
export HOME="$TMP/home"
mkdir -p "$HOME" "$TMP/repos"

new_repo() {
    local r="$TMP/repos/$1"
    mkdir -p "$r"
    git -C "$r" init -q --initial-branch=main 2>/dev/null \
        || { git -C "$r" init -q && git -C "$r" checkout -q -b main; }
    git -C "$r" -c user.email=t@t -c user.name=t commit -q --allow-empty -m init
    echo "$r"
}

slot() {  # the common-dir hooks/pre-push path of repo $1
    local c
    c="$(git -C "$1" rev-parse --git-common-dir)"
    case "$c" in /*) ;; *) c="$1/$c" ;; esac
    echo "$c/hooks/pre-push"
}

# ---- Hook cases: run from a checkout on main ----

MAIN_REPO="$(new_repo h-main)"
cd "$MAIN_REPO"

# Case 1: checkout on main, backup destination -> allowed, silent.
out="$(run_hook "refs/heads/work abc123 refs/heads/backup/anything $Z
")"; rc=$?
if [[ $rc -eq 0 && -z "$out" ]]; then
    pass "case 1: checkout on main + backup destination -> exit 0, silent"
else
    fail "case 1: expected exit 0 and no output, got rc=$rc out='$out'"
fi

# Case 2: destination is refs/heads/main -> refused, names the destination.
out="$(run_hook "refs/heads/main abc123 refs/heads/main $Z
" origin git@github.com:example/example.git)"; rc=$?
if [[ $rc -eq 1 && "$out" == *"refs/heads/main"* && "$out" != *--no-verify* ]]; then
    pass "case 2: refs/heads/main destination -> exit 1, names refs/heads/main, no hook-skipping flag"
else
    fail "case 2: expected exit 1 naming refs/heads/main with no flag advice, got rc=$rc out='$out'"
fi

# Case 3: feature/main -> allowed. The basename reduction is gone.
out="$(run_hook "refs/heads/feature/main abc123 refs/heads/feature/main $Z
")"; rc=$?
if [[ $rc -eq 0 && -z "$out" ]]; then
    pass "case 3: refs/heads/feature/main destination -> exit 0"
else
    fail "case 3: expected exit 0 silent, got rc=$rc out='$out'"
fi

# Case 4: delete of a protected branch -> refused by the same destination test.
out="$(run_hook "(delete) $Z refs/heads/main $Z
")"; rc=$?
if [[ $rc -eq 1 && "$out" == *"refs/heads/main"* ]]; then
    pass "case 4: (delete) line writing refs/heads/main -> exit 1"
else
    fail "case 4: expected exit 1 naming refs/heads/main, got rc=$rc out='$out'"
fi

# Case 5: a tag is not a branch write -> allowed.
out="$(run_hook "refs/tags/v1.0.0 abc123 refs/tags/v1.0.0 $Z
")"; rc=$?
if [[ $rc -eq 0 && -z "$out" ]]; then
    pass "case 5: refs/tags/v1.0.0 -> exit 0"
else
    fail "case 5: expected exit 0 silent, got rc=$rc out='$out'"
fi

# Case 6: empty stdin -> allowed.
out="$(run_hook "")"; rc=$?
if [[ $rc -eq 0 && -z "$out" ]]; then
    pass "case 6: empty stdin -> exit 0"
else
    fail "case 6: expected exit 0 silent, got rc=$rc out='$out'"
fi

# Case 7: every stdin line is judged, not just the first.
out="$(run_hook "refs/heads/a abc123 refs/heads/backup/x $Z
refs/heads/b def456 refs/heads/dev $Z
")"; rc=$?
if [[ $rc -eq 1 && "$out" == *"refs/heads/dev"* ]]; then
    pass "case 7: second line writing refs/heads/dev -> exit 1 naming it"
else
    fail "case 7: expected exit 1 naming refs/heads/dev, got rc=$rc out='$out'"
fi

# Case 8: regression - the current-branch arm must not come back.
if grep -q 'symbolic-ref' "$HOOK"; then
    fail "case 8: hooks/pre-push.sh reintroduced the current-branch arm (symbolic-ref found: $(grep -n 'symbolic-ref' "$HOOK" | head -1))"
else
    pass "case 8: no symbolic-ref call in hook source"
fi
if grep -q -- '--no-verify' "$HOOK"; then
    fail "case 8b: hook remedy text advertises the hook-skipping flag"
else
    pass "case 8b: no hook-skipping flag in hook source"
fi

# ---- Installer cases ----

# Case 9: --check with no hook -> exit 1, names absent.
R1="$(new_repo i-fresh)"
SLOT="$(slot "$R1")"
out="$(cd "$R1" && bash "$INSTALLER" --check 2>&1)"; rc=$?
if [[ $rc -eq 1 && "$out" == *absent* ]]; then
    pass "case 9: --check with empty slot -> exit 1, absent"
else
    fail "case 9: expected exit 1 absent, got rc=$rc out='$out'"
fi

# Case 10: install -> symlink at the common-dir slot, then --check exits 0.
out="$(cd "$R1" && bash "$INSTALLER" 2>&1)"; rc=$?
if [[ $rc -eq 0 && -L "$SLOT" && "$(readlink "$SLOT")" == "$HOOK" ]]; then
    pass "case 10: install -> common-dir hooks/pre-push symlinks the repo hook"
else
    fail "case 10: expected exit 0 with symlink at $SLOT, got rc=$rc out='$out'"
fi
out="$(cd "$R1" && bash "$INSTALLER" --check 2>&1)"; rc=$?
if [[ $rc -eq 0 && "$out" == *installed* ]]; then
    pass "case 10b: --check after install -> exit 0"
else
    fail "case 10b: expected exit 0 installed, got rc=$rc out='$out'"
fi

# Case 11: re-run -> no change, idempotent.
out="$(cd "$R1" && bash "$INSTALLER" 2>&1)"; rc=$?
if [[ $rc -eq 0 && "$out" == *"no change"* && "$(readlink "$SLOT")" == "$HOOK" ]]; then
    pass "case 11: second install run -> no change, symlink unchanged"
else
    fail "case 11: expected no-change idempotence, got rc=$rc out='$out'"
fi

# Case 12: legacy current-branch hook -> --check names symbolic-ref, install
# backs it up and replaces it.
R2="$(new_repo i-legacy)"
SLOT2="$(slot "$R2")"
mkdir -p "$(dirname "$SLOT2")"
cat >"$SLOT2" <<'EOF'
#!/bin/sh
BRANCH=$(git symbolic-ref HEAD | sed -e 's,.*/\(.*\),\1,')
[ "$BRANCH" = "main" ] && echo "PUSH BLOCKED: direct push to main" && exit 1
exit 0
EOF
chmod +x "$SLOT2"
out="$(cd "$R2" && bash "$INSTALLER" --check 2>&1)"; rc=$?
if [[ $rc -eq 1 && "$out" == *symbolic-ref* ]]; then
    pass "case 12: --check against legacy hook -> exit 1 naming symbolic-ref"
else
    fail "case 12: expected exit 1 naming symbolic-ref, got rc=$rc out='$out'"
fi
out="$(cd "$R2" && bash "$INSTALLER" 2>&1)"; rc=$?
BACKUPS=("$SLOT2".backup.*)
if [[ $rc -eq 0 && -L "$SLOT2" && -f "${BACKUPS[0]}" ]] \
    && grep -q 'symbolic-ref' "${BACKUPS[0]}"; then
    pass "case 12b: legacy real file backed up to ${BACKUPS[0]##*/}, symlink replaces it"
else
    fail "case 12b: expected backup sibling + symlink, got rc=$rc out='$out'"
fi

# Case 13: core.hooksPath elsewhere -> names the effective path, guard NOT
# reported active.
R3="$(new_repo i-hookspath)"
mkdir -p "$R3/otherhooks"
git -C "$R3" config core.hooksPath "$R3/otherhooks"
out="$(cd "$R3" && bash "$INSTALLER" 2>&1)"; rc=$?
if [[ $rc -eq 0 && "$out" == *"$R3/otherhooks/pre-push"* && "$out" == *"NOT reported active"* ]]; then
    pass "case 13: hooksPath divergence -> effective path named, guard not reported active"
else
    fail "case 13: expected effective path + NOT reported active, got rc=$rc out='$out'"
fi

# Case 14: a foreign hook (neither ours nor legacy) -> --check exits 1.
R4="$(new_repo i-foreign)"
SLOT4="$(slot "$R4")"
mkdir -p "$(dirname "$SLOT4")"
printf '#!/bin/sh\nexit 0\n' >"$SLOT4"
out="$(cd "$R4" && bash "$INSTALLER" --check 2>&1)"; rc=$?
if [[ $rc -eq 1 && "$out" == *foreign* ]]; then
    pass "case 14: --check against a foreign hook -> exit 1, foreign"
else
    fail "case 14: expected exit 1 foreign, got rc=$rc out='$out'"
fi

# Case 15: outside any git repo -> --check exits 1, not-a-repo.
out="$(cd "$TMP" && bash "$INSTALLER" --check 2>&1)"; rc=$?
if [[ $rc -eq 1 && "$out" == *not-a-repo* ]]; then
    pass "case 15: --check outside a repo -> exit 1, not-a-repo"
else
    fail "case 15: expected exit 1 not-a-repo, got rc=$rc out='$out'"
fi

# ---- summary ----

echo ""
echo "pre-push destination gate: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]] || exit 1
exit 0
