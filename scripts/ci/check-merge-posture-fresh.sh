#!/usr/bin/env bash
# scripts/ci/check-merge-posture-fresh.sh
#
# Tripwire for the merge-refusal carrier's ONE vocabulary table (x-8151).
# The canonical file humans edit is
# `crates/fno-agents/src/merge_posture.toml`; every build of `fno-agents`
# produces the byte copy `cli/src/fno/agents/merge_posture.toml` (loaded as
# Python package data) via `build.rs`. The copy stays tracked because the
# Python side must load without this repo's workspace. Same shape as
# check-harness-capabilities-fresh.sh, which is the pattern this gate follows.
#
# Modes:
#   (default)    compare the COMMITTED bytes of every copy
#   --worktree   compare the working-tree bytes
#   --write      copy canonical over every copy and exit 0
#
# Exit codes: 0 fresh, 1 diverged, 2 misuse (a file is missing).
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/gate-fresh-common.sh"
gate_parse_mode "$@"
gate_resolve_repo_root
MODE="$GATE_MODE"

CANONICAL_REL="crates/fno-agents/src/merge_posture.toml"
COPY_RELS=(
  "cli/src/fno/agents/merge_posture.toml"
)

CANONICAL="$REPO_ROOT/$CANONICAL_REL"

if [[ ! -f "$CANONICAL" ]]; then
  echo "ERROR: canonical merge_posture.toml missing at $CANONICAL_REL" >&2
  exit 2
fi

for rel in "${COPY_RELS[@]}"; do
  if [[ ! -f "$REPO_ROOT/$rel" ]]; then
    echo "ERROR: generated merge_posture.toml copy missing at $rel" >&2
    exit 2
  fi
done

if [[ "$MODE" == "write" ]]; then
  for rel in "${COPY_RELS[@]}"; do
    cp "$CANONICAL" "$REPO_ROOT/$rel"
  done
  echo "merge posture generated copy written from $CANONICAL_REL"
  exit 0
fi

TMPDIR_GATE="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_GATE"' EXIT

LEFT="$CANONICAL"
SCOPE="worktree"
COPY_BLOBS=()
for _ in "${COPY_RELS[@]}"; do COPY_BLOBS+=(""); done
if [[ "$MODE" == "committed" ]]; then
  # Degrade per file when a blob is absent at HEAD (an unborn branch, or a
  # file not yet tracked). A repo with nothing committed has no committed
  # drift to find, and silently passing would be worse than saying so.
  if gate_committed_blob "$CANONICAL_REL" "$TMPDIR_GATE/canonical"; then
    LEFT="$TMPDIR_GATE/canonical"
    SCOPE="committed"
    i=0
    for rel in "${COPY_RELS[@]}"; do
      if gate_committed_blob "$rel" "$TMPDIR_GATE/copy-$i"; then
        COPY_BLOBS[$i]="$TMPDIR_GATE/copy-$i"
      else
        echo "NOTE: no committed blob for $rel; comparing its worktree instead" >&2
      fi
      i=$((i + 1))
    done
  else
    echo "NOTE: no committed blobs for the canonical path; comparing the worktree instead" >&2
  fi
fi

failed=0
i=0
for rel in "${COPY_RELS[@]}"; do
  RIGHT="${COPY_BLOBS[$i]}"
  if [[ -z "$RIGHT" ]]; then
    RIGHT="$REPO_ROOT/$rel"
  fi
  if ! cmp -s "$LEFT" "$RIGHT"; then
    echo "ERROR: merge_posture.toml generated copy has diverged from the canonical:" >&2
    echo "  canonical: $CANONICAL_REL" >&2
    echo "  generated: $rel" >&2
    echo "  compared:  $SCOPE bytes" >&2
    failed=1
  fi
  i=$((i + 1))
done

if [[ "$failed" != "0" ]]; then
  echo "" >&2
  echo "The generated copy is produced from the canonical file. To resync, run:" >&2
  echo "  cargo build -p fno-agents" >&2
  echo "or, with no cargo toolchain:" >&2
  echo "  bash scripts/ci/check-merge-posture-fresh.sh --write" >&2
  exit 1
fi

echo "merge posture carrier table fresh ($SCOPE)"
