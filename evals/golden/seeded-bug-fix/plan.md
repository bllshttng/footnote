---
status: ready
---
# Fix off-by-one bug in clamp

## Goal

`test_clamp_at_hi` currently fails because `clamp` in `interval.py` has an
off-by-one bug. Fix the bug in `interval.py` without modifying the test file.

## Tasks

1. Run `python -m pytest` to see the failing test.
2. Read `interval.py` and identify the off-by-one error in `clamp`.
3. Fix `clamp` so all tests pass.

## Acceptance Criteria

- `python -m pytest` exits 0.
- `test_clamp_at_hi` passes.
- `test_interval.py` content is unchanged.
