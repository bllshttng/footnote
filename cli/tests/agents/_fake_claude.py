"""Fake-claude fixture helper for provider integration tests.

Writes a small Python script that mimics ``claude --bg``'s output contract
(``backgrounded · <8hex> · <name>``) so the provider's subprocess path can
be exercised without the real ``claude`` CLI.

The fake honors three knobs via environment variables read at run time:

- ``FAKE_CLAUDE_EXIT``: integer exit code (default 0).
- ``FAKE_CLAUDE_STDOUT``: the literal stdout to print. If unset, the fake
  prints ``backgrounded · 7c5dcf5d · <name>`` derived from the ``--name``
  argv.
- ``FAKE_CLAUDE_STDERR``: stderr text to emit (default empty).
- ``FAKE_CLAUDE_STDIN_DUMP``: if set, path to dump received stdin so the
  argv-overflow test can verify the 300KB message arrived intact.

This file is a test fixture; nothing in ``src/fno/`` imports it.
"""
from __future__ import annotations

import os
import stat
import sys
from pathlib import Path

# Bake the actual python interpreter into the shebang so the test doesn't
# rely on `python3` being on the test-isolated PATH.
FAKE_SCRIPT = f"""#!{sys.executable}
import os, sys
exit_code = int(os.environ.get("FAKE_CLAUDE_EXIT", "0"))
stdout_override = os.environ.get("FAKE_CLAUDE_STDOUT")
stderr_text = os.environ.get("FAKE_CLAUDE_STDERR", "")
stdin_dump = os.environ.get("FAKE_CLAUDE_STDIN_DUMP")

argv = sys.argv[1:]
name = None
for i, tok in enumerate(argv):
    if tok == "--name" and i + 1 < len(argv):
        name = argv[i + 1]
        break

if stdin_dump:
    data = sys.stdin.read()
    with open(stdin_dump, "w") as f:
        f.write(data)

if stdout_override is not None:
    sys.stdout.write(stdout_override)
else:
    sys.stdout.write(f"backgrounded · 7c5dcf5d · {{name or 'unknown'}}\\n")

if stderr_text:
    sys.stderr.write(stderr_text)

sys.exit(exit_code)
"""


def install_fake_claude(bin_dir: Path) -> Path:
    """Write the fake-claude script into ``bin_dir`` and chmod +x it.

    Returns the path to the installed fake-claude binary.
    """
    bin_dir.mkdir(parents=True, exist_ok=True)
    target = bin_dir / "claude"
    target.write_text(FAKE_SCRIPT, encoding="utf-8")
    mode = target.stat().st_mode
    target.chmod(mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return target


def configure_fake(
    monkeypatch: object,
    *,
    exit_code: int = 0,
    stdout: str | None = None,
    stderr: str | None = None,
    stdin_dump: str | None = None,
) -> None:
    """Set env vars that the fake-claude script reads at run time."""
    monkeypatch.setenv("FAKE_CLAUDE_EXIT", str(exit_code))  # type: ignore[attr-defined]
    if stdout is not None:
        monkeypatch.setenv("FAKE_CLAUDE_STDOUT", stdout)  # type: ignore[attr-defined]
    else:
        monkeypatch.delenv("FAKE_CLAUDE_STDOUT", raising=False)  # type: ignore[attr-defined]
    if stderr is not None:
        monkeypatch.setenv("FAKE_CLAUDE_STDERR", stderr)  # type: ignore[attr-defined]
    else:
        monkeypatch.delenv("FAKE_CLAUDE_STDERR", raising=False)  # type: ignore[attr-defined]
    if stdin_dump is not None:
        monkeypatch.setenv("FAKE_CLAUDE_STDIN_DUMP", stdin_dump)  # type: ignore[attr-defined]
    else:
        monkeypatch.delenv("FAKE_CLAUDE_STDIN_DUMP", raising=False)  # type: ignore[attr-defined]
