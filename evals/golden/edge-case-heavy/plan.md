---
status: ready
---
# Implement parse_duration with edge-case handling

## Goal

`duration.py` has a stub `parse_duration(s)` that raises `NotImplementedError`.
Implement it so it parses human-readable duration strings into total seconds.

## Tasks

1. Implement `parse_duration(s: str) -> int` in `duration.py`.
2. Make all tests in `test_duration.py` pass.

## Acceptance Criteria

- AC1-HP: `parse_duration("90s")` returns `90`.
- AC2-HP: `parse_duration("5m")` returns `300`.
- AC3-HP: `parse_duration("1h30m")` returns `5400`.
- AC4-HP: `parse_duration("0s")` returns `0`.
- AC-EDGE1: `parse_duration("")` raises `ValueError`.
- AC-EDGE2: `parse_duration("abc")` raises `ValueError`.
- AC-EDGE3: `parse_duration("-5s")` raises `ValueError`.
