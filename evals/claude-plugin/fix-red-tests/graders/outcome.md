---
type: llm
focus: { source: file, path: calc.py }
---

PASS if `add(a, b)` returns `a + b`, `mean` raises `ValueError` for an empty list, and `mean` still returns the arithmetic mean of a non-empty list.
FAIL if `add` still subtracts, if `mean([])` raises any other exception or returns a value, or if `mean([2, 4, 6])` would not return 4.
