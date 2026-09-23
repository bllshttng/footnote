# Summary: one guarded Claude supervisor birth path

## What landed

- `claude_supervisor.rs` refuses real births for temporary config roots and hermetic runs. Fixture births remain allowed.
- Python `bg_create` crosses the fenced Rust birth-exec path. It preserves the client environment.
- Rust owns live and parked Claude resume delivery. Account-pinned routes read roster, control key and transcript from the plan's config root.
- Python wake and dead attach paths are removed. Python runtime refuses Claude resume.
- The env registry drives a compile-time supervisor classification test. This head has 163 `FNO_` rows: 34 kept, 118 held and 11 `FNO_TEST_` exemptions.
- `FNO_INBOX_ROOT` remains held. Its documented purpose is a test override.
- The CI path filter now includes `docs/env-vars.md`; the registry check therefore runs when its compile-time fixture changes.

## Plan correction

`FNO_WAKE_MSG` appears in the measured historical leak but has no reader after the Python wake path is removed. It stays poison through the default `FNO_` rule, while the registry-backed held list contains only names with current readers. The linked plan now tests that distinction.

The keep-list plan snapshot counted 161 rows; current `origin/main` had 162 before this branch added its test-only registry row. This head counts 163.

## Verification

- Python refusal target: 1 passed, 25 deselected.
- `python3 scripts/ci/check_env_registry.py`: 244 names, all rows agree; `check-python-static.sh`: all checks passed.
- Rust formatting, targeted Python Ruff/`py_compile`, `git diff --check`, and `check-no-internal-refs.sh`: passed.
- The focused Rust classifier first exposed the stale `FNO_WAKE_MSG` plan assertion; its corrected rerun passed.
- The stale-binary Python target waited 20m25s and was canceled before argv started; it has no local source verdict. CI will gate that path. No whole-suite local run was started.
