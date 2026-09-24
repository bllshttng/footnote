---
type: llm
focus: { source: file, path: findings.md }
---

PASS if the document states all three facts and ties each one to `client.py` by a line number, the `fetch` function, or quoted code: (1) a 429 is retried at most 3 times; (2) with no usable Retry-After header the wait doubles from 1 second (1, 2, 4 seconds); (3) a numeric Retry-After header replaces that wait, but a date-form Retry-After is ignored and the doubling wait is used instead.
FAIL if any of the three facts is missing or wrong, or if the document cites nothing in `client.py`.
