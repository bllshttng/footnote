from __future__ import annotations

import os
import subprocess
from pathlib import Path


def test_keyless_dispatch_to_terminal_reports_effect_sequence() -> None:
    repo_root = Path(__file__).parents[3]
    env = os.environ.copy()
    for key in tuple(env):
        if key.startswith("FNO_"):
            env.pop(key, None)
    for key in (
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "AGY_API_KEY",
        "CODEX_API_KEY",
    ):
        env.pop(key, None)

    result = subprocess.run(
        ["python3", "scripts/evals/keyless_smoke.py"],
        cwd=repo_root,
        env=env,
        capture_output=True,
        text=True,
        timeout=90,
    )

    assert result.returncode == 0, result.stdout + result.stderr
    assert "keyless smoke: PASS" in result.stdout
    assert "dispatch -> claims -> registry -> terminal -> receipts" in result.stdout
