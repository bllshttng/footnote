# spawn_compose goldens

Characterization of the Python seam `fno.agents.spawn_defaults.inject_spawn_defaults`
captured at origin/main `dea93127389b` (worktree branch base, 2026-09-30),
before the port moved the composition into `crates/fno-agents/src/spawn_compose.rs`.
Each case is one JSON file: the inputs `compose(&Inputs)` takes, plus `expect`
(argv, stderr lines, exit, stdout, journal). The parity test
`crates/fno-agents/tests/spawn_compose_parity.rs` feeds `Inputs` to the pure
compose and compares the answer to `expect`.

## Capture recipe (cannot run after the Python leg is deleted)

Run from the repo root with the merge-base binary pinned, in this worktree's
Python environment:

```bash
FNO_AGENTS_BIN=<merge-base fno-agents> \
  uv run --project cli python <capture_goldens.py>
```

The capture script (kept out of the repo; its shape is the recipe):

1. For each case: write `config_toml` to a temp file and pin the hermetic
   environment: `FNO_CONFIG` -> that file, `FNO_STATE_DIR`/`FNO_AGENTS_HOME` ->
   empty temp dirs (capacity reads answer `unknown`, the registry reads
   empty), `FNO_EVENTS_PATH` -> a temp journal, `FNO_NO_CANONICAL_CONFIG=1`,
   harness-inference markers scrubbed so the ambient harness reads `claude`.
2. `normalize_spawn_args` the case `argv` (the transport's step; the golden
   `argv` is the normalized one).
3. Build `scan` from `argv[1:]` with the real scanners (`_scan`, `_flag_value`,
   `_flag_present`, `_role_of`, `_has_explicit_substrate`, `_has_permission_mode`,
   `_seed_slot`, `_seed_of`, `_positional_indices`) and `facts`
   (`role_resolves` via `model_routing.resolve_route`, `role_protected` via
   `PROTECTED_ROLES`).
4. Resolve the node the way the old body did: `--node` flag, else the
   `spawn-axes` `spawn_node` ask; `node_row` is the row the case plants
   (monkeypatched over `_grid_node`, exactly as the Python tests do).
5. Run `inject_spawn_defaults(argv, env={}, stderr=..., ...)`, catching
   `SystemExit`; read the journal's last `spawn_defaults_applied` row and drop
   `ts`. The `slot-exhausted-queue` case stubs `route_slot_call` to return an
   exhausted payload, because a hermetic capture cannot plant exhausted
   account-usage state; the walk's own queue behavior stays pinned by
   route_slot's in-file tests. The case carries `stub_route_slot: true`, and
   the Rust parity run skips stub-backed cases for the same reason.
6. Emit inputs + expect as one JSON file per case.

The `slot-exhausted-queue` stub and the planted `node_row`s are the only
non-live inputs; every other expected value was produced by the unmodified
merge-base Python and binary.

## The verbose field

The compose owns the routing-provenance line (`applied axis=value (source) (routing), ...`). A spawn prints what the user acts on. Only inputs carrying `verbose: true` get the line. Cases captured before that gate keep their captured stderr and carry `verbose: true`. `slot-pin-pick-quiet` is the same pin case without it, pinning the silent default.

## Hand-authored cases

`crown-codex-yolo.json`, `crown-codex-short-flag.json`, and `crown-codex-bounded-refuses.json` have no Python capture behind them. They pin the crown-codex posture the Rust compose owns. A crowned codex spawn defaults to `yolo` at rung `builtin.crown` (`--crown` on the first case, the attached `-kx-aaaa` spelling on the second). One that names a bounded mode refuses with exit 2. Each case is authored against the compose contract, in the same shape as the captured goldens.

## Post-capture drift applied

One case was refreshed after main moved under the capture: main merged
`56456eba70` (declare zcode a harness) after the goldens were taken, so the
`harness-unknown-config-provider` case's `valid:` roster grew `zcode`. The
composition did not change. Declaring `footnote` a harness grew the same roster by `footnote`, refreshed the same way.
