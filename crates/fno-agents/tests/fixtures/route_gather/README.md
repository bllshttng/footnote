# route_gather goldens

Characterization of the Python config gather `fno.route_resolve._slot_payload`
(and the inventory fold `_inventory_payload(resolve_inventory())`) captured at
origin/main `dea93127389b` (worktree branch base, 2026-09-30), before the port
moved the gather into `crates/fno-agents/src/route_gather.rs`. Each case is one
JSON file: `config_toml`, `transport_payload` (the keys the caller supplies),
`expect_payload` (the full payload `_slot_payload` sent to the route-slot
binary, gathered keys included), and `expect_inventory`
(`_inventory_payload(resolve_inventory())`). The parity test
`crates/fno-agents/tests/route_gather_parity.rs` builds the transport payload,
runs `route_gather::fill`, and compares against `expect_payload`; the
`payload_fingerprint` over the filled payload must equal the fingerprint
computed over `expect_payload`.

## Capture recipe (cannot run after the Python gather is deleted)

Run from the repo root with the merge-base binary pinned:

```bash
FNO_AGENTS_BIN=<merge-base fno-agents> \
  uv run --project cli python <capture_goldens.py>
```

Per case: write `config_toml` to a temp file and pin the hermetic environment
(`FNO_CONFIG`, empty `FNO_STATE_DIR`/`FNO_AGENTS_HOME`, `FNO_EVENTS_PATH`,
`FNO_NO_CANONICAL_CONFIG=1`, harness markers scrubbed); resolve the inventory
(`resolve_inventory()`); call `resolve_slot(...)` with a spy over
`route_slot_call` that records the payload it was handed; record
`_inventory_payload(inventory)`. Three configs are covered: no
`routing.models`, a declared inventory with lanes and `by_difficulty`, and a
strict config with `enforce_inventory = true`.
