"""Live integration smoke against the real `codex` CLI (CODEX_SMOKE=1).

Verifies that the JSONL event-type strings the running codex actually
emits match the constants pinned in providers/codex.py's _EVENT_TYPES
dict. This is the discriminator for Locked Decision 14 (warn-on-drift):
if codex's vocabulary moves between versions, this test fails loudly
rather than the parser silently going session_id=None.

Gated by CODEX_SMOKE=1 so CI hosts without codex installed skip it.
"""
from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

import pytest

from fno.agents.providers import codex as codex_mod


pytestmark = pytest.mark.skipif(
    os.environ.get("CODEX_SMOKE", "0") != "1" or shutil.which("codex") is None,
    reason="set CODEX_SMOKE=1 and ensure codex is on PATH to run live smoke",
)


def _run_capture_script() -> dict[str, set[str]]:
    """Run the smoke capture script and return the distinct event types it saw.

    The script lives at ``cli/scripts/smoke/capture-codex-jsonl.sh`` and
    prints distinct type names to stderr. We parse them out and verify
    against the parser's pinned constants.
    """
    script_path = Path(__file__).parent.parent.parent / "scripts" / "smoke" / "capture-codex-jsonl.sh"
    if not script_path.exists():
        pytest.skip(f"smoke script missing: {script_path}")

    env = os.environ.copy()
    env["CODEX_SMOKE"] = "1"
    proc = subprocess.run(
        ["bash", str(script_path)],
        env=env,
        capture_output=True,
        text=True,
        timeout=120.0,
    )
    if proc.returncode != 0:
        pytest.fail(f"smoke script failed rc={proc.returncode}: {proc.stderr}")

    types_seen: set[str] = set()
    item_types_seen: set[str] = set()
    section = None
    for line in proc.stderr.splitlines():
        stripped = line.strip()
        if "distinct event types seen" in line:
            section = "event"
            continue
        if "distinct item.type values" in line:
            section = "item"
            continue
        if section == "event" and stripped and "." in stripped:
            types_seen.add(stripped)
        elif section == "item" and stripped and not stripped.startswith("codex-"):
            item_types_seen.add(stripped)

    return {"event": types_seen, "item": item_types_seen}


def test_smoke_event_types_match_pinned_constants():
    """The live codex CLI's event vocabulary must contain the pinned strings.

    Drift detection: if a future codex release renames `thread.started` to
    `session.created` (or similar), this test fails with the actual vs
    expected set diff, and the parser update is a documented change rather
    than a silent regression.
    """
    seen = _run_capture_script()
    expected_event_types = set(codex_mod._EVENT_TYPES.values())
    expected_item_types = set(codex_mod._ITEM_TYPES.values())

    # All pinned event types MUST appear in the live capture.
    missing_event = expected_event_types - seen["event"]
    # Item-level: only "message" is load-bearing; "error" is best-effort.
    missing_item = {codex_mod._ITEM_TYPES["message"]} - seen["item"]

    assert not missing_event, (
        f"pinned _EVENT_TYPES not seen in live codex output: missing={missing_event}; "
        f"saw={seen['event']}"
    )
    assert not missing_item, (
        f"pinned _ITEM_TYPES['message'] not seen in live codex output: "
        f"missing={missing_item}; saw={seen['item']}"
    )


def test_smoke_create_with_from_name_succeeds():
    """Smoke check: create succeeds end-to-end with a real codex shellout.

    Verifies the substrate works against the live CLI:
      - argv is accepted by codex (no flag-shape drift since the design)
      - thread.started event fires (session_id captured)
      - turn.completed event fires (parser breaks the read loop)
      - exit_code == 0

    Open Question 2 (from spec): the [from: SMOKE_MARKER] bracket prefix
    is in the prompt argv but codex's JSONL events describe the model's
    RESPONSE, not the input — codex does not tee the prompt itself into
    output.jsonl. Whether the model attends to the prefix is an
    empirical question this test does NOT gate on; if the model ignores
    bracket prefixes in a future release, the fallback (AGENTS.md
    injection pattern) is documented in the spec for a follow-up cycle.
    """
    import tempfile

    with tempfile.TemporaryDirectory() as td:
        out_file = Path(td) / "output.jsonl"
        try:
            result = codex_mod.create(
                cwd=Path("/tmp"),
                prompt="reply with the word 'ok'",
                from_name="SMOKE_MARKER_XYZ",
                yolo=False,
                output_path=out_file,
                timeout=60.0,
            )
        except codex_mod.NoSessionIdError:
            pytest.fail("codex did not emit session id during smoke")
        except codex_mod.CodexTimeoutError:
            pytest.skip("codex took >60s; smoke inconclusive")

        assert result.exit_code == 0
        assert result.session_id is not None
        # The tee captured the JSONL stream.
        assert out_file.exists()
        assert "thread.started" in out_file.read_text(encoding="utf-8")
