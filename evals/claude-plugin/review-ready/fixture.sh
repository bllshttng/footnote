#!/usr/bin/env bash
# The latest commit adds 1-based pagination with an off-by-one: page 1 skips
# the first page. Its test checks page 2 against the buggy slice, so it passes.
set -euo pipefail

cat >store.py <<'EOF'
ITEMS = [f"item-{n}" for n in range(30)]


def list_items():
    return list(ITEMS)
EOF

printf '# store\n\nRun the tests with `python3 -m unittest -v`.\n' >README.md

git init -q -b main
git add .
git -c user.name=eval -c user.email=eval@example.com commit -q -m "Add item store"
git checkout -q -b feature/pagination

cat >pager.py <<'EOF'
def paginate(items, page, size=10):
    """Return page `page` of `items`. Pages are numbered from 1."""
    start = page * size
    return items[start:start + size]
EOF

cat >test_pager.py <<'EOF'
import unittest

from pager import paginate


class PaginateTest(unittest.TestCase):
    def test_page_two(self):
        self.assertEqual(paginate(list(range(30)), 2), list(range(20, 30)))


if __name__ == "__main__":
    unittest.main()
EOF

git add .
git -c user.name=eval -c user.email=eval@example.com commit -q -m "Add pagination for list_items"
