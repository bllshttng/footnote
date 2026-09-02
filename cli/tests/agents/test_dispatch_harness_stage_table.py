"""One home for the autonomous-dispatch harness axis.

``agents.profiles.<verb>.provider`` (the stage table) is the home; the
deprecated ``dispatch.harness`` reads as the fallback rung beneath it for one
release. Both dispatch doors - ``resolve_dispatch`` (dispatch-node.sh /
``fno agents dispatch resolve``) and ``fno agents spawn``
(inject_spawn_defaults) - must answer one harness for one node.
"""
from __future__ import annotations

from typing import Optional

from fno.agents.harness_map import resolve_dispatch
from fno.dispatch_flags import configured_dispatch_harness


class _Defaults:
    def __init__(self, provider="", model="", effort="", substrate="", permission_mode="",
                 route="", account="", pane_group="", lanes=None):
        self.provider = provider
        self.model = model
        self.effort = effort
        self.substrate = substrate
        self.permission_mode = permission_mode
        self.route = route
        self.account = account
        self.pane_group = pane_group
        self.lanes = [
            _Defaults(**lane) if isinstance(lane, dict) else lane
            for lane in (lanes or [])
        ]


class _Settings:
    def __init__(self, profiles=None, dispatch=None, **kw):
        prof = {k: _Defaults(**v) for k, v in (profiles or {}).items()}
        self.agents = type(
            "A",
            (),
            {
                "defaults": _Defaults(**kw),
                "profiles": prof,
                "max_lanes": {},
            },
        )()
        self.dispatch = dispatch


def _settings(
    profile_provider: Optional[str] = None,
    legacy_harness: Optional[str] = None,
    verb: str = "target",
) -> _Settings:
    from types import SimpleNamespace

    profiles = {verb: {"provider": profile_provider}} if profile_provider else None
    # Attribute access, like the typed DispatchBlock the real model carries.
    dispatch = (
        SimpleNamespace(harness=legacy_harness, substrate="", command="")
        if legacy_harness
        else None
    )
    return _Settings(profiles=profiles, dispatch=dispatch)


def test_resolver_reads_the_stage_table_first() -> None:
    harness, note = configured_dispatch_harness(
        _settings(profile_provider="codex", legacy_harness=None)
    )
    assert harness == "codex"
    assert note is None


def test_resolver_falls_back_to_the_legacy_key() -> None:
    harness, note = configured_dispatch_harness(
        _settings(profile_provider=None, legacy_harness="codex")
    )
    assert harness == "codex"
    assert note is None


def test_resolver_names_the_losing_spelling_when_keys_disagree() -> None:
    harness, note = configured_dispatch_harness(
        _settings(profile_provider="codex", legacy_harness="claude")
    )
    assert harness == "codex"
    assert note is not None
    assert "dispatch.harness" in note
    assert "claude" in note
    assert "codex" in note


def test_resolver_unset_everywhere_answers_none() -> None:
    assert configured_dispatch_harness(_settings()) == (None, None)


def test_resolver_verb_selects_the_profile() -> None:
    harness, _ = configured_dispatch_harness(
        _settings(profile_provider="opencode", verb="think"), verb="/fno:think"
    )
    assert harness == "opencode"


def test_dispatch_door_resolves_the_stage_table() -> None:
    out = resolve_dispatch(settings=_settings(profile_provider="codex"))
    assert out["harness"] == "codex"


def test_dispatch_door_names_the_losing_spelling_in_its_decision() -> None:
    out = resolve_dispatch(
        settings=_settings(profile_provider="codex", legacy_harness="claude")
    )
    assert out["harness"] == "codex"
    assert any("dispatch.harness" in entry and "claude" in entry for entry in out["decision"])


def test_dispatch_door_keeps_the_legacy_key_alone() -> None:
    """A legacy-only installation is unchanged: the key still wins its rung,
    and nothing is named as shadowed (nothing was)."""
    out = resolve_dispatch(settings=_settings(legacy_harness="codex"))
    assert out["harness"] == "codex"
    assert not any("ignored" in entry for entry in out["decision"])


def test_spawn_door_answers_the_same_harness() -> None:
    """The measured failure this file pins: one node through both doors must
    not land on two vendors. The spawn door reads the same stage-table field
    its own merge already owned; the dispatch door now reads it too."""
    from fno.agents.spawn_defaults import inject_spawn_defaults

    stub = _settings(profile_provider="codex", legacy_harness="claude")
    argv = inject_spawn_defaults(
        ["spawn", "/fno:target x-1"], settings=stub, env={}
    )
    assert "--harness" in argv
    assert argv[argv.index("--harness") + 1] == "codex"
