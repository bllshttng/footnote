"""`fno config doctor` names the band-routing gap (x-374b, task 1.2).

`config.routing.models` is config-first on purpose: declare no rows and
`resolve_grid` records `grid=no-inventory-declared` and picks nothing, so every
difficulty band lands on the ambient default. Nothing said so. Measured
2026-09-02: two joiners spawned on the most expensive lane for medium-band work,
bands computed correctly and never consulted.

Every test asserts a POSITIVE marker - the advisory line, or a row that makes it
go away - never a bare absence.
"""

from __future__ import annotations

import pytest
import typer

from fno.rust_binary import find_dev_binary

requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)


from fno.config_cli import _report_band_routing


def _capture(monkeypatch: pytest.MonkeyPatch) -> list[str]:
    out: list[str] = []
    monkeypatch.setattr(typer, "echo", lambda m="", **k: out.append(str(m)))
    return out


def _pin_inventory(monkeypatch: pytest.MonkeyPatch, *, declared: bool):
    """Pin `resolve_inventory` to a declared / undeclared answer."""
    from fno import route_resolve

    inv = route_resolve.resolve_inventory()
    monkeypatch.setattr(
        route_resolve,
        "resolve_inventory",
        lambda **kw: route_resolve.Inventory(rows=inv.rows, declared=declared),
    )


def _pin_roles(monkeypatch: pytest.MonkeyPatch, roles: dict):
    from types import SimpleNamespace

    monkeypatch.setattr(
        "fno.config.load_settings",
        lambda: SimpleNamespace(model_routing=SimpleNamespace(roles=roles)),
    )


def _pin_settings(
    monkeypatch: pytest.MonkeyPatch,
    *,
    lanes: dict | None = None,
    rows: list | None = None,
    roles: dict | None = None,
):
    """Settings with declared rows and per-verb lanes for the slot readout."""
    from types import SimpleNamespace

    profiles = {
        verb: SimpleNamespace(lanes=verb_lanes, on_exhausted="refuse")
        for verb, verb_lanes in (lanes or {}).items()
    }
    settings = SimpleNamespace(
        routing=SimpleNamespace(models=rows or []),
        agents=SimpleNamespace(profiles=profiles),
        model_routing=SimpleNamespace(roles=roles or {}),
    )
    monkeypatch.setattr("fno.config.load_settings", lambda: settings)
    return settings


def test_undeclared_inventory_names_the_gap(monkeypatch):
    _pin_inventory(monkeypatch, declared=False)
    _pin_roles(monkeypatch, {})
    out = _capture(monkeypatch)

    _report_band_routing()

    text = "\n".join(out)
    assert "band routing inactive:" in text
    assert "routing.models" in text


@requires_rust
def test_declared_inventory_with_a_resolving_lane_prints_nothing(monkeypatch):
    """AC4 silence needs an ARMED slot: a declared row PLUS a verb lane that
    resolves it. A declared inventory with no lanes still says so."""
    _pin_inventory(monkeypatch, declared=True)
    _pin_settings(
        monkeypatch,
        lanes={"target": ["row-x"]},
        rows=[{"name": "row-x", "harness": "claude", "model": "m-1"}],
    )
    out = _capture(monkeypatch)

    _report_band_routing()

    assert not [line for line in out if "band routing inactive" in line]


@requires_rust
def test_doctor_names_verbs_with_empty_slots(monkeypatch):
    """AC4-EDGE: the line names BOTH halves - the declared inventory count and
    every dispatched verb whose slot has no lane."""
    _pin_inventory(monkeypatch, declared=False)
    _pin_roles(monkeypatch, {})
    out = _capture(monkeypatch)

    _report_band_routing()

    text = "\n".join(out)
    assert "band routing inactive:" in text
    assert "declares 0 row(s)" in text
    for verb in ("think", "blueprint", "target", "review", "crown"):
        assert verb in text


def test_roles_set_alongside_an_empty_inventory_says_they_are_a_different_axis(
    monkeypatch,
):
    """The trap this line exists for: roles ARE configured, so the operator
    reasonably reads routing as on. Roles route by role, never by band."""
    _pin_inventory(monkeypatch, declared=False)
    _pin_roles(monkeypatch, {"tidy": "zai/glm-4.7"})
    out = _capture(monkeypatch)

    _report_band_routing()

    text = "\n".join(out)
    assert "band routing inactive:" in text
    assert "model_routing.roles" in text and "ROLE" in text


def test_unreadable_roles_still_prints_the_line(monkeypatch):
    """The roles note is a hint on top of the advisory, so a failed settings
    read must not cost the operator the line the advisory exists to print."""
    _pin_inventory(monkeypatch, declared=False)

    def boom():
        raise RuntimeError("unreadable")

    monkeypatch.setattr("fno.config.load_settings", boom)
    out = _capture(monkeypatch)

    _report_band_routing()

    text = "\n".join(out)
    assert "band routing inactive:" in text
    assert "model_routing.roles" not in text


# ---------------------------------------------------------------------------
# Harness overlay readout (x-8975)
# ---------------------------------------------------------------------------


def _pin_overlay_settings(
    monkeypatch: pytest.MonkeyPatch,
    *,
    profiles: dict | None = None,
    defaults: dict | None = None,
):
    """Settings whose agents block carries raw harness overlays."""
    from types import SimpleNamespace

    agents = SimpleNamespace(
        defaults=SimpleNamespace(
            permission_mode=defaults.get("permission_mode", "") if defaults else "",
            effort=defaults.get("effort", "") if defaults else "",
            harness=defaults.get("harness", {}) if defaults else {},
        ),
        profiles={
            verb: SimpleNamespace(
                permission_mode=cfg.get("permission_mode", ""),
                effort=cfg.get("effort", ""),
                harness=cfg.get("harness", {}),
            )
            for verb, cfg in (profiles or {}).items()
        },
    )
    monkeypatch.setattr(
        "fno.config.load_settings", lambda: SimpleNamespace(agents=agents)
    )


def test_doctor_names_a_codex_scalar_that_cannot_serve_claude(monkeypatch):
    """AC3-HP: the live-shaped defect - a codex spelling on the target profile
    - gets one line naming the verb, the harness, the value, and the overlay
    path that fixes it."""
    from fno.config_cli import _report_harness_overlays

    _pin_overlay_settings(
        monkeypatch,
        profiles={"target": {"permission_mode": "yolo", "effort": "high"}},
    )
    out = _capture(monkeypatch)

    _report_harness_overlays()

    text = "\n".join(out)
    assert "agents.profiles.target.permission_mode" in text
    assert "'yolo'" in text and "claude" in text
    assert "[agents.profiles.target.harness.claude]" in text


def test_doctor_names_an_effort_value_with_no_surface(monkeypatch):
    """A profile effort on gemini (no reasoning-effort surface) is named with
    the mapper's own reason and the overlay fix path."""
    from fno.config_cli import _report_harness_overlays

    _pin_overlay_settings(
        monkeypatch,
        profiles={"target": {"effort": "high"}},
    )
    out = _capture(monkeypatch)

    _report_harness_overlays()

    text = "\n".join(out)
    assert "config.agents.profiles.target.effort = 'high'" in text
    assert "gemini" in text
    assert "[agents.profiles.target.harness.gemini]" in text


def test_doctor_silent_when_every_pair_maps(monkeypatch):
    """AC3-EDGE: with the scalars empty and each harness's answer under its
    own overlay, every (verb, harness) pair maps and the readout prints
    nothing. One scalar CANNOT be silent (no value maps on every harness),
    which is exactly why the overlay exists."""
    from fno.config_cli import _report_harness_overlays

    _pin_overlay_settings(
        monkeypatch,
        profiles={
            "target": {
                "permission_mode": "",
                "harness": {
                    "claude": {"permission_mode": "bypassPermissions"},
                    "codex": {"permission_mode": "yolo", "effort": "xhigh"},
                },
            },
        },
    )
    out = _capture(monkeypatch)

    _report_harness_overlays()

    assert out == []


def test_doctor_accepts_every_claude_help_value(monkeypatch):
    """claude --help lists six permission modes; each must read as a claude
    answer and print nothing, including the two easy to forget."""
    from fno.config_cli import _report_harness_overlays
    from fno.agents.harness_map import CLAUDE_PERMISSION_MODES

    assert CLAUDE_PERMISSION_MODES == {
        "default", "acceptEdits", "auto", "dontAsk", "plan", "bypassPermissions",
    }
    _pin_overlay_settings(
        monkeypatch,
        profiles={"target": {"permission_mode": "dontAsk"}},
    )
    out = _capture(monkeypatch)

    _report_harness_overlays()

    claude_lines = [line for line in out if "claude" in line and "permission_mode" in line]
    assert claude_lines == []
