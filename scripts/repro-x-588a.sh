#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
  target_dir="$CARGO_TARGET_DIR"
else
  target_dir="$(mktemp -d /tmp/fno-x588a-target.XXXXXX)"
  trap 'rm -rf "$target_dir"' EXIT
fi

run_test() {
  local filter="$1"
  CARGO_TARGET_DIR="$target_dir" RUSTC_WRAPPER= cargo test \
    --manifest-path "$root_dir/crates/fno/Cargo.toml" "$filter" --lib
}

printf '%s\n' 'x-588a repro: stale pane selection must refuse'
run_test resolve_selector_refuses_a_paneless_live_row_over_a_stale_pane_ref

printf '%s\n' 'x-588a repro: pane ids must advance across restart reservation'
run_test pane_id_reservation_survives_restart_and_advances_past_floor

printf '%s\n' 'x-588a repro: pane send must refuse a captured-identity mismatch'
run_test pane_send_refuses_when_registry_name_disagrees_with_pane_identity

printf '%s\n' 'x-588a repro: positive markers observed'
