"""The style gate on ``fno agents mail send``.

Two claims, tested two ways:

1. STRUCTURAL: every authored-body entry point runs the style check. The guard
   is the lockstep between ``_enforce_body_cap`` and ``_enforce_style`` in
   ``cli/src/fno/mail/cli.py``: a new body-cap call without a paired style check
   fails this test rather than a review. This is NOT a snapshot of today's
   callers (that would pass forever); it is a relation between two calls.
2. DYNAMIC: a subprocess ``fno agents mail send`` with a violating body exits 1 and
   names the rule, before any delivery. The escape (``--style-exception``) and
   the kill switch (``FNO_STYLE_ENFORCE=0``) let a violating body through.
"""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

REPO_CLI = Path(__file__).resolve().parents[1]
MAIL_CLI = REPO_CLI / "src" / "fno" / "mail" / "cli.py"

STYLE_REFUSED = "message blocked by the style rules"


def _run(args: list[str], env_extra: dict[str, str], tmp_path: Path):
    """Run ``fno agents mail send`` in a sandboxed state dir; return the CompletedProcess."""
    state = tmp_path / ".fno"
    (state / "agents").mkdir(parents=True, exist_ok=True)
    settings = state / "settings.yaml"
    settings.write_text(f"schema_version: 1\nconfig:\n  state_dir: {state}/\n")
    (state / ".path-migration-done").touch()
    env = {
        "PATH": "/usr/bin:/bin",
        "HOME": str(tmp_path),
        "FNO_CONFIG": str(settings),
        "PYTHONPATH": str(REPO_CLI / "src"),
        **env_extra,
    }
    return subprocess.run(
        [sys.executable, "-m", "fno.cli", "mail", "send", *args],
        capture_output=True,
        text=True,
        env=env,
        cwd=str(tmp_path),
        timeout=120,
    )


def test_every_body_cap_call_is_paired_with_a_style_check():
    """The structural guard: adding a body-cap site without a style check fails here.

    Enumerates every ``_enforce_body_cap(...)`` CALL (not the def) and asserts
    the next non-blank line calls ``_enforce_style(...)``. A new authored-body
    path that caps bytes but skips style breaks this, which is the point: the
    byte gate and the structure gate must not drift apart on a path.
    """
    lines = MAIL_CLI.read_text(encoding="utf-8").splitlines()
    call_lines = [
        i
        for i, line in enumerate(lines)
        if "_enforce_body_cap(" in line and not line.lstrip().startswith("def ")
    ]
    assert call_lines, "expected at least one _enforce_body_cap call site"
    for i in call_lines:
        following = next(lines[j] for j in range(i + 1, len(lines)) if lines[j].strip())
        assert "_enforce_style(" in following, (
            f"_enforce_body_cap at line {i + 1} is not followed by _enforce_style: "
            f"{following!r}"
        )


def test_violating_body_is_refused_before_delivery(tmp_path):
    proc = _run(["worker", "you should fix this; it breaks."], {}, tmp_path)
    assert proc.returncode == 1, proc.stderr
    assert STYLE_REFUSED in proc.stderr, proc.stderr
    # The refusal names the rule it broke, not just the gate.
    assert "rule 3" in proc.stderr and "rule 2" in proc.stderr, proc.stderr


def test_clean_body_passes_the_style_gate(tmp_path):
    proc = _run(["worker", "the build is green and the tests pass."], {}, tmp_path)
    assert STYLE_REFUSED not in proc.stderr, proc.stderr


def test_style_exception_flag_bypasses_the_gate(tmp_path):
    proc = _run(
        ["worker", "you should fix this; it breaks.", "--style-exception", "legacy one-off"],
        {},
        tmp_path,
    )
    assert STYLE_REFUSED not in proc.stderr, proc.stderr


def test_kill_switch_disables_the_gate(tmp_path):
    proc = _run(
        ["worker", "you should fix this; it breaks."],
        {"FNO_STYLE_ENFORCE": "0"},
        tmp_path,
    )
    assert STYLE_REFUSED not in proc.stderr, proc.stderr
