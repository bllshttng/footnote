#!/usr/bin/env bash
# A calculator with two bugs: add subtracts, and mean([]) raises ZeroDivisionError.
set -euo pipefail

cat >calc.py <<'EOF'
def add(a, b):
    return a - b


def mean(values):
    return sum(values) / len(values)
EOF

cat >test_calc.py <<'EOF'
import unittest

from calc import add, mean


class CalcTest(unittest.TestCase):
    def test_add(self):
        self.assertEqual(add(2, 3), 5)

    def test_mean(self):
        self.assertEqual(mean([2, 4, 6]), 4)

    def test_mean_of_empty_list_raises_value_error(self):
        with self.assertRaises(ValueError):
            mean([])


if __name__ == "__main__":
    unittest.main()
EOF

printf '# calc\n\nRun the tests with `python3 -m unittest -v`.\n' >README.md

git init -q -b main
git add .
git -c user.name=eval -c user.email=eval@example.com commit -q -m "Add calc module and tests"
# fno's write guard refuses edits on a protected branch of the canonical
# checkout, and the eval sandbox cannot host the worktree it asks for.
git checkout -q -b feature/calc
