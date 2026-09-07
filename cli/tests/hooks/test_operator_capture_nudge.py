"""Hook tests for hooks/operator-capture-nudge.sh.

The hook is depth-gated: silent at 0, speaks with the literal count at N,
silent on a stale deployed fno (Typer exit 2), and a report (never silence)
on any other failed read. The real-pipeline runs drive the LOCAL source
through a stub `fno` on PATH, so a silent hook and a hook that never ran
stay distinguishable: the depth-3 run must print the literal count.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).parents[3]
HOOK = REPO_ROOT / "hooks" / "operator-capture-nudge.sh"


def _fno_wrapper(bin_dir: Path, *, exit_code: int = 0) -> Path:
    """A stub `fno` that either runs the local CLI source or fakes an old one."""
    stub = bin_dir / "fno"
    if exit_code:
        stub.write_text(f"#!/usr/bin/env bash\nexit {exit_code}\n")
    else:
        python = sys.executable
        src = REPO_ROOT / "cli" / "src"
        stub.write_text(
            "#!/usr/bin/env bash\n"
            f'exec "{python}" -c "import sys; sys.path.insert(0, {str(src)!r}); '
            'from fno.cli import app; app()" "$@"\n'
        )
    stub.chmod(0o755)
    return stub


def _user_row(text: str, uuid: str, ts: str) -> dict:
    return {
        "type": "user",
        "uuid": uuid,
        "timestamp": ts,
        "message": {"role": "user", "content": text},
    }


def _run_hook(tmp_path: Path, *, env_extra: dict[str, str], bin_dir: Path) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '')}"
    env.update(env_extra)
    return subprocess.run(
        ["bash", str(HOOK)],
        env=env,
        cwd=str(tmp_path),
        capture_output=True,
        text=True,
        timeout=60,
    )


def _queue_env(tmp_path: Path, rows: list[dict]) -> dict[str, str]:
    transcript = tmp_path / "transcript.jsonl"
    transcript.write_text(
        "\n".join(json.dumps(r) for r in rows) + "\n", encoding="utf-8"
    )
    return {
        "FNO_OPERATOR_SESSION_ID": "s-hook-test",
        "FNO_OPERATOR_TRANSCRIPT": str(transcript),
        "FNO_OPERATOR_CAPTURE_DIR": str(tmp_path / "operator-capture"),
    }


def test_silent_at_depth_zero(tmp_path):
    """AC: depth 0 writes nothing to stdout and exits 0."""
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    _fno_wrapper(bin_dir)
    env = _queue_env(
        tmp_path,
        [_user_row("<fno_mail from=\"peer\">mail never queues</fno_mail>", "u-1", "2026-09-06T21:00:00.000Z")],
    )
    result = _run_hook(tmp_path, env_extra=env, bin_dir=bin_dir)
    assert result.returncode == 0, result.stderr
    assert result.stdout == ""


def test_speaks_with_literal_count_at_depth_three(tmp_path):
    """AC + positive control: three prose turns -> the literal 3 and the oldest excerpt in stdout."""
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    _fno_wrapper(bin_dir)
    env = _queue_env(
        tmp_path,
        [
            _user_row("oldest operator ask", "u-1", "2026-09-06T20:00:00.000Z"),
            _user_row("second operator ask", "u-2", "2026-09-06T21:00:00.000Z"),
            _user_row("/fno:review medium", "u-cmd", "2026-09-06T21:30:00.000Z"),
            _user_row("third operator ask", "u-3", "2026-09-06T22:00:00.000Z"),
        ],
    )
    result = _run_hook(tmp_path, env_extra=env, bin_dir=bin_dir)
    assert result.returncode == 0, result.stderr
    assert ": 3 undispositioned" in result.stdout
    assert "oldest operator ask" in result.stdout


def test_silent_when_deployed_fno_lacks_the_verb(tmp_path):
    """AC: exit 2 (Typer no-such-command) -> silent, exit 0, no perpetual nag."""
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    _fno_wrapper(bin_dir, exit_code=2)
    result = _run_hook(tmp_path, env_extra={}, bin_dir=bin_dir)
    assert result.returncode == 0, result.stderr
    assert result.stdout == ""


def test_other_failed_read_is_reported_never_silent(tmp_path):
    """Any non-2 failure prints a report: a failed read must not read as an empty queue."""
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    _fno_wrapper(bin_dir, exit_code=1)
    result = _run_hook(tmp_path, env_extra={}, bin_dir=bin_dir)
    assert result.returncode == 0, result.stderr
    assert "could not be read" in result.stdout
