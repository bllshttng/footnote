---
type: llm
focus: { source: file, path: findings.md }
---

PASS needs all of these facts, each tied to `client.py` by a line number, the `fetch` function, or quoted code:

- A 429 is retried at most 3 times.
- With no usable Retry-After header, the wait doubles from 1 second: 1, 2, then 4 seconds.
- A numeric Retry-After header replaces that wait.
- A date-form Retry-After is ignored, and the doubling wait applies instead.

FAIL on any of these:

- One of the facts above is missing or wrong.
- The document cites nothing in `client.py`.
