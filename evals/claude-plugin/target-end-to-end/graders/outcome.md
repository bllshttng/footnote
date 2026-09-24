---
type: llm
focus: { source: file, path: app.py }
---

PASS if `login` refuses further attempts for a user after a fixed number of failed attempts inside a time window, and a correct password under that limit still returns True. The limit can live in `login` itself or in a helper that `app.py` defines or imports and that `login` calls.
FAIL if `login` has no attempt limit, if the limit is only described in a comment or docstring, or if correct credentials under the limit are refused.
