#!/usr/bin/env bash
# Commit-time salvage ref (plan section 4).
#
# A provider-killed worker never reaches a stop gate, so commit time is the
# only moment guaranteed to occur before the kill. This drives a real
# scratch repo + bare remote + linked worktree (not mocked git output) and
# asserts the three acceptance criteria named in the plan: the ref lands
# locally on commit, a commit in a DETACHED worktree survives that
# worktree's removal, and a dead remote never blocks the commit or the
# local write.
#
# Scope note: this file covers the salvage-ref hook only. The classifier
# (classify/resolve_node_id) and the recovery acts (act_on_stranded,
# apply_sweep - push, file, emit, stop-at-first-failure) are covered by
# cli/tests/unit/test_worktree_stranded.py, which drives them directly with
# a monkeypatched subprocess.run rather than a live `fno` CLI subprocess:
# `fno` on PATH resolves the deployed binary, not this worktree's source
# (verified gotcha), so a real end-to-end `fno agents workspace worktree stranded --apply`
# shell test would silently exercise the WRONG code.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SALVAGE_HOOK="$REPO_ROOT/hooks/worktree-salvage-ref.sh"
SCRATCH="$(mktemp -d)"
SCRATCH="$(cd "$SCRATCH" && pwd -P)"
trap 'rm -rf "$SCRATCH"' EXIT

fail() { echo "FAIL: $1" >&2; exit 1; }

SETUP_WORKTREE_SH="$REPO_ROOT/scripts/setup/setup-worktree.sh"

extract_install_snippet() {
  # Extracts the REAL salvage-ref install block from setup-worktree.sh
  # instead of hand-copying it a third time (a code-review finding: this
  # file and the create/prepend logic it mirrors had already drifted once).
  # Bounded by two markers that each occur exactly once in the file; the
  # end marker line is excluded from the extracted range.
  sed -n '/^_salvage_marker="worktree-salvage-ref\.sh"$/,/^if (( events_journal_shared == 0 )); then$/p' \
    "$SETUP_WORKTREE_SH" | sed '$d'
}

run_setup_worktree_install() {
  # Runs the extracted snippet for real against a synthetic $WORKTREE, so
  # this test proves the ACTUAL create/prepend logic works, not a
  # hand-typed mirror of it. The snippet's own `if [[ ! -e "$_post_commit" ]]`
  # branch decides create vs. prepend, so one function covers both cases.
  local wt="$1"
  local snippet
  snippet="$(extract_install_snippet)"
  [[ -n "$snippet" ]] || fail "could not extract the salvage-ref install snippet from setup-worktree.sh - markers moved?"
  WORKTREE="$wt" bash -c "$snippet"
}

link_real_hook() {
  local wt="$1"
  mkdir -p "$wt/hooks"
  cp "$SALVAGE_HOOK" "$wt/hooks/worktree-salvage-ref.sh"
  chmod +x "$wt/hooks/worktree-salvage-ref.sh"
}

remote="$SCRATCH/remote.git"
canonical="$SCRATCH/canonical"
git init --bare -q "$remote"
git clone -q "$remote" "$canonical" >/dev/null
git -C "$canonical" config user.email t@t.co
git -C "$canonical" config user.name t
git -C "$canonical" checkout -q -b trunk
echo x > "$canonical/f.txt"
git -C "$canonical" add f.txt
git -C "$canonical" commit -q -m init
git -C "$canonical" push -q origin trunk

# --- AC1: a commit lands a local salvage ref -----------------------------

attached="$SCRATCH/attached-wt"
git -C "$canonical" worktree add -q -b attached-branch "$attached" >/dev/null
link_real_hook "$attached"
run_setup_worktree_install "$attached"
git -C "$attached" config user.email t@t.co
git -C "$attached" config user.name t
echo y > "$attached/g.txt"
git -C "$attached" add g.txt
git -C "$attached" commit -q -m "attached commit"
sha_attached="$(git -C "$attached" rev-parse HEAD)"
sleep 1

wt_name="$(basename "$attached")"
ref_out="$(git -C "$canonical" for-each-ref "refs/fno/salvage/$wt_name")"
[[ -n "$ref_out" ]] || fail "no local salvage ref written for $wt_name"
echo "$ref_out" | grep -q "$sha_attached" || fail "salvage ref does not point at the commit"

remote_ref_out="$(git --git-dir="$remote" for-each-ref "refs/fno/salvage/$wt_name")"
[[ -n "$remote_ref_out" ]] || fail "salvage ref did not mirror to origin"

echo "PASS: commit lands a local salvage ref, mirrored to origin"

# --- AC2: a DETACHED worktree's commit survives its own removal ---------

detached="$SCRATCH/detached-wt"
git -C "$canonical" worktree add -q --detach "$detached" >/dev/null
link_real_hook "$detached"
# Dispatcher already installed once on the shared common dir; every
# worktree's own hooks/worktree-salvage-ref.sh resolves independently.
git -C "$detached" config user.email t@t.co
git -C "$detached" config user.name t
echo z > "$detached/h.txt"
git -C "$detached" add h.txt
git -C "$detached" commit -q -m "detached commit"
sha_detached="$(git -C "$detached" rev-parse HEAD)"
sleep 1

rm -rf "$detached"
git -C "$canonical" worktree prune

git -C "$canonical" cat-file -e "$sha_detached" \
  || fail "detached worktree's commit did not survive worktree removal"

echo "PASS: a detached worktree's commit survives its own removal"

# --- AC3: a dead remote never blocks the commit or the local write ------

offline="$SCRATCH/offline-wt"
git -C "$canonical" worktree add -q -b offline-branch "$offline" >/dev/null
link_real_hook "$offline"
git -C "$offline" config user.email t@t.co
git -C "$offline" config user.name t
# Point origin at a path that does not exist: every push from this worktree
# fails, exactly the mid-run network-loss case the hook exists for.
git -C "$offline" remote set-url origin "$SCRATCH/does-not-exist.git"

echo w > "$offline/i.txt"
git -C "$offline" add i.txt
set +e
git -C "$offline" commit -q -m "offline commit"
commit_rc=$?
set -e
(( commit_rc == 0 )) || fail "commit failed with a dead remote (hook must exit 0 regardless)"

sha_offline="$(git -C "$offline" rev-parse HEAD)"
offline_ref="$(git -C "$canonical" for-each-ref "refs/fno/salvage/$(basename "$offline")")"
[[ -n "$offline_ref" ]] || fail "local salvage ref missing when the remote was unreachable"
echo "$offline_ref" | grep -q "$sha_offline" || fail "local salvage ref did not point at the commit with a dead remote"

echo "PASS: a dead remote never blocks the commit or the local salvage ref"

# --- AC4: prepending onto a pre-existing hook that ends in `exit 0` ------
# A code-review finding: appending after a hook's own bare `exit` makes the
# appended block dead code - the interpreter never reaches it, regardless of
# its position in the file. This proves both directions: the salvage ref
# still lands, AND the pre-existing hook's own tail logic still runs.

preexisting="$SCRATCH/preexisting-wt"
git -C "$canonical" worktree add -q -b preexisting-branch "$preexisting" >/dev/null
link_real_hook "$preexisting"

common_dir="$(git -C "$preexisting" rev-parse --git-common-dir)"
[[ "$common_dir" = /* ]] || common_dir="$preexisting/$common_dir"
marker_file="$SCRATCH/other-hook-ran"
cat > "$common_dir/hooks/post-commit" <<HOOK
#!/usr/bin/env bash
touch "$marker_file"
exit 0
HOOK
chmod +x "$common_dir/hooks/post-commit"
run_setup_worktree_install "$preexisting"

git -C "$preexisting" config user.email t@t.co
git -C "$preexisting" config user.name t
echo v > "$preexisting/j.txt"
git -C "$preexisting" add j.txt
git -C "$preexisting" commit -q -m "preexisting-hook commit"
sha_preexisting="$(git -C "$preexisting" rev-parse HEAD)"
sleep 1

[[ -f "$marker_file" ]] || fail "pre-existing hook's own logic did not run after prepend"

preexisting_ref="$(git -C "$canonical" for-each-ref "refs/fno/salvage/$(basename "$preexisting")")"
[[ -n "$preexisting_ref" ]] || fail "salvage ref missing when prepended onto a hook ending in exit 0"
echo "$preexisting_ref" | grep -q "$sha_preexisting" || fail "salvage ref did not point at the commit"

echo "PASS: prepending onto a pre-existing exit-terminated hook still runs both"
