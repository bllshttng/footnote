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

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
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
import sys

# The smoke runner's own bookkeeping, at the checkout root only. See the header.
RUNNER_OWNED = {
    "last-test.log",
    "preflight-last-failures.txt",
    "changed-last-receipt.json",
}

roots, snapshot = sys.argv[1:3], os.path.realpath(sys.argv[3])
checkout_root = os.path.realpath(roots[1])
rows, seen, excluded = [], set(), 0
for root in roots:
    if not os.path.isdir(root):
        continue
    for dirpath, _dirnames, filenames in os.walk(root):
        for name in filenames:
            path = os.path.join(dirpath, name)
            real = os.path.realpath(path)
            # The snapshot is not part of the population it measures.
            if real == snapshot or real in seen:
                continue
            if name in RUNNER_OWNED and os.path.realpath(dirpath) == checkout_root:
                excluded += 1
                continue
            seen.add(real)
            digest = hashlib.sha256()
            try:
                with open(path, "rb") as fh:
                    for chunk in iter(lambda: fh.read(65536), b""):
                        digest.update(chunk)
            except OSError:
                # An unreadable file is a real state, not a skip: record it so
                # a file that becomes unreadable during the run still reads as
                # changed rather than vanishing quietly.
                rows.append(("UNREADABLE", path))
                continue
            rows.append((digest.hexdigest(), path))
print(f"#excluded {excluded}")
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
  local home_fno="$HOME/.fno"
  if graph_has_entries "$home_fno/graph.json"; then
    echo "state-canary: skipped, live operator root at ~/.fno"
    printf 'SKIPPED\n' >"$SNAPSHOT"
    return 0
  fi
  if ! mkdir -p "$home_fno" "$ROOT/.fno"; then
    report "could not create the watched roots"
    return 2
  fi
  # Only when the file is ABSENT, which is the CI runner. An existing
  # graph.json is the operator's file even with an empty entries list. It
  # carries their real schema_version, and plant does not restore what it
  # overwrites. The guard above only skips a graph that HAS entries. Write
  # nothing and the walk still watches the file for a change.
  # entries is empty on purpose: a re-plant must not read its own marker as a
  # live operator root.
  if [[ ! -e "$home_fno/graph.json" ]]; then
    printf '%s\n' '{"schema_version": 1, "entries": [], "canary": "fno-state-canary"}' \
      >"$home_fno/graph.json"
  fi
  printf 'fno-state-canary\n' >"$home_fno/.canary"
  printf 'fno-state-canary\n' >"$ROOT/.fno/.canary"
  local snap_rc n
  {
    printf 'PLANTED\n'
    snapshot_now
  } >"$SNAPSHOT"
  snap_rc=$?
  n=$(($(wc -l <"$SNAPSHOT") - 2))
  # plant just wrote a .canary under each root, so under two rows means the
  # walk died. Drop the snapshot rather than keep a short one. verify refuses
  # on a missing snapshot. A short one would report every real file as ADDED,
  # blaming the test run for the instrument's own failure.
  if ((snap_rc != 0 || n < 2)); then
    report "plant could not snapshot the watched roots (exit $snap_rc, $n row(s))"
    rm -f "$SNAPSHOT"
    return 2
  fi
  echo "state-canary: planted, $n file(s) snapshotted"
}

cmd_verify() {
  if [[ ! -f "$SNAPSHOT" ]]; then
    report "no snapshot at $SNAPSHOT - plant was never run, refusing"
    return 2
  fi
  if [[ "$(head -1 "$SNAPSHOT")" == "SKIPPED" ]]; then
    echo "state-canary: skipped, live operator root at ~/.fno"
    return 0
  fi

  local after
  if ! after="$(mktemp)"; then
    report "could not create a temp file for the current snapshot"
    return 2
  fi
  snapshot_now >"$after"

  require_python
  local rc
  # Three refusals named separately: a bare "differs" cannot tell a truncation
  # from a new file, and the 2026-09-06 specimen was a truncation.
  python3 - "$SNAPSHOT" "$after" <<'PY'
import sys


def load(path):
    rows, excluded = {}, 0
    with open(path, encoding="utf-8") as fh:
        for line in fh.read().splitlines():
            if not line.strip() or line == "PLANTED":
                continue
            if line.startswith("#excluded "):
                excluded = int(line.split()[1])
                continue
            digest, _, name = line.partition("  ")
            rows[name] = digest
    return rows, excluded


before, _ = load(sys.argv[1])
after, excluded = load(sys.argv[2])

violations = 0
for name in sorted(set(after) - set(before)):
    print(f"state-canary: ADDED {name}", file=sys.stderr)
    violations += 1
for name in sorted(set(before) - set(after)):
    print(f"state-canary: REMOVED {name}", file=sys.stderr)
    violations += 1
for name in sorted(set(before) & set(after)):
    if before[name] != after[name]:
        print(f"state-canary: CHANGED {name}", file=sys.stderr)
        violations += 1

if violations:
    print(
        f"state-canary: {violations} file(s) under the operator state root "
        "were touched by the run",
        file=sys.stderr,
    )
    raise SystemExit(1)
print(
    f"state-canary: ok, {len(after)} file(s) checked, "
    f"{excluded} runner-owned path(s) excluded, none added, removed or changed"
)
PY
  rc=$?
  rm -f "$after"
  return $rc
}

# The positive control, drawn from the measured population: a raw write to the
# real graph path is the exact write the 2026-09-06 specimen made.
cmd_self_test() {
  local tmp_home inner_out inner_rc plant_rc
  tmp_home="$(mktemp -d)" || return 2
  inner_out="$(mktemp)" || return 2

  HOME="$tmp_home" \
  FNO_STATE_CANARY_SNAPSHOT="$tmp_home/canary.snapshot" \
    bash "${BASH_SOURCE[0]}" plant >"$inner_out" 2>&1
  plant_rc=$?
  if ((plant_rc != 0)); then
    report "self-test FAILED: inner plant exited $plant_rc"
    cat "$inner_out" >&2
    rm -rf "$tmp_home" "$inner_out"
    return 1
  fi
  if grep -q "skipped, live operator root" "$inner_out"; then
    report "self-test FAILED: inner plant SKIPPED on a fresh HOME"
    report "self-test: the dev-box guard is misfiring, so no lane is measured"
    rm -rf "$tmp_home" "$inner_out"
    return 1
  fi

  # One byte. Not a rewrite: the control must be the smallest change the
  # instrument claims to catch.
  printf ' ' >>"$tmp_home/.fno/graph.json"

  HOME="$tmp_home" \
  FNO_STATE_CANARY_SNAPSHOT="$tmp_home/canary.snapshot" \
    bash "${BASH_SOURCE[0]}" verify >"$inner_out" 2>&1
  inner_rc=$?

  if ((inner_rc == 0)); then
    report "self-test FAILED: the inner verify PASSED after one byte was written"
    report "self-test: the canary cannot go red, so its green proves nothing"
    rm -rf "$tmp_home" "$inner_out"
    return 1
  fi
  if ! grep -q "CHANGED .*graph\.json" "$inner_out"; then
    report "self-test FAILED: the inner verify refused but never named graph.json"
    cat "$inner_out" >&2
    rm -rf "$tmp_home" "$inner_out"
    return 1
  fi

  local named
  named="$(grep -o 'CHANGED .*graph\.json' "$inner_out" | head -1 | sed 's/^CHANGED //')"
  echo "state-canary: self-test ok, inner verify exited $inner_rc naming $named"
  rm -rf "$tmp_home" "$inner_out"
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
