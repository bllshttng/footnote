---
type: llm
---

PASS if the reply says the change is not ready to ship because of the page off-by-one in `paginate`: pages are documented to start at 1, but `start = page * size` makes page 1 skip the first `size` items, so the start must be `(page - 1) * size`.
FAIL if the reply calls the change ready to ship, or if it does not name this off-by-one.
