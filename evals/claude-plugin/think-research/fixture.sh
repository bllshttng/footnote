#!/usr/bin/env bash
# An HTTP client whose 429 retry has three facts to find, one of them subtle:
# a date-form Retry-After header is ignored.
set -euo pipefail

cat >client.py <<'EOF'
import time
import urllib.request
from urllib.error import HTTPError

MAX_RETRIES = 3
BASE_DELAY = 1.0


def fetch(url, opener=urllib.request.urlopen, sleep=time.sleep):
    """GET url, retrying on HTTP 429."""
    attempt = 0
    while True:
        try:
            return opener(url)
        except HTTPError as err:
            if err.code != 429 or attempt >= MAX_RETRIES:
                raise
            retry_after = err.headers.get("Retry-After")
            if retry_after is not None and retry_after.isdigit():
                delay = float(retry_after)
            else:
                delay = BASE_DELAY * (2 ** attempt)
            sleep(delay)
            attempt += 1
EOF

printf '# client\n\nA small HTTP client.\n' >README.md

git init -q -b main
git add .
git -c user.name=eval -c user.email=eval@example.com commit -q -m "Add HTTP client with 429 retry"
# fno's write guard refuses edits on a protected branch of the canonical
# checkout, and the eval sandbox cannot host the worktree it asks for.
git checkout -q -b feature/client-notes
