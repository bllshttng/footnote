"""Behavioral tests that EXECUTE the shared page-reload script.

The script rides both operator pages (reign.html and the local board) as an
inline ``<script data-fno-reload="N">``. A string assertion proves the tag
shipped; it cannot prove the reload happens only when the reader has walked
away, or that what the reader set comes back after the reload. These run the
real script against a minimal DOM and read the real result.

Node is a declared CI dependency (``.github/workflows/cli-ci.yml`` sets up
Node 22), so a missing interpreter is a FAILURE here, never a skip. The
script path is the build-generated copy the Python package reads;
``cargo build -p fno-agents`` writes it, and the cli-ci generated-copies step
holds it byte-identical to ``crates/fno-agents/src/page_reload.js``.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
from pathlib import Path

HARNESS = Path(__file__).parent / "page_reload_harness.mjs"
SCRIPT = Path(__file__).parents[2] / "src" / "fno" / "graph" / "page_reload.js"


def _run(stage: str, stored: str | None = None) -> dict:
    node = shutil.which("node")
    assert node, (
        "node is missing. It is a declared CI dependency (cli-ci.yml sets up "
        "Node 22); this guard fails rather than skipping, because a skipped "
        "guard proves nothing."
    )
    assert SCRIPT.is_file(), f"{SCRIPT} is missing; build the crate to generate it"
    env = {**os.environ, "STAGE": stage}
    if stored is not None:
        env["STORED"] = stored
    result = subprocess.run(
        [node, str(HARNESS), str(SCRIPT)],
        capture_output=True,
        text=True,
        env=env,
        timeout=60,
    )
    assert result.returncode == 0, f"harness failed:\n{result.stderr}"
    return json.loads(result.stdout)


def test_the_reload_saves_only_what_changed():
    """AC1-HP: one untouched interval reloads, carrying exactly the deltas.

    The search box, the pressed project chip, the expanded row, the collapsed
    group and the scroll position are saved; the chip the reader never touched
    is not.
    """
    out = _run("save")
    assert out["ms"] == 60000, out["ms"]
    assert out["reloaded"] is True
    assert out["stored"]["y"] == 900
    values = out["stored"]["values"]
    assert len(values) == 4, values
    assert "crown" in values.values()
    assert sorted(values.values()).count("true") == 2  # row + project chip
    assert "false" in values.values()  # the collapsed group head


def test_restore_replays_each_changed_control_once():
    """AC2-HP: the saved state comes back, and nothing else is touched.

    An input gets its value plus input and change events, a stateful button
    gets exactly one click, and the key is spent.
    """
    saved = _run("save")["stored"]
    out = _run("restore", stored=json.dumps(saved))
    assert out["value"] == "crown"
    assert out["events"] == ["input", "change"]
    assert out["statusClicks"] == 0, "an unchanged chip must get no click"
    assert out["projectClicks"] == 1
    assert out["headClicks"] == 1
    assert out["rowClicks"] == 1
    assert out["scrollTo"] == [[0, 900]]
    assert out["keyRemains"] is False


def test_a_hidden_tab_does_not_reload():
    """AC1-ERR: a background tab never reloads itself."""
    out = _run("hidden")
    assert out["reloaded"] is False


def test_a_recent_keypress_defers_the_reload():
    """AC1-ERR: input inside the last interval defers the reload."""
    out = _run("touched")
    assert out["reloaded"] is False


def test_zero_registers_no_interval():
    """AC1-ERR: data-fno-reload="0" turns the reload off entirely."""
    out = _run("off")
    assert out["registered"] is False
