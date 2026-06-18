#!/usr/bin/env bash
#
# Detect whether RTK (https://github.com/anthropics/rtk-cli) is installed
# and reachable on PATH. Always exits 0; the wizard reads stdout to branch.
#
# stdout contract (exactly one line):
#   missing                    - rtk not on PATH
#   installed:<version-string> - rtk responds within 60s (truncated to 256 bytes)
#   error:<reason>             - rtk on PATH but version probe failed or hung
#
# Pure read, no file writes. bash 3.2 portable (macOS default shell).

set -euo pipefail

if ! command -v rtk >/dev/null 2>&1; then
  echo "missing"
  exit 0
fi

# macOS does not ship `timeout` by default; coreutils via Homebrew gives
# us `gtimeout`. Degrade gracefully if neither is available — the wizard
# still gets a usable answer, just without the hang ceiling.
TIMEOUT_BIN=""
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT_BIN="timeout"
elif command -v gtimeout >/dev/null 2>&1; then
  TIMEOUT_BIN="gtimeout"
fi

if [[ -n "$TIMEOUT_BIN" ]]; then
  VERSION_OUT=$("$TIMEOUT_BIN" 60 rtk --version 2>&1) || {
    rc=$?
    if [[ $rc -eq 124 ]]; then
      echo "error:rtk --version timed out after 60s"
    else
      # Bash-native: avoid `printf | head -c | tr` (the pipe can SIGPIPE
      # under `set -o pipefail` on large output, violating the
      # always-exit-0 contract). Also strips \r — `tr -d '\n'` did not.
      MSG="${VERSION_OUT//[$'\r\n']/}"
      MSG="${MSG:0:256}"
      echo "error:rtk --version exit $rc: $MSG"
    fi
    exit 0
  }
else
  # Bash-native watchdog: required when neither timeout nor gtimeout is on
  # PATH (stock macOS without coreutils). Without this, a hung `rtk --version`
  # would deadlock the wizard with zero signal — AC1-EDGE invariant violation.
  TMP_OUT=$(mktemp 2>/dev/null || echo "/tmp/detect-rtk.$$.out")
  trap 'rm -f "$TMP_OUT" 2>/dev/null' EXIT
  rtk --version >"$TMP_OUT" 2>&1 &
  RTK_PID=$!
  ( sleep 60 && kill -TERM "$RTK_PID" 2>/dev/null ) &
  WATCH_PID=$!
  # set -e is active; wait/kill must NOT abort the script when the child
  # exits non-zero (a non-zero rtk is a normal signal, not a script error).
  RTK_RC=0
  wait "$RTK_PID" 2>/dev/null || RTK_RC=$?
  kill -TERM "$WATCH_PID" 2>/dev/null || true
  wait "$WATCH_PID" 2>/dev/null || true
  VERSION_OUT=$(cat "$TMP_OUT" 2>/dev/null || printf '')
  if [[ $RTK_RC -ne 0 ]]; then
    # SIGTERM from the watchdog produces rc=143 (128+15) or, on some shells,
    # the signal name. Either way, treat any non-zero as timeout when the
    # output is empty or matches no-output-after-60s.
    if [[ $RTK_RC -eq 143 || $RTK_RC -eq 124 ]]; then
      echo "error:rtk --version timed out after 60s"
    else
      MSG="${VERSION_OUT//[$'\r\n']/}"
      MSG="${MSG:0:256}"
      if [[ -z "$MSG" ]]; then
        MSG="(no output)"
      fi
      echo "error:rtk --version exit $RTK_RC: $MSG"
    fi
    exit 0
  fi
fi

VERSION="${VERSION_OUT//[$'\r\n']/}"
VERSION="${VERSION:0:256}"
echo "installed:$VERSION"
