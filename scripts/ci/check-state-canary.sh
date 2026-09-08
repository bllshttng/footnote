#!/usr/bin/env bash
# check-state-canary.sh - prove a test run wrote nothing into the operator's
# real state root, and prove the proof can go red.
#
# Replaces `_real_graph_leak_tripwire` (cli/tests/conftest.py), which watched
# ONE file, `~/.fno/graph.json`, for ADDED node ids, and only when CI was set.
# That shape cannot see a truncation. On 2026-09-06 a run cut a 2297-node graph
# to a single 64-byte entry; a truncation adds no node id, so the tripwire
# passed. It also ran on CI only, where HOME is empty and the specimen it was
# written for cannot occur.
#
# A content hash over every file under both watched roots catches an added, a
# removed and a changed file alike.
#
# Verbs:
#   plant      write a marker graph and the two .canary files, then snapshot a
#              hash of every file under $HOME/.fno and <checkout>/.fno.
#   verify     recompute and refuse on any added, removed or changed file,
#              naming each path. On success print the file count checked, so a
#              green is a positive marker rather than an absence.
#   self-test  the positive control. Plant into a fresh HOME, write ONE BYTE
#              into the planted graph.json, and require the inner verify to
#              exit non-zero naming that file. An inner verify that PASSES
#              makes self-test exit 1.
#
# The dev-box guard is not optional. When $HOME/.fno/graph.json already holds
# entries, that is a live operator root: print the skip receipt and do nothing.
# No dev box ever has its graph planted over. On CI the runner HOME is empty,
# so the canary runs on every shard.
#
# The snapshot lives OUTSIDE both watched roots. A snapshot written inside one
# would be a file plant creates and verify then reads as added, so the
# instrument would flag itself.
#
# Three paths directly under <checkout>/.fno are excluded BY NAME, because the
# smoke runner writes them while the canary brackets it. They are the
# instrument's own exhaust, not a test reaching operator state:
#
#   last-test.log               cli/src/fno/test_cmd.py:255
#   preflight-last-failures.txt cli/src/fno/test_cmd.py SMOKE_FAILURE_RECORD_DEFAULT
#   changed-last-receipt.json   cli/src/fno/test_cmd.py CHANGED_RECEIPT_DEFAULT
#
# The list is by exact basename, at the checkout root only, and verify prints
# how many it excluded. A green that rests on a growing ignore list is the
# failure this whole script refuses, so the count is on screen every run. The
# same names under $HOME/.fno are NOT excluded; there they would be a finding.
#
# Run: bash scripts/ci/check-state-canary.sh {plant|verify|self-test}

set -uo pipefail

# The checkout whose .fno is the second watched root. Overridable so self-test
# can sandbox it: without that its inner plant wrote a .canary into the REAL
# checkout and never removed it, because only HOME was redirected.
ROOT="${FNO_STATE_CANARY_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
SNAPSHOT="${FNO_STATE_CANARY_SNAPSHOT:-${TMPDIR:-/tmp}/fno-state-canary.$(id -u).snapshot}"

report() { echo "state-canary: $1" >&2; }

# python3 is required rather than grepped around: a byte heuristic on JSON is
# how a guard starts lying, and a canary that cannot read fails closed.
require_python() {
  command -v python3 >/dev/null 2>&1 && return 0
  report "no python3 on PATH - refusing rather than degrading to a green"
  exit 2
}

# Walk both roots and print "<sha256>  <path>", sorted. Symlinks are NOT
# followed. setup-worktree.sh retired the fno state links, so a worktree's .fno
# holds plain files and following finds nothing new. It also costs. On one dev
# box following raised the population from 74,515 files to 180,362. The links
# under ~/.fno/worktrees reach the vault and the canonical checkout, and every
# file found is hashed, twice per run.
snapshot_now() {
  require_python
  python3 - "$HOME/.fno" "$ROOT/.fno" "$SNAPSHOT" <<'PY'
import hashlib
import os
import stat
import sys

# The smoke runner's own bookkeeping, at the checkout root only. See the header.
RUNNER_OWNED = {
    "last-test.log",
    "preflight-last-failures.txt",
    "changed-last-receipt.json",
}

# Pruned at the state root's top level. A worktree is a git checkout with its
# own history, not operator state, and it dominates the population: measured on
# one dev box, 25,027 of 68,279 files and 2.89 of 4.17 GB. Every prune is named
# in the receipt, because a green resting on a silent ignore list is the thing
# this script refuses.
PRUNED_TOPLEVEL = {"worktrees"}

roots, snapshot = sys.argv[1:3], os.path.realpath(sys.argv[3])
checkout_root = os.path.realpath(roots[1])
rows, seen, excluded, pruned = [], set(), 0, []
for root in roots:
    if not os.path.isdir(root):
        continue
    root_real = os.path.realpath(root)
    for dirpath, dirnames, filenames in os.walk(root):
        if os.path.realpath(dirpath) == root_real:
            for name in sorted(dirnames):
                if name in PRUNED_TOPLEVEL:
                    pruned.append(os.path.join(dirpath, name))
            dirnames[:] = [d for d in dirnames if d not in PRUNED_TOPLEVEL]
        # realpath the DIRECTORY, never the entry: a root that is itself a
        # symlink still dedups, while an entry symlink keeps its own identity.
        dir_real = os.path.realpath(dirpath)
        for name in filenames:
            path = os.path.join(dirpath, name)
            key = os.path.join(dir_real, name)
            # The snapshot is not part of the population it measures.
            if key == snapshot or key in seen:
                continue
            if name in RUNNER_OWNED and dir_real == checkout_root:
                excluded += 1
                continue
            seen.add(key)
            try:
                st = os.lstat(path)
            except OSError:
                rows.append(("UNREADABLE", path))
                continue
            # Only a REGULAR file carries content to hash. A socket, fifo,
            # device or symlink is furniture: it has no bytes to compare, and
            # open() on one raises, so hashing it would report ordinary state
            # as a violation. Measured on one dev box, a live operator root
            # holds 10 sockets and 9 dangling symlinks. Record the kind (and a
            # symlink's target) instead: an added, removed or retargeted one
            # still shows up as ADDED, REMOVED or CHANGED, with no read.
            if stat.S_ISLNK(st.st_mode):
                try:
                    rows.append((f"SYMLINK:{os.readlink(path)}", path))
                except OSError:
                    rows.append(("SYMLINK:?", path))
                continue
            if not stat.S_ISREG(st.st_mode):
                rows.append((f"NOTAFILE:{stat.S_IFMT(st.st_mode):#o}", path))
                continue
            digest = hashlib.sha256()
            try:
                with open(path, "rb") as fh:
                    for chunk in iter(lambda: fh.read(65536), b""):
                        digest.update(chunk)
            except OSError:
                # A REGULAR file we cannot read is a real instrument failure,
                # not furniture. Record it so a file that becomes unreadable
                # during the run still reads as changed rather than vanishing.
                # verify refuses on the marker rather than comparing it: two
                # unreadable reads of the SAME file compare equal, and a test
                # can rewrite a write-only file between them with nothing to
                # see.
                rows.append(("UNREADABLE", path))
                continue
            # Size rides along with the digest so verify can tell a TRUNCATION
            # from ordinary churn. The 2026-09-06 specimen cut a 2297-node
            # graph to 64 bytes, and on a live root that distinction is what
            # separates a real incident from another session's normal write.
            rows.append((f"{digest.hexdigest()}:{st.st_size}", path))
print(f"#excluded {excluded}")
for p in pruned:
    print(f"#pruned {p}")
print("\n".join(f"{h}  {p}" for h, p in sorted(rows, key=lambda r: r[1])))
PY
}

# A live operator root is any graph.json carrying at least one entry.
graph_has_entries() {
  local graph="$1"
  [[ -f "$graph" ]] || return 1
  require_python
  python3 - "$graph" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as fh:
        data = json.load(fh)
except (OSError, ValueError):
    # Unreadable or corrupt is NOT "empty". Treat it as live so a dev box with
    # a damaged graph is never planted over on top of the damage.
    raise SystemExit(0)
entries = data.get("entries", []) if isinstance(data, dict) else data
raise SystemExit(0 if entries else 1)
PY
}

cmd_plant() {
  local home_fno="$HOME/.fno" live="" header="PLANTED" floor=2
  # A live operator root is WATCHED, never written and never skipped. Only the
  # marker write needs an empty root; the snapshot and the comparison are
  # read-only and work over whatever is already there. Skipping instead was the
  # defect: the header's motivating specimen is a run that truncated a
  # 2297-node graph, and a 2297-node graph IS a live root, so the instrument
  # stood down on precisely the machine class it was written for.
  if graph_has_entries "$home_fno/graph.json"; then
    live=1
    header="WATCHING"
    floor=1
  fi
  if ! mkdir -p "$home_fno" "$ROOT/.fno"; then
    report "could not create the watched roots"
    return 2
  fi
  if [[ -z "$live" ]]; then
    # Only when the file is ABSENT, which is the CI runner. An existing
    # graph.json is the operator's file even with an empty entries list, and
    # plant does not restore what it overwrites. The walk watches it either
    # way. `entries` is empty on purpose: a re-plant must not read its own
    # marker as a live operator root.
    if [[ ! -e "$home_fno/graph.json" ]]; then
      printf '%s\n' '{"entries": []}' >"$home_fno/graph.json"
    fi
    printf 'fno-state-canary\n' >"$home_fno/.canary"
    printf 'fno-state-canary\n' >"$ROOT/.fno/.canary"
  fi
  local snap_rc n
  {
    printf '%s\n' "$header"
    snapshot_now
  } >"$SNAPSHOT"
  snap_rc=$?
  n=$(($(wc -l <"$SNAPSHOT") - 2))
  # A planting run just wrote a .canary under each root, so under two rows
  # means the walk died. A watching run writes nothing, so one row is the
  # floor there. Drop the snapshot rather than keep a short one. verify
  # refuses on a missing snapshot. A short one would report every real file as
  # ADDED, blaming the test run for the instrument's own failure.
  if ((snap_rc != 0 || n < floor)); then
    report "plant could not snapshot the watched roots (exit $snap_rc, $n row(s))"
    rm -f "$SNAPSHOT"
    return 2
  fi
  if [[ -n "$live" ]]; then
    echo "state-canary: watching a live operator root read-only, $n path(s) snapshotted"
  else
    echo "state-canary: planted, $n path(s) snapshotted"
  fi
}

cmd_verify() {
  if [[ ! -f "$SNAPSHOT" ]]; then
    report "no snapshot at $SNAPSHOT - plant was never run, refusing"
    return 2
  fi
  # A stale SKIPPED snapshot from the previous release of this script. The
  # skip is gone; refuse rather than reading it as a clean run.
  if [[ "$(head -1 "$SNAPSHOT")" == "SKIPPED" ]]; then
    report "snapshot says SKIPPED, which this version never writes - re-run plant"
    return 2
  fi

  local after
  if ! after="$(mktemp)"; then
    report "could not create a temp file for the current snapshot"
    return 2
  fi
  local snap_rc after_n
  snapshot_now >"$after"
  snap_rc=$?
  after_n=$(($(wc -l <"$after") - 1))
  # The mirror of plant's floor. A python3 that dies mid-walk leaves the after
  # snapshot empty, and every baselined path then prints as REMOVED. That
  # reports the instrument's own failure as a leak, which is the exact
  # misattribution plant already refuses.
  if ((snap_rc != 0 || after_n < 1)); then
    report "verify could not snapshot the watched roots (exit $snap_rc, $after_n row(s))"
    rm -f "$after"
    return 2
  fi

  require_python
  local rc
  # Three refusals named separately: a bare "differs" cannot tell a truncation
  # from a new file, and the 2026-09-06 specimen was a truncation.
  python3 - "$SNAPSHOT" "$after" <<'PY'
import sys


def header_of(path):
    with open(path, encoding="utf-8") as fh:
        return fh.readline().strip()


def load(path):
    rows, excluded, pruned = {}, 0, []
    with open(path, encoding="utf-8") as fh:
        for line in fh.read().splitlines():
            if not line.strip() or line in ("PLANTED", "WATCHING"):
                continue
            if line.startswith("#excluded "):
                excluded = int(line.split()[1])
                continue
            if line.startswith("#pruned "):
                pruned.append(line[len("#pruned "):])
                continue
            digest, _, name = line.partition("  ")
            rows[name] = digest
    return rows, excluded, pruned


watching = header_of(sys.argv[1]) == "WATCHING"
before, _, _ = load(sys.argv[1])
after, excluded, pruned = load(sys.argv[2])

violations = 0
# An unreadable file carries no content evidence on either side, so it can
# never support "unchanged". Two UNREADABLE markers for one path compare EQUAL,
# so without this a test rewriting a write-only file reads as untouched. Report
# it once and keep it out of the content comparison below.
unreadable = {n for n, d in before.items() if d == "UNREADABLE"}
unreadable |= {n for n, d in after.items() if d == "UNREADABLE"}
for name in sorted(unreadable):
    print(f"state-canary: UNREADABLE {name}", file=sys.stderr)
    violations += 1
for name in sorted(set(after) - set(before)):
    print(f"state-canary: ADDED {name}", file=sys.stderr)
    violations += 1
for name in sorted(set(before) - set(after)):
    print(f"state-canary: REMOVED {name}", file=sys.stderr)
    violations += 1
def size_of(marker):
    _, _, tail = marker.rpartition(":")
    return int(tail) if tail.isdigit() else -1


truncations = 0
for name in sorted((set(before) & set(after)) - unreadable):
    if before[name] == after[name]:
        continue
    was, now = size_of(before[name]), size_of(after[name])
    # A collapse, not a change. This is the 2026-09-06 specimen's shape, and it
    # gates even on a live root, where ordinary churn does not.
    if was > 0 and 0 <= now <= was // 10:
        print(f"state-canary: TRUNCATED {name} ({was} -> {now} bytes)", file=sys.stderr)
        truncations += 1
    else:
        print(f"state-canary: CHANGED {name}", file=sys.stderr)
    violations += 1

if violations:
    print(
        f"state-canary: {violations} path(s) under the operator state root "
        f"changed during the run ({truncations} truncation(s))",
        file=sys.stderr,
    )
    # On a live operator root the comparison cannot attribute a change to THIS
    # suite: measured on one box, 87 live sessions minted four 13.1 MB graph
    # backups inside a single 20-second window. Gating there would be
    # permanently red, which is how a guard gets disabled. So report every
    # change and gate only on a truncation, whose shape no ordinary write has.
    # A planted root is exclusively ours, so everything gates.
    if watching and not truncations:
        print(
            "state-canary: advisory only. This root is a live operator root, "
            "shared with other sessions, so a change here is not attributable "
            "to this run. Read the list above; nothing is failed on it.",
            file=sys.stderr,
        )
        raise SystemExit(0)
    raise SystemExit(1)
print(
    f"state-canary: ok, {len(after)} path(s) checked, "
    f"{excluded} runner-owned path(s) excluded, "
    f"{len(pruned)} subtree(s) pruned, none added, removed or changed"
)
for p in pruned:
    print(f"state-canary: pruned subtree {p}")
PY
  rc=$?
  rm -f "$after"
  return $rc
}

# One lane of the positive control. `mode` is the shape of the HOME it builds:
# `fresh` is the CI runner, `live` is a developer box whose graph already has
# entries. Both must end red on a one-byte write to graph.json.
#
# Running BOTH is the point. The old control only ever built a fresh HOME, so
# it passed identically whether the production lane measured a dev box or stood
# down on it. That is the trap AGENTS.md names: a green control aimed at the
# wrong target still reads as proof.
self_test_lane() {
  local mode="$1"
  local tmp_home tmp_root inner_out inner_rc plant_rc want
  tmp_home="$(mktemp -d)" || return 2
  tmp_root="$(mktemp -d)" || return 2
  inner_out="$(mktemp)" || return 2

  mkdir -p "$tmp_home/.fno"
  if [[ "$mode" == "live" ]]; then
    # An operator root with real work in it. plant must watch, never write.
    # Sized like a real graph on purpose: the truncation check below is a
    # PROPORTIONAL collapse, and a two-line fixture cannot collapse. The first
    # version of this control used one entry, so the truncation never tripped
    # the threshold and the control passed on a graph that had not collapsed.
    require_python
    python3 - "$tmp_home/.fno/graph.json" <<'PY'
import json
import sys

entries = [
    {"id": f"x-{i:04x}", "title": f"real work {i}", "details": "x" * 200}
    for i in range(500)
]
with open(sys.argv[1], "w", encoding="utf-8") as fh:
    json.dump({"entries": entries}, fh)
PY
    want="watching a live operator root"
  else
    want="planted"
  fi

  local -a env_pins=(
    "HOME=$tmp_home"
    "FNO_STATE_CANARY_ROOT=$tmp_root"
    "FNO_STATE_CANARY_SNAPSHOT=$tmp_home/canary.snapshot"
  )

  env "${env_pins[@]}" bash "${BASH_SOURCE[0]}" plant >"$inner_out" 2>&1
  plant_rc=$?
  if ((plant_rc != 0)); then
    report "self-test[$mode] FAILED: inner plant exited $plant_rc"
    cat "$inner_out" >&2
    rm -rf "$tmp_home" "$tmp_root" "$inner_out"
    return 1
  fi
  if ! grep -q "$want" "$inner_out"; then
    report "self-test[$mode] FAILED: inner plant did not report '$want'"
    cat "$inner_out" >&2
    rm -rf "$tmp_home" "$tmp_root" "$inner_out"
    return 1
  fi
  if [[ "$mode" == "live" ]]; then
    # The read-only promise, checked rather than asserted.
    if [[ -e "$tmp_home/.fno/.canary" ]]; then
      report "self-test[live] FAILED: plant wrote .canary into a live operator root"
      rm -rf "$tmp_home" "$tmp_root" "$inner_out"
      return 1
    fi
    if ! grep -q 'real work 499' "$tmp_home/.fno/graph.json"; then
      report "self-test[live] FAILED: plant overwrote a live operator graph"
      rm -rf "$tmp_home" "$tmp_root" "$inner_out"
      return 1
    fi
  fi

  # One byte. Not a rewrite: the control must be the smallest change the
  # instrument claims to catch.
  printf ' ' >>"$tmp_home/.fno/graph.json"

  env "${env_pins[@]}" bash "${BASH_SOURCE[0]}" verify >"$inner_out" 2>&1
  inner_rc=$?

  if ! grep -q "CHANGED .*graph\.json" "$inner_out"; then
    report "self-test[$mode] FAILED: the inner verify never named the changed graph.json"
    cat "$inner_out" >&2
    rm -rf "$tmp_home" "$tmp_root" "$inner_out"
    return 1
  fi

  if [[ "$mode" == "live" ]]; then
    # An ordinary change on a live root is REPORTED and not failed, because it
    # is not attributable to this run. Both halves are checked: it was named
    # above, and it must not gate here.
    if ((inner_rc != 0)); then
      report "self-test[live] FAILED: an ordinary change gated on a live root (rc $inner_rc)"
      report "self-test[live]: a permanently red dev box is how a guard gets disabled"
      cat "$inner_out" >&2
      rm -rf "$tmp_home" "$tmp_root" "$inner_out"
      return 1
    fi
    # The incident shape MUST gate, live root or not. This is the 2026-09-06
    # specimen: a 2297-node graph cut to a single 64-byte entry.
    printf '{"entries": []}' >"$tmp_home/.fno/graph.json"
    env "${env_pins[@]}" bash "${BASH_SOURCE[0]}" verify >"$inner_out" 2>&1
    inner_rc=$?
    if ((inner_rc == 0)); then
      report "self-test[live] FAILED: a TRUNCATED graph did not gate on a live root"
      report "self-test[live]: this is the exact incident the canary was written for"
      cat "$inner_out" >&2
      rm -rf "$tmp_home" "$tmp_root" "$inner_out"
      return 1
    fi
    if ! grep -q "TRUNCATED .*graph\.json" "$inner_out"; then
      report "self-test[live] FAILED: the refusal never named the truncation"
      cat "$inner_out" >&2
      rm -rf "$tmp_home" "$tmp_root" "$inner_out"
      return 1
    fi
    local shown
    shown="$(grep -o 'TRUNCATED .*' "$inner_out" | head -1)"
    echo "state-canary: self-test[live] ok, change advisory and $shown"
    rm -rf "$tmp_home" "$tmp_root" "$inner_out"
    return 0
  fi

  if ((inner_rc == 0)); then
    report "self-test[$mode] FAILED: the inner verify PASSED after one byte was written"
    report "self-test[$mode]: the canary cannot go red, so its green proves nothing"
    rm -rf "$tmp_home" "$tmp_root" "$inner_out"
    return 1
  fi
  local named
  named="$(grep -o 'CHANGED .*graph\.json' "$inner_out" | head -1 | sed 's/^CHANGED //')"
  echo "state-canary: self-test[$mode] ok, inner verify exited $inner_rc naming $named"
  rm -rf "$tmp_home" "$tmp_root" "$inner_out"
}

# The positive control, drawn from the measured population: a raw write to the
# real graph path is the exact write the 2026-09-06 specimen made.
cmd_self_test() {
  local mode
  for mode in fresh live; do
    self_test_lane "$mode" || return 1
  done
  echo "state-canary: self-test ok, both lanes measured and both went red"
}

case "${1:-}" in
  plant) cmd_plant ;;
  verify) cmd_verify ;;
  self-test) cmd_self_test ;;
  *)
    echo "usage: bash scripts/ci/check-state-canary.sh {plant|verify|self-test}" >&2
    exit 2
    ;;
esac
