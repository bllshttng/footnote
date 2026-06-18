#!/usr/bin/env bash
# capture-readiness-grid.sh — pin the interactive-TUI readiness prompt for
# codex / gemini (Phase 6 Wave 2, Open Questions #2 / #3).
#
# The Rust readiness detectors (crates/fno-agents/src/readiness.rs) decide an
# agent is "ready for input" by matching a prompt glyph on the rendered grid's
# last non-blank line (PROMPT_GLYPHS) and rejecting busy / auth-wall states.
# Those glyphs are best-known from documentation, NOT yet pinned against a live
# interactive CLI. This script spawns each CLI under a PTY, lets it draw its
# idle composer, captures the rendered screen, and prints the last visible line
# plus a hexdump so a human can confirm the real prompt glyph and update
# PROMPT_GLYPHS if it differs.
#
# Usage:
#   READINESS_SMOKE=1 bash scripts/smoke/capture-readiness-grid.sh [codex|gemini]
#
# A faithful grid render needs `pyte` (pip install pyte); without it the script
# falls back to a naive ANSI strip that does NOT handle cursor save/restore
# animations (codex's startup), so the fallback capture is best-effort only.
#
# Without READINESS_SMOKE=1 the script prints a skip note and exits 0 so CI
# hosts (and the default test run) skip it. It NEVER sends input to the CLI; it
# only observes startup, then SIGTERMs the child. Safe to abort with Ctrl-C.
#
# Outputs (per provider) under cli/tests/agents/fixtures/:
#   readiness-grid-<provider>.txt   — rendered visible screen
#   readiness-grid-<provider>.hex   — hexdump of the last non-blank line
#
# Exit codes: 0 ok/skip, 14 provider not on PATH, 11 empty capture.

set -euo pipefail

if [[ "${READINESS_SMOKE:-0}" != "1" ]]; then
    echo "capture-readiness-grid: READINESS_SMOKE!=1, skipping (set READINESS_SMOKE=1 to run)" >&2
    exit 0
fi

PROVIDER="${1:-codex}"
case "$PROVIDER" in
    codex|gemini) ;;
    *) echo "capture-readiness-grid: provider must be codex|gemini, got '$PROVIDER'" >&2; exit 2 ;;
esac

if ! command -v "$PROVIDER" >/dev/null 2>&1; then
    echo "capture-readiness-grid: $PROVIDER CLI not on PATH" >&2
    exit 14
fi
if ! command -v python3 >/dev/null 2>&1; then
    echo "capture-readiness-grid: python3 not on PATH" >&2
    exit 14
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLI_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
FIX_DIR="${CLI_ROOT}/tests/agents/fixtures"
mkdir -p "$FIX_DIR"

OUT_TXT="${FIX_DIR}/readiness-grid-${PROVIDER}.txt"
OUT_HEX="${FIX_DIR}/readiness-grid-${PROVIDER}.hex"

# Spawn the CLI under a PTY, read ~4s of startup output, render it through a
# terminal-state parser, and dump the visible screen + last non-blank line. The
# child is SIGTERM'd after the read window; no input is ever sent.
CAP_TXT="$(PROVIDER="$PROVIDER" python3 - "$PROVIDER" <<'PY'
import os, pty, select, signal, sys, time

provider = sys.argv[1]
# Interactive composer mode (no -p / exec one-shot): we want the idle prompt.
argv = {
    "codex": ["codex"],
    "gemini": ["gemini", "--skip-trust"],
}[provider]

pid, fd = pty.fork()
if pid == 0:  # child
    os.execvp(argv[0], argv)
    os._exit(127)

buf = bytearray()
deadline = time.time() + 4.0
try:
    while time.time() < deadline:
        r, _, _ = select.select([fd], [], [], 0.2)
        if fd in r:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            buf.extend(chunk)
finally:
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass

# Render the captured bytes into a screen grid. Prefer pyte if available; else
# fall back to a naive ANSI strip so the script still yields a usable capture.
text = ""
try:
    import pyte  # type: ignore
    screen = pyte.Screen(120, 40)
    stream = pyte.ByteStream(screen)
    stream.feed(bytes(buf))
    text = "\n".join(line.rstrip() for line in screen.display).rstrip("\n")
except Exception:
    import re
    stripped = re.sub(rb"\x1b\[[0-9;?]*[ -/]*[@-~]", b"", bytes(buf))
    stripped = re.sub(rb"\x1b[\]P^_].*?(\x07|\x1b\\)", b"", stripped, flags=re.S)
    text = stripped.decode("utf-8", "replace")

sys.stdout.write(text)
PY
)"

if [[ -z "${CAP_TXT//[[:space:]]/}" ]]; then
    echo "capture-readiness-grid: empty capture for $PROVIDER (auth wall? trust prompt?)" >&2
    exit 11
fi

printf '%s\n' "$CAP_TXT" > "$OUT_TXT"

# Last non-blank line is what the detector matches a trailing glyph against.
LAST_LINE="$(printf '%s\n' "$CAP_TXT" | awk 'NF{last=$0} END{print last}')"
printf '%s' "$LAST_LINE" | hexdump -C > "$OUT_HEX" 2>/dev/null || \
    printf '%s' "$LAST_LINE" | od -An -tx1 > "$OUT_HEX"

echo "capture-readiness-grid: wrote $OUT_TXT and $OUT_HEX" >&2
echo "--- last non-blank line (confirm the prompt glyph vs PROMPT_GLYPHS) ---" >&2
printf '%s\n' "$LAST_LINE" >&2
