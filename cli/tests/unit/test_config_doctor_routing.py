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


def test_undeclared_inventory_names_the_gap(monkeypatch):
    _pin_inventory(monkeypatch, declared=False)
    _pin_roles(monkeypatch, {})
    out = _capture(monkeypatch)

    _report_band_routing()

    text = "\n".join(out)
    assert "band routing inactive:" in text
    assert "routing.models" in text


def test_declared_inventory_prints_nothing(monkeypatch):
    _pin_inventory(monkeypatch, declared=True)
    _pin_roles(monkeypatch, {})
    out = _capture(monkeypatch)

    _report_band_routing()

    assert not [line for line in out if "band routing inactive" in line]


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
