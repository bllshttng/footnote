---
status: ready
---
# Refactor report.py into named functions

## Goal

`report.py` has one long `build_report` function that mixes parsing, formatting,
and summarizing. Split it into three named helper functions while keeping the
public API and all tests green.

## Tasks

1. Extract `parse_rows(lines)` - splits raw lines into `[str, int]` pairs.
2. Extract `format_row(name, count)` - returns a single formatted string.
3. Extract `summarize(rows)` - returns the total count across all rows.
4. Keep `build_report(text)` as the public entry point calling the three helpers.
5. Verify `python -m pytest` still passes.

## Acceptance Criteria

- `build_report` still accepts a multi-line string and returns the same output.
- The three functions `parse_rows`, `format_row`, `summarize` are importable.
- All tests pass without modification.
