"""Reality-check dispatcher driven by gate_reality_map.yaml.

Loads the YAML map, substitutes {state.FIELD} placeholders, then invokes
the appropriate domain check (gh, notion, sheets, ...) for a given gate name.

Unknown gates or gates mapped to 'none' return {ok: true, note: "..."}.
"""
from __future__ import annotations

import re
from pathlib import Path
from typing import Any, Dict, Optional

import yaml

from fno.state.io import read_frontmatter


# -- YAML map --

_MAP_PATH = Path(__file__).parent.parent / "gate_reality_map.yaml"


def load_reality_map(map_path: Optional[Path] = None) -> Dict[str, Any]:
    """Load and parse the gate_reality_map.yaml file.

    Args:
        map_path: Override the default map path.

    Returns:
        Parsed YAML dict with 'gates' key.
    """
    path = Path(map_path) if map_path is not None else _MAP_PATH
    with path.open("r", encoding="utf-8") as fh:
        return yaml.safe_load(fh)


# -- Template substitution --

def _substitute_state(value: Any, state: Dict[str, Any]) -> Any:
    """Replace {state.FIELD} placeholders in a string value with state values."""
    if not isinstance(value, str):
        return value
    def replacer(m: re.Match) -> str:
        field = m.group(1)
        return str(state.get(field, m.group(0)))
    return re.sub(r"\{state\.([^}]+)\}", replacer, value)


def _substitute_args(args: Dict[str, Any], state: Dict[str, Any]) -> Dict[str, Any]:
    """Substitute all {state.FIELD} placeholders in args values."""
    return {k: _substitute_state(v, state) for k, v in args.items()}


# -- Domain invocation --

def _invoke_domain(domain: str, args: Dict[str, Any]) -> Dict[str, Any]:
    """Invoke the given reality-check domain with resolved args.

    This delegates to the domain-specific Python module directly (no subprocess)
    so callers can mock it cleanly in tests.

    Args:
        domain: Domain name (e.g. "gh", "notion", "sheets").
        args: Resolved arguments dict (all {state.FIELD} already substituted).

    Returns:
        JSON-serializable result dict from the domain.
    """
    if domain == "gh":
        from fno.reality_check.gh import check_gh
        pr = args.get("pr")
        expect = args.get("expect", "open")
        timeout = int(args.get("timeout", 5))
        # pr may be a string after template substitution
        if pr is not None:
            try:
                pr = int(pr)
            except (ValueError, TypeError):
                pass
        return check_gh(pr_number=pr, expect=expect, timeout=timeout)

    if domain == "notion":
        from fno.reality_check.notion import check_notion
        return check_notion(**args)

    if domain == "sheets":
        from fno.reality_check.sheets import check_sheets
        return check_sheets(**args)

    return {"ok": False, "error": {"kind": "unknown_domain", "domain": domain}}


# -- Main dispatcher --

def run_reality_check(
    gate: str,
    *,
    state_path: Optional[Path] = None,
    map_path: Optional[Path] = None,
) -> Dict[str, Any]:
    """Run the reality check for a given gate using the YAML map.

    Args:
        gate: Gate name (e.g. "artifact_shipped", "quality_check_passed").
        state_path: Path to target-state.md for {state.FIELD} substitution.
        map_path: Override the default map path.

    Returns:
        JSON-serializable result dict from the domain check, or
        {ok: true, note: "no reality-check defined for <gate>"} if not mapped.
    """
    if state_path is None:
        state_path = Path(".fno/target-state.md")

    # Load state for template substitution
    try:
        state_data, _body = read_frontmatter(Path(state_path))
    except (FileNotFoundError, OSError):
        state_data = {}

    # Load the map
    mapping = load_reality_map(map_path)
    gates = mapping.get("gates", {})

    if gate not in gates:
        return {"ok": True, "note": f"no reality-check defined for {gate}"}

    gate_config = gates[gate]
    domain = gate_config.get("reality_check", "none")

    if not domain or domain == "none":
        return {"ok": True, "note": f"no reality-check defined for {gate}"}

    # Substitute state fields in args
    raw_args = gate_config.get("args", {})
    resolved_args = _substitute_args(raw_args, state_data)

    return _invoke_domain(domain, resolved_args)
