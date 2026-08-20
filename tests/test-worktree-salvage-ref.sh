#!/usr/bin/env bash
# Commit-time salvage ref (x-f4e9, plan section 4).
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
# (verified gotcha), so a real end-to-end `fno worktree stranded --apply`
# shell test would silently exercise the WRONG code.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SALVAGE_HOOK="$REPO_ROOT/hooks/worktree-salvage-ref.sh"
SCRATCH="$(mktemp -d)"
SCRATCH="$(cd "$SCRATCH" && pwd -P)"
trap 'rm -rf "$SCRATCH"' EXIT

fail() { echo "FAIL: $1" >&2; exit 1; }

install_dispatcher() {
  # Mirrors scripts/setup/setup-worktree.sh's install: a shared post-commit
  # dispatcher in the git-common-dir that resolves the COMMITTING worktree's
  # own checked-out copy of the real hook at execution time.
  local wt="$1"
  local common_dir
  common_dir="$(git -C "$wt" rev-parse --git-common-dir)"
  [[ "$common_dir" = /* ]] || common_dir="$wt/$common_dir"
  mkdir -p "$common_dir/hooks"
  cat > "$common_dir/hooks/post-commit" <<'HOOK'
#!/usr/bin/env bash
toplevel="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0
script="$toplevel/hooks/worktree-salvage-ref.sh"
[[ -x "$script" ]] && exec "$script"
exit 0
HOOK
  chmod +x "$common_dir/hooks/post-commit"
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
install_dispatcher "$attached"
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
