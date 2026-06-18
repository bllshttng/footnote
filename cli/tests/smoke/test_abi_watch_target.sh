#!/usr/bin/env bash
# Smoke test: abi-watch resolves its fswatch target to the thread store's
# per-project inbox DIRECTORY (inbox_root_for) - the SAME path `fno mail drain`
# reads - NOT a flat inbox.md the store never writes.
#
# Regression for ab-d3e7da36: the PRIMARY branch used to resolve
# paths_inbox_thread "$PROJECT/inbox.md" (a flat file under
# $REPO_ROOT/.fno/inbox) so the daemon never woke to drain real thread
# messages. The fallback was fixed in PR #430; this pins the primary path.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
CLI_DIR="$REPO_ROOT/cli"
DAEMON="$REPO_ROOT/scripts/abi-watch.sh"
PROJECT="smoke-watch-target"

# Extract _resolve_inbox_path from the daemon (bash 3.2 compat: eval an awk
# slice, the same idiom as test_daemon_e2e.sh - no `source <(...)`).
eval "$(awk '
  /^_resolve_inbox_path\(\)/ { f=1 }
  f { print }
  f && /^\}$/ { f=0 }
' "$DAEMON")"

# Fail loudly if the awk slice did not yield the function (renamed, reformatted
# with a brace on its own line, or a bare `}` introduced in the body) rather
# than eval-ing a truncated definition and failing confusingly downstream.
if ! declare -f _resolve_inbox_path >/dev/null; then
  echo "FAIL: could not extract _resolve_inbox_path from $DAEMON" >&2
  exit 1
fi

# The function reads STATE_DIR (no-vault fallback) and CLI_DIR/PROJECT (above).
STATE_DIR="${STATE_DIR:-$HOME/.fno}"

GOT="$(_resolve_inbox_path)"
WANT="$(WATCH_PROJECT="$PROJECT" uv run --project "$CLI_DIR" python3 -c 'import os
from fno.paths import inbox_root_for
print(inbox_root_for(os.environ["WATCH_PROJECT"]))')"

FAIL=0
if [[ "$GOT" != "$WANT" ]]; then
  echo "FAIL: watch target '$GOT' != inbox_root_for '$WANT'" >&2
  FAIL=1
fi
case "$GOT" in
  *inbox.md)
    echo "FAIL: watch target is a flat inbox.md the store never writes: $GOT" >&2
    FAIL=1
    ;;
esac
if [[ "$FAIL" -ne 0 ]]; then
  echo "FAIL" >&2
  exit 1
fi
echo "OK: abi-watch target == inbox_root_for($PROJECT) = $GOT"
