"""Migration for the retired `agents.spawn_permission_mode` key (x-7198); split
out of `config/__init__.py` to stay inside its shrink-only file budget."""

from __future__ import annotations


def accept_legacy_spawn_permission_mode(data: object) -> object:
    """Migrate `agents.spawn_permission_mode` onto `defaults.permission_mode`
    (x-7198): copy it over when unset there, keep the surviving value and warn
    when both are set, and always drop the legacy field afterward.
    """
    if not isinstance(data, dict) or "spawn_permission_mode" not in data:
        return data
    from fno.config import _warn_legacy_once

    legacy_val = data["spawn_permission_mode"]
    defaults_raw = data.get("defaults")
    defaults_dict = dict(defaults_raw) if isinstance(defaults_raw, dict) else {}
    modern_val = defaults_dict.get("permission_mode")
    if modern_val:
        _warn_legacy_once(
            "agents.spawn_permission_mode",
            "fno config: agents.spawn_permission_mode is retired; "
            f"agents.defaults.permission_mode is already set to "
            f"{modern_val!r} and wins, the legacy value {legacy_val!r} "
            "is dropped",
        )
    elif legacy_val == "":
        # The retired field defaulted to "bypassPermissions", so an operator
        # who explicitly wrote "" was opting OUT of auto-approval on
        # autonomous dispatch. The surviving field's "" means unset, not
        # opt-out, so that opt-out is lost here - a verb-seeded spawn with no
        # other config now resolves the built-in instead. Loud on purpose:
        # this is a real behavior change, not a cosmetic rename.
        defaults_dict["permission_mode"] = legacy_val
        _warn_legacy_once(
            "agents.spawn_permission_mode",
            "fno config: agents.spawn_permission_mode was explicitly set to "
            '"" (the old opt-out from auto-approval on autonomous dispatch); '
            "agents.defaults.permission_mode has no equivalent opt-out - an "
            "unset value there resolves the built-in bypassPermissions for "
            "any verb-seeded spawn. Set agents.defaults.permission_mode to "
            '"default" to keep normal prompting.',
        )
    else:
        defaults_dict["permission_mode"] = legacy_val
        _warn_legacy_once(
            "agents.spawn_permission_mode",
            "fno config: agents.spawn_permission_mode is renamed "
            "agents.defaults.permission_mode; the legacy spelling still "
            "parses (x-7198)",
        )
    data = {k: v for k, v in data.items() if k != "spawn_permission_mode"}
    data["defaults"] = defaults_dict
    return data
