---
type: llm
focus: { source: file, path: app.py }
---

PASS needs all of these:

- `login` refuses more attempts for a user after a fixed number of failed attempts inside a time window.
- A correct password under that limit still returns True.
- The limit lives in `login`, or in a helper that `app.py` defines or imports and `login` calls.

FAIL on any of these:

- `login` has no attempt limit.
- The limit exists only in a comment or docstring.
- Correct credentials under the limit are refused.
