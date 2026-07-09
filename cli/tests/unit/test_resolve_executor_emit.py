"""resolve-executor.sh executor_resolved telemetry (x-64cb US1).

The emit is best-effort: routing (the stdout value) must be byte-identical
whether or not the event emits (AC1-HP), and a missing `fno` must not break
resolution (AC5-ERR). A stub `fno` on PATH captures the emit payload so the
test asserts tier + warn_fallback without touching real events.jsonl.
"""
from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
SCRIPT = REPO_ROOT / "skills/do/scripts/resolve-executor.sh"


@pytest.fixture()
def stub_fno(tmp_path):
    """A fake `fno` on PATH that appends the emit `-d` payload to a log."""
    bindir = tmp_path / "bin"
    bindir.mkdir()
    log = tmp_path / "emit.log"
    (bindir / "fno").write_text(
        "#!/usr/bin/env bash\n"
        'if [[ "$1" == "event" && "$2" == "emit" ]]; then\n'
        '  while [[ $# -gt 0 ]]; do [[ "$1" == "-d" ]] && printf "%s\\n" "$2" >> "$FNO_EMIT_LOG"; shift; done\n'
        "fi\nexit 0\n"
    )
    (bindir / "fno").chmod(0o755)
    env = dict(os.environ)
    env["PATH"] = f"{bindir}:{env['PATH']}"
    env["FNO_EMIT_LOG"] = str(log)
    return env, log


def _run(env, extra):
    e = dict(env)
    e.update(extra)
    return subprocess.run(
        ["bash", str(SCRIPT)], capture_output=True, text=True, env=e
    )


def test_inference_tier_emits_and_stdout_is_only_the_value(stub_fno):
    env, log = stub_fno
    r = _run(env, {"TASK_ID": "1.2", "PLAN_PATH": "p.md", "TASK_FILES": "src/App.tsx"})
    assert r.stdout.strip() == "impeccable"  # AC1-HP: contract unchanged
    payload = log.read_text().strip()
    assert '"tier":"surface-inference"' in payload
    assert '"warn_fallback":false' in payload
    assert '"task":"1.2"' in payload


def test_default_tier(stub_fno):
    env, log = stub_fno
    r = _run(env, {})
    assert r.stdout.strip() == "do"
    assert '"tier":"default"' in log.read_text()


def test_warn_fallback_flagged(stub_fno):
    env, log = stub_fno
    r = _run(env, {"TASK_EXEC": "bogus"})
    assert r.stdout.strip() == "do"  # fail-closed
    payload = log.read_text()
    assert '"warn_fallback":true' in payload
    assert '"tier":"task-block"' in payload  # tier that produced the bad value


def test_fno_unavailable_never_breaks_routing(tmp_path):
    # AC5-ERR: no fno on PATH -> resolved value + a single stderr note, exit 0.
    env = dict(os.environ)
    env["PATH"] = "/usr/bin:/bin"
    r = _run(env, {"TASK_EXEC": "impeccable"})
    assert r.returncode == 0
    assert r.stdout.strip() == "impeccable"
    assert "skipped executor_resolved emit" in r.stderr
