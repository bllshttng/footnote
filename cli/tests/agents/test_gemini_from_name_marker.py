"""Smoke marker test for [from: <name>] prefix injection (Wave 2.3 AC7-HP).

Gated by GEMINI_SMOKE=1 + @pytest.mark.smoke; excluded from per-PR CI
because it requires the real gemini binary and an authenticated
session. The test prompts gemini to echo its from-name annotation —
if the model's prompt template strips bracket-annotations from
user-visible context, the marker won't appear and the test FAILS
LOUDLY (AC7-ERR), signaling that the design's --from-name story for
gemini needs the no-op-with-WARN fallback.

To run manually:

    cd cli && GEMINI_SMOKE=1 uv run pytest tests/agents/test_gemini_from_name_marker.py -m smoke -v
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

import pytest


smoke = pytest.mark.smoke


def _gemini_on_path() -> bool:
    try:
        subprocess.run(
            ["gemini", "--version"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            check=True, timeout=5,
        )
        return True
    except (FileNotFoundError, subprocess.CalledProcessError, subprocess.TimeoutExpired):
        return False


@smoke
@pytest.mark.skipif(
    os.environ.get("GEMINI_SMOKE", "0") != "1",
    reason="GEMINI_SMOKE=1 not set (real-binary test; excluded from CI)",
)
@pytest.mark.skipif(
    not _gemini_on_path(),
    reason="gemini binary not on PATH",
)
def test_from_name_marker_reaches_model_context(tmp_path: Path) -> None:
    """AC7-HP: the [from: <smoke-marker>] prefix injected via
    inject_from_name reaches gemini's model context — the model can
    see and echo the marker.

    On failure (marker NOT in reply): AC7-ERR fires. This is a LOUD
    failure (assertion raises) NOT a skip — the design lock needs
    flipping to the no-op-with-WARN fallback documented in the spec.
    """
    from fno.agents.providers import gemini as gemini_mod

    marker = "smoke-marker-7c5dcf5d"
    prompt = (
        f"Echo back ONLY the contents of the [from: ...] annotation that "
        f"appears in your prompt. Just the inner value, nothing else."
    )

    result = gemini_mod.create(
        cwd=tmp_path,
        prompt=prompt,
        from_name=marker,
        yolo=False,
        output_path=tmp_path / "from_name_smoke.jsonl",
        timeout=60.0,
    )

    # AC7-HP: the marker MUST appear in the reply.
    assert marker in result.last_msg, (
        f"AC7-ERR: gemini did NOT echo the [from: <name>] prefix. "
        f"This means the bracket-annotation does NOT reach gemini's "
        f"model context. The design lock for --from-name on gemini "
        f"needs flipping to no-op-with-WARN per AC7-ERR. Reply was: "
        f"{result.last_msg!r}"
    )
