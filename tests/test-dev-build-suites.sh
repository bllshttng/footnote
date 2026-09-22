#!/usr/bin/env bash
# Runs every test that takes the native_backlog_door fixture with this
# checkout's fno-agents build present. The smoke pytest legs delete
# crates/fno-agents/target/debug/fno-agents and target/release/fno-agents, so
# the fixture skips there. Discovery runs this file after the build step.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../cli"

# The fixture skips when find_dev_binary() finds nothing. Ask the same function
# first, so a missing build fails here instead of passing as a run of skips.
if ! uv run python -c 'import sys; from fno.rust_binary import find_dev_binary; sys.exit(0 if find_dev_binary() else 1)'; then
  echo "test-dev-build-suites: no fno-agents dev build; run: cargo build --manifest-path crates/fno-agents/Cargo.toml" >&2
  exit 1
fi

# Two door tests journal through the native event store, whose client resolves
# FNO_BIN or the checkout's own crates/fno build (store_client.py). The court
# fixture pins PATH to a fake bin, so a PATH fno cannot answer at test time.
fno_bin=${FNO_BIN:-}
if [ -z "$fno_bin" ]; then
  for profile in debug release; do
    if [ -x "../crates/fno/target/$profile/fno" ]; then
      fno_bin="../crates/fno/target/$profile/fno"
      break
    fi
  done
fi
if [ -z "$fno_bin" ]; then
  echo "test-dev-build-suites: no fno front-door build; run: cargo build --manifest-path crates/fno/Cargo.toml" >&2
  exit 1
fi

# Select by the fixture name, so a new file that takes it joins with no edit.
# grep exits 1 on no candidate and pytest exits 5 on an empty -m selection;
# both fail this harness.
files=$(grep -rl --include='test_*.py' native_backlog_door tests)
# shellcheck disable=SC2086  # repo test paths carry no spaces
uv run pytest --tb=short -q -n auto --maxprocesses=4 --dist=loadgroup -m dev_build $files
