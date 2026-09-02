"""The capability probe's verdict contract (x-244c, AC3-AC6).

Four verdicts, and UNKNOWN never acts: a missing binary, a timeout, or a
missing behavioral instrument is UNKNOWN with its reason, never a
disagreement, because an absent instrument is not a measurement (AC4-ERR).
Every instrument here is faked at the seam (the authority runner, the
vendor-store instrument) so the suite spawns nothing and needs no harness
binary.
"""

from __future__ import annotations

from copy import deepcopy
from pathlib import Path

import pytest

from fno.agents import capability_probe, harness_map

AGY_HELP = (
    "agy 1.1.24\n"
    "  --conversation   Resume a previous conversation by ID\n"
    "  --effort         Reasoning effort for the current CLI session (low|medium|high)\n"
    "  --model          Model for the current CLI session\n"
)

NO_EFFORT_HELP = (
    "somebin 1.0.0\n  --conversation   Resume a previous conversation by ID\n"
)


@pytest.fixture
def bundled_state():
    rows = deepcopy(harness_map._HARNESS_CAPS)
    warnings = list(harness_map.OVERRIDE_WARNINGS)
    yield
    harness_map._HARNESS_CAPS.clear()
    harness_map._HARNESS_CAPS.update(rows)
    harness_map.OVERRIDE_WARNINGS[:] = warnings


@pytest.fixture
def fake_authority(monkeypatch):
    """Fake the authority read at the seam; record every argv it ran."""
    calls: list[list[str]] = []

    def install(output: str, code: int = 0):
        def run(command, cwd=None):
            calls.append(list(command))
            return code, output

        monkeypatch.setattr(capability_probe, "_run_authority", run)
        monkeypatch.setattr(capability_probe.shutil, "which", lambda p: "/fake/bin")
        return calls

    return install


def _report(harness: str = "agy", **kwargs) -> dict:
    return capability_probe.probe_harness(harness, **kwargs)


def _field(report: dict, name: str) -> dict:
    return next(f for f in report["fields"] if f["field"] == name)


def test_disagrees_quotes_the_authority_line(bundled_state, fake_authority) -> None:
    """AC4-HP: the bundled agy row understates the CLI; the probe says so and
    quotes the line that settles it."""
    calls = fake_authority(AGY_HELP)

    report = _report()

    field = _field(report, "model_switch_strategy")
    assert field["verdict"] == "DISAGREES"
    assert "unsupported" in field["detail"]
    assert field["evidence"].startswith("--effort")
    # AC5-EDGE: the probe read the authority command and ran NOTHING else -
    # no deliberately-invalid value, no vocabulary probe by rejection.
    assert calls == [["agy", "--help"]], calls


def test_agrees_when_row_matches_the_declaration(bundled_state, fake_authority) -> None:
    fake_authority(NO_EFFORT_HELP)

    report = _report()

    assert _field(report, "model_switch_strategy")["verdict"] == "AGREES"


def test_agrees_when_a_config_row_lands(bundled_state, fake_authority, monkeypatch, tmp_path: Path) -> None:
    """AC6-HP first half: once the corrected row is in config, the same probe
    run reports agreement - no code change in between."""
    fake_authority(AGY_HELP)
    (tmp_path / ".fno").mkdir(parents=True)
    (tmp_path / ".fno" / "config.toml").write_text(
        "[harness.agy.model_switch_strategy]\n"
        'kind = "direct"\n'
        'tokens = ["--model {model}", "--effort {effort}"]\n'
        "effort_labels = {}\n"
        'status_command = "/status"\n'
        "status_pattern = 'Model:\\\\s+(?P<model>\\\\S+).*?effort\\\\s+(?P<effort>low|medium|high)'\n",
        encoding="utf-8",
    )
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("FNO_GLOBAL_SETTINGS_PATH", raising=False)
    monkeypatch.setenv("HOME", str(tmp_path))
    harness_map.reload_capability_overrides()

    report = _report()

    assert _field(report, "model_switch_strategy")["verdict"] == "AGREES"


def test_missing_binary_is_unknown_never_disagrees(bundled_state, monkeypatch) -> None:
    """AC4-ERR: an absent instrument is not a measurement."""
    monkeypatch.setattr(capability_probe.shutil, "which", lambda p: None)

    report = _report()

    field = _field(report, "model_switch_strategy")
    assert field["verdict"] == "UNKNOWN"
    assert "not on PATH" in field["detail"]


def test_authority_failure_is_unknown(bundled_state, fake_authority) -> None:
    fake_authority("", code=124)

    field = _field(_report(), "model_switch_strategy")

    assert field["verdict"] == "UNKNOWN"
    assert "124" in field["detail"]


def test_unprobeable_field_emits_no_value(bundled_state) -> None:
    """AC3-ERR: the delay was measured over 15 timed trials on a live pane; a
    probe that emitted a number for it would overwrite a measurement with a
    guess."""
    report = _report()

    field = _field(report, "send_keys_enter_delay_ms")
    assert field["verdict"] == "UNPROBEABLE"
    assert "5 delays" in field["detail"]
    assert "evidence" not in field
    assert "value" not in field


def test_declared_field_without_a_rule_is_undeclared(
    bundled_state, fake_authority, monkeypatch
) -> None:
    fake_authority(AGY_HELP)
    decls = capability_probe.probe_declarations()
    decls["brand_new_field"] = {"kind": "declared", "authority": "{bin} --version", "pattern": "x"}
    monkeypatch.setattr(capability_probe, "probe_declarations", lambda: decls)

    report = _report()

    field = _field(report, "brand_new_field")
    assert field["verdict"] == "UNDECLARED"
    assert "refuses to guess" in field["detail"]


def test_behavioral_needs_live_and_never_spawns_read_only(bundled_state) -> None:
    """Read-only by default: a behavioral field spawns nothing."""
    report = _report()

    field = _field(report, "thread")
    assert field["verdict"] == "UNKNOWN"
    assert "--live" in field["detail"]


def test_behavioral_accepts_only_on_a_vendor_marker(
    bundled_state, monkeypatch
) -> None:
    """AC5-HP: the form is accepted only on a marker the vendor's own store
    produced, and the report names the marker it read."""
    state = {"calls": 0, "marker": "session 01a04 present in `codex thread list` only after the run"}

    def fake_instrument(harness: str) -> object:
        def read(harness_: str, field: str) -> str:
            if field != "thread":
                return ""
            state["calls"] += 1
            return state["marker"] if state["calls"] == 1 else ""

        return read

    monkeypatch.setattr(capability_probe, "_store_instrument", fake_instrument)

    report = _report("codex", live=True)

    field = _field(report, "thread")
    assert field["verdict"] == "AGREES"
    assert "vendor-produced marker" in field["detail"]

    state["calls"] = 0
    state["marker"] = ""
    report = _report("codex", live=True)

    field = _field(report, "thread")
    assert field["verdict"] == "UNKNOWN"
    assert "not accepted" in field["detail"]


def test_behavioral_without_a_wired_instrument_is_unknown(bundled_state) -> None:
    report = _report("agy", live=True)

    field = _field(report, "thread")
    assert field["verdict"] == "UNKNOWN"
    assert "instrument" in field["detail"]


def test_write_emits_stanza_with_evidence_and_date(bundled_state, fake_authority) -> None:
    """AC6-HP: --write emits the stanza for each disagreement with the
    evidence line and the measurement date beside it."""
    fake_authority(AGY_HELP)

    report = _report(write=True)

    stanza = report["stanza"]
    assert stanza is not None
    assert "[harness.agy.model_switch_strategy]" in stanza
    assert "--effort" in stanza
    assert "measured" in stanza
    assert "UNMEASURED" in stanza, "the status pair stays honest: it is not derived"


def test_write_without_disagreements_writes_nothing(bundled_state, fake_authority) -> None:
    fake_authority(NO_EFFORT_HELP)

    report = _report(write=True)

    assert report["stanza"] is None
