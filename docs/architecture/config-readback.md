# Reading the config back

An operator edits a config file and wants one answer. Did the edit take effect?

`fno config get <leaf>` answers it one leaf at a time. Its `source:` line names the deciding file and the files it overrode, and that line is the only place precedence is visible. It requires already knowing which leaf to ask about.

`fno config doctor` is the verb an operator reaches for instead. Its verdict used to be computed from a config it never proved it read. The checks in `cli/src/fno/config_readback.py` close that gap. `cli/src/fno/setup/doctor.py` only prints them.

## The parse the doctor can see

`_load_raw` returns `({}, False)` on any read or parse error and logs a warning, so one bad file never makes every fno command exit. Nothing above it can say which file failed.

`_read_settings_doc` is now the one parser. `_parse_settings` is the operator-facing view over it. It reports a read failure, and it also calls a non-table document a shape error. `_load_raw` is a wrapper over the same parser and keeps its exact signature, its `ok` semantics and its warning text.

Measured 2026-09-05 with `FNO_CONFIG` pinned to a scratch file. A `config.toml` holding YAML: the doctor printed the file as the settings source, printed `[doctor] OK; no suspicious paths detected.`, and exited 0. A `settings.yaml` holding a YAML list did the same with no warning at all. A list parses cleanly and then becomes `{}` at `config_io.py`'s `data if isinstance(data, dict) else {}`.

An empty file exits 0. That is correct and stays correct.

## The four checks

`check_config_files_read` names every candidate that exists and failed to parse, and every document that parsed to a non-table, with the type it parsed to.

`check_unknown_keys` walks each candidate layer SEPARATELY rather than the merged result, so every message names the file that actually holds the key. It reads `_aliased_layers`, the loader's own per-file collector, so a legacy spelling the loader accepts is never called a typo. `extra="ignore"` is forward compatibility. It also means a typo'd section or leaf is accepted in silence. `warn_unknown_keys` already found them and, without `FNO_DEBUG`, said nothing.

`check_enabled_with_empty_population` reports a switch that is on with nothing that can satisfy it. `review.cross_model` is the one pair here, and it is here because its consumer was read. `review_assurance` widens the reviewer set from `available_provider_kinds()`. With no dispatchable non-claude provider the diversity requirement can never be met.

`check_wip_caps` reads the `config.toml` sibling as well as the legacy `settings.yaml`. It read only the yaml name before, so it had been a no-op on every machine since the yaml-to-toml migration.

## Two bounds the report needs

Both were found by running the check against a real machine config rather than by reading it. The first run returned 31 findings on an install that works.

**A `dict[str, Model]` field's keys are data, not schema.** `warn_unknown_keys` resolved the annotation to its VALUE model. It then checked the map's own keys against that model's fields. So `agents.profiles.blueprint` read as a typo for a field name. Nineteen of the thirty-one were entries in such a block: `agents.profiles`, `work.workspaces`, `model_routing.providers`, `accounts.combos`, `agents.provider_limits`. It now recurses into each VALUE with the map key in the prefix, so a typo INSIDE a profile is still caught.

**A report has to stay readable.** An unknown table with more than `_UNKNOWN_LEAF_CAP` leaves reports as the table, not a line per leaf. A foreign tool's block sharing `~/.fno/config.toml` printed six lines and a non-zero exit nobody can clear. A leaf name shared by more than `_NEAR_MISS_CAP` sections gets no hint. `enabled` lives under 25 of them, and a hint naming all 25 is the schema dumped into a doctor line.

Down to 7 on the same config. One of the seven is a real typo: `target.dedupe_dead_duplicate`, where the model reads `dedupe_dead_duplicates`.

The node's own acceptance asked for every leaf name appearing in more than one section. Measured: 16 colliding names on a clean install. That report teaches an operator to ignore the doctor, so the check reports the operator's own wrong key instead.

## Naming the decider

When a config file decides `key`, `source_note(key)` returns `set in <file>`. Otherwise it returns `None`. It is the one renderer, reading `resolve_source`'s answer rather than re-deriving one. `_state_root_selector` in `config_cli.py` and the doctor's accessor loop both call it.

The settings-source line lists the files that CONTRIBUTED, from `_aliased_layers`, in precedence order. It named the highest-priority file PRESENT before, so an unreadable project config was printed as the source of values the global file decided.

## The documented schema must be the documented format

`docs/path-config.md` names `config.toml` files and used to hand the reader YAML in the legacy `config:`-wrapped shape. `tomllib` raises on it, `_load_raw` swallows the raise, the doctor certified the file. An operator who copied the documented schema into the documented path got a file that no-ops.

`cli/tests/unit/test_docs_fences.py` walks every fence in `docs/` and parses it as its declared language. Its non-zero fence count is the positive control on the walker. A walker that found no files can otherwise pass by finding nothing to reject.

## Two blocks the walker skips

`_UNMODELED_BLOCKS` holds `kanban` and `providers`. The board renderer reads `kanban` straight out of the file, so the model never carries it. `providers` is the pre-rename spelling of `accounts`. The loader's alias copies it across and leaves it in place. Both work. Neither is unknown.

## The module loads fno.config by importlib

`config_readback.py` reaches `fno.config` through `_cfg()`, which calls `importlib.import_module`. A plain import puts `fno.config` in a mypy strongly-connected component. There the lazy `__getattr__` re-exports in `graph/_constants.py` degrade to `Optional[Path]`, and two files this feature never touches fail.

The number came from a measurement, not from reasoning. `origin/main` is clean at 567 files. The branch reported three errors at 568. Hiding the module made them vanish, which named the cause. Removing the doctor's import edge did not, and neither did moving the module out of the package.

`fno.config._revoke_unbacked_optouts` records the same failure and the same remedy.

## Not here

The `auto_continue` specimen had a second half. A control-plane arms readout printed `reason=disabled` from a stored tick stamp. The stamp was taken 37 minutes before the edit that flipped the setting, and a king read it three times as current. That line is written by the arms code, not the config resolver, so no change here reaches it. It is filed on its own.
