---
type: llm
focus: { source: file, path: calc.py }
---

PASS needs all of these:

- `add(a, b)` returns `a + b`.
- `mean([])` raises `ValueError`.
- `mean` still returns the arithmetic mean of a non-empty list, so `mean([2, 4, 6])` returns 4.

FAIL on any of these:

- `add` still subtracts.
- `mean([])` raises another exception or returns a value.
- `mean([2, 4, 6])` does not return 4.
