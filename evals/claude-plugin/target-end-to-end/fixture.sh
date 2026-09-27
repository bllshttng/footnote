#!/usr/bin/env bash
# A login service with no limit on failed attempts.
set -euo pipefail

cat >app.py <<'EOF'
"""Tiny login service."""

USERS = {"ada": "lovelace", "alan": "turing"}


def login(username, password):
    """Return True when the credentials match."""
    return USERS.get(username) == password
EOF

cat >test_app.py <<'EOF'
import unittest

from app import login


class LoginTest(unittest.TestCase):
    def test_correct_password(self):
        self.assertTrue(login("ada", "lovelace"))

    def test_wrong_password(self):
        self.assertFalse(login("ada", "babbage"))


if __name__ == "__main__":
    unittest.main()
EOF

printf '# login\n\nRun the tests with `python3 -m unittest -v`.\n' >README.md

git init -q -b main
git add .
git -c user.name=eval -c user.email=eval@example.com commit -q -m "Add login service"
# fno's write guard refuses edits on a protected branch of the canonical
# checkout, and the eval sandbox cannot host the worktree it asks for.
git checkout -q -b feature/login-limit
