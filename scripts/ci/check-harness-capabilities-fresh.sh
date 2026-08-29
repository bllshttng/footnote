#!/usr/bin/env bash
# scripts/ci/check-harness-capabilities-fresh.sh
#
# Tripwire, not a sync step. `crates/fno-agents/build.rs` PRODUCES
# cli/src/fno/agents/harness_capabilities.toml from the canonical
# crates/fno-agents/src/harness_capabilities.toml on every build, so the only
# way the two diverge now is a hand edit of the copy. This gate catches that.
# The third copy, crates/fno/src/harness_capabilities.toml, is what the fno
# crate embeds with include_str!; it lives inside the crate so the packaged
# package builds standalone, and it is hand-synced, so it is gated too.
#
# Modes:
#   (default)    compare the COMMITTED bytes of every copy
#   --worktree   compare the working-tree bytes
#   --write      copy canonical over every copy and exit 0
#
# Comparing HEAD by default closes an ordering hazard the producer creates: a
# CI job that builds Rust before running this gate would resync the WORKTREE
# copies and launder a hand edit that is still committed. HEAD is
# order-independent.
#
# Exit codes: 0 fresh, 1 diverged, 2 misuse (a file is missing).
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/gate-fresh-common.sh"
gate_parse_mode "$@"
gate_resolve_repo_root
MODE="$GATE_MODE"

CANONICAL_REL="crates/fno-agents/src/harness_capabilities.toml"
COPY_RELS=(
  "cli/src/fno/agents/harness_capabilities.toml"
  "crates/fno/src/harness_capabilities.toml"
)

CANONICAL="$REPO_ROOT/$CANONICAL_REL"

if [[ ! -f "$CANONICAL" ]]; then
  echo "ERROR: canonical harness_capabilities.toml missing at $CANONICAL_REL" >&2
  exit 2
fi

for rel in "${COPY_RELS[@]}"; do
  if [[ ! -f "$REPO_ROOT/$rel" ]]; then
    echo "ERROR: copy harness_capabilities.toml missing at $rel" >&2
    exit 2
  fi
done

if [[ "$MODE" == "write" ]]; then
  for rel in "${COPY_RELS[@]}"; do
    cp "$CANONICAL" "$REPO_ROOT/$rel"
  done
  echo "harness capabilities copies written from $CANONICAL_REL"
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
    echo "ERROR: harness_capabilities.toml copies have diverged:" >&2
    echo "  canonical: $CANONICAL_REL" >&2
    echo "  copy:      $rel" >&2
    echo "  compared:  $SCOPE bytes" >&2
    failed=1
  fi
  i=$((i + 1))
done

if [[ "$failed" != "0" ]]; then
  echo "" >&2
  echo "The copies are kept in step with the canonical file. To resync, run:" >&2
  echo "  cargo build -p fno-agents" >&2
  echo "or, with no cargo toolchain:" >&2
  echo "  bash scripts/ci/check-harness-capabilities-fresh.sh --write" >&2
  echo "  cp $CANONICAL_REL ${COPY_RELS[0]}" >&2
  echo "  cp $CANONICAL_REL ${COPY_RELS[1]}" >&2
  exit 1
fi

echo "harness capabilities table fresh ($SCOPE)"
