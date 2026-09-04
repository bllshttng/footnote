"""Resolved-row parity between the two capability readers (x-244c, AC2-ERR).

The x-244c collapse left ONE table file but TWO readers of it: the Rust one
(crates/fno/src/agents_view.rs, the pane/attach surface) and the Python one
(fno.agents.harness_map, the dispatch surface). Byte parity of the bundled
file cannot see a merge the two implement differently, so this guard stages
one config chain covering every field kind, resolves it through the Python
reader HERE, and re-resolves it through the Rust reader inside one cargo
test that fails naming the harness and field.

Two asymmetries are deliberate and not staged here. A NEW name with no bundled
row is the x-296f teach path on the Rust side (the pane lane lands on any
parsable form block), while the Python dispatch reader stays roster-gated.
Dispatch needs a measured row; a pane does not. And a nested override whose
keys are all bundled but whose VALUES fail the contract (an incomplete
strategy table, say) lands in the Rust reader and is refused by Python: the
crates publish independently, so the Rust gate is the bundled key vocabulary,
not the semantic validator. The divergence is inert because the mux reads
lane forms from these rows; the accepted class is what this test pins.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
from copy import deepcopy
from pathlib import Path

import pytest

from fno.agents import harness_map

# One override per field kind: a nested strategy table, scalars, a list, a
# shallow lane alias, plus one refusal (unknown roster name) whose ABSENCE
# both readers must agree on.
STAGED_OVERRIDES = """
[harness.agy.model_switch_strategy]
kind = "direct"
tokens = ["--model {model}", "--effort {effort}"]
effort_labels = {}
status_command = "/status"
status_pattern = 'Model:\\s+(?P<model>\\S+).*?effort\\s+(?P<effort>low|medium|high)'

[harness.agy]
send_keys_enter_delay_ms = 400
permission_bypass = ["--yolo"]

[harness.pi.attach]
kind = "session_flag"
tokens = ["pi", "--session", "{session_id}"]

[harness.codex]
route_on_pane = true

[harness.brandnewharness]
unheard_of_field = true
"""


@pytest.fixture
def bundled_state():
    rows = deepcopy(harness_map._HARNESS_CAPS)
    warnings = list(harness_map.OVERRIDE_WARNINGS)
    yield
    harness_map._HARNESS_CAPS.clear()
    harness_map._HARNESS_CAPS.update(rows)
    harness_map.OVERRIDE_WARNINGS[:] = warnings


def test_resolved_rows_match_between_readers(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, bundled_state
) -> None:
    if shutil.which("cargo") is None:
        pytest.skip("no cargo toolchain: the Rust half of the parity check cannot run")
    stage = tmp_path / "stage"
    (stage / ".fno").mkdir(parents=True)
    (stage / ".fno" / "config.toml").write_text(STAGED_OVERRIDES, encoding="utf-8")
    monkeypatch.chdir(stage)
    monkeypatch.delenv("FNO_GLOBAL_SETTINGS_PATH", raising=False)
    monkeypatch.setenv("HOME", str(stage))
    harness_map.reload_capability_overrides()
    expected = {name: dict(row) for name, row in harness_map._HARNESS_CAPS.items()}
    expected_path = tmp_path / "expected.json"
    expected_path.write_text(json.dumps(expected), encoding="utf-8")

    repo_root = Path(__file__).resolve().parents[3]
    env = {
        **os.environ,
        "FNO_CAPABILITY_PARITY_DIR": str(stage),
        "FNO_CAPABILITY_PARITY_JSON": str(expected_path),
    }
    result = subprocess.run(
        [
            "cargo",
            "test",
            "--manifest-path",
            str(repo_root / "crates" / "fno" / "Cargo.toml"),
            "--",
            "--exact",
            "--ignored",
            "agents_view::tests::capability_parity_with_python",
        ],
        env=env,
        cwd=repo_root,
        capture_output=True,
        text=True,
        timeout=900,
    )
    output = result.stdout + result.stderr
    assert result.returncode == 0, (
        "the Rust reader resolved the staged config differently:\n"
        + output[-4000:]
    )
    # The refusal the stage plants is a POSITIVE marker, not a silent pass:
    # both readers must have declined it and the Python side must say so.
    assert any("brandnewharness" in w for w in harness_map.OVERRIDE_WARNINGS)
