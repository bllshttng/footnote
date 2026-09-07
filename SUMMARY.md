# SUMMARY: x-e221 territory-is-one-list

Branch `feature/x-e221`, four commits, one per wave:

- 386b394f4 + 1f4dc8b06 (wave 1): canonical territory key and membership (Python `territory_membership`, Rust `compile_territory`), the widened drain receipt, and the scope-keyed supervisor.
- 46edeade6 (wave 2): `agents.max_live_per_territory` (default 4) enforced by both spawn gates, exit 82, shared parity fixture.
- 60bdbb1d4 (wave 3): the standing scope-keyed blueprinter: `fno agents worker blueprint-feed` (status/deliver/repair) plus the Rust tick that spawns at most one replacement per tick through the standard gates.
- 6b47cac6e (wave 4): `territory_rows()` readout projection, the `config active-backlog-territories` verb, the AC8 board pin, reign guidance, and `scripts/repro-x-e221.sh`.

## Deviations from the plan

- Task 3.2's `verify` runs `bash scripts/repro-x-e221.sh`, but that script is created by task 4.2. Execution order kept the plan's wave order; the script was run after 4.2 and passes all markers (the wave-3 recovery paths it exercises were implemented with 3.1).
- The plan's verify names `cli/tests/agents/test_king_court.py` and `cli/tests/unit/test_dashboard_behavior.py`; neither exists in the tree. The real neighboring suites were used instead: `test_crown_court.py`, `test_king_board_default_state.py` (Python) and the `king_board` lib tests (Rust), plus a new dedicated test per acceptance criterion.
- The stop hook needed no change: its actionable set is the session's own plan and node by construction (target-state manifest), so it is territory-scoped already and the plan's "stop-hook actionable set" clause is satisfied without edits.
- `dispatch_member`'s argv order regressed in wave 1 (`--json` moved first); restored to the documented seam `advance --epic <id> --continuation --json`, which the integration test pins.
- The new `max_live_per_territory` config field staled three pinned seams, each caught by its own gate: the committed config-doc references (schema-drift tests), the provider loader's reserved-keys frozenset (set-equality test against the schema), and the route-survival gate-event tuple. The repro script's `timeout || gtimeout` preference chain was rewritten onto the shared `with_timeout` bound when the single-implementation guard flagged it.
- Task 1.1 test coverage consolidated `test_crown_level_derivation.py` cases into `test_king_scope.py` (noted at that wave).

## Design notes worth keeping

- Blueprinter feed selects plan rungs IDEA and DESIGN only: those are the nodes the drain can never dispatch (`UNSELECTABLE_RUNGS`), so the blueprinter and the cold-dispatch lane never race the same node.
- Blueprinter liveness rides the spawn gate's census (`live_registry_names` exposed), one liveness oracle for the whole fleet.
- The fed ledger self-heals: failed delivery retries after 30 minutes, a delivered idea still un-ready re-delivers after 24 hours; closed nodes are pruned on every read.
- `territory_rows` reads membership as an explicit `unknown` rather than a zero, and the loose territory's live count excludes crowned nodes, matching the gate's exclusive-membership rule.
- The repro script's isolation guard refuses to write unless the graph path resolves inside the temp home; it exists because an unisolated test writer overwrote the production graph during this very node's execution.
