---
type: llm
---

PASS needs all of these:

- The reply says the change is not ready to ship.
- It names the page off-by-one in `paginate`.
- It gives the cause: pages start at 1, but `start = page * size` skips the first page. The start must be `(page - 1) * size`.

FAIL on any of these:

- The reply calls the change ready to ship.
- The reply does not name this off-by-one.
