"""Harness-capability map + shared dispatch resolver (US1 / G3).

One versioned table from a *capability* to each harness's concrete value, so
dispatch is provider-neutral instead of claude-shaped by accident. Every
autonomous launcher (dispatch-node.sh, backlog advance, /think handoff, the
active_backlog daemon) resolves argv through :func:`resolve_dispatch` instead of
hand-rolling it - the duplicated-spawn bug class (x-2c27 fixed three of four
copies and missed the fourth) disappears when exactly one resolver owns the
(harness, substrate, command) decision (Locked Decision 10).

The resolver is PURE: config + context -> tuple. It never acquires a claim,
spawns, or touches the network. Claims and spawning stay in the launchers.

Per-environment override: ``config.dispatch`` (harness / substrate / command)
overlays the built-in defaults; the map itself is the versioned in-tree table.

Verified facts (2026-07-13):
- permission_bypass tokens mirror the provider adapters (claude.py,
  codex.py, gemini.py) - the flag a headless/bg worker needs so it never wedges
  on an approval prompt (the concrete cause of the manual-approve pain).
- bg is claude-only (``claude --bg``); every other harness falls back to
  ``headless`` (Locked Decision 3, HARNESSES.md).
- stop_hook is native for all THREE dispatchable harnesses: the autonomous
  target loop's stop-equivalent hook fires under ``claude -p`` (verified),
  ``codex exec`` (CODEX_THREAD_ID), and ``gemini -p`` natively - so NO dispatch
  target needs the ``run-target-loop.sh`` wrapper as its floor (that wrapper
  stays only for the non-dispatch hermes/openclaw drivers). This resolves the
  US1 verification spike (HARNESSES.md lines 22-23).
"""
from __future__ import annotations

from typing import Mapping, Optional

# Bump when a capability KEY is added/removed or a value's meaning changes, so a
# consumer can assert the shape it was written against.
MAP_VERSION = 1

# capability -> per-harness value. Keys are the READABLE_PROVIDERS set; only
# claude/codex/gemini are dispatchable today (agy/opencode are headless-only
# readable rows whose bypass is owned by their own adapter - US4 fills them in).
_HARNESS_CAPS: dict[str, dict] = {
    "claude": {
        "permission_bypass": ["--dangerously-skip-permissions"],
        "resume": "native-session",  # session store + --resume <uuid>
        "bg": True,  # claude --bg
        "stop_hook": "native",
    },
    "codex": {
        "permission_bypass": ["--dangerously-bypass-approvals-and-sandbox"],
        "resume": "native-thread",  # CODEX_THREAD_ID + exec resume
        "bg": False,  # -> headless
        "stop_hook": "native",
    },
    "gemini": {
        "permission_bypass": ["--yolo"],
        "resume": "native-continue",
        "bg": False,
        "stop_hook": "native",
    },
    "agy": {
        # Antigravity: headless-only readable row. Bypass owned by the agy
        # adapter/Rust spawn path; left empty here until a dispatch path needs it.
        "permission_bypass": [],
        "resume": "native-continue",
        "bg": False,
        "stop_hook": "native",
    },
    "opencode": {
        # Headless-only (serve). US4 wires the opencode headless one-shot; its
        # bypass flag is confirmed there, not guessed here.
        "permission_bypass": [],
        "resume": "native-continue",  # opencode run --continue
        "bg": False,
        "stop_hook": "native",
    },
}


class DispatchResolveError(ValueError):
    """A dispatch cannot be resolved (unknown harness, bad substrate, empty
    command). Carries a message naming the offending value AND the map location
    so the failure is loud and actionable (AC1-ERR)."""


def known_harnesses() -> list[str]:
    """Sorted harness names the map knows (the loud-error candidate list)."""
    return sorted(_HARNESS_CAPS)


def capabilities(harness: str) -> dict:
    """Capability dict for ``harness``. Raises :class:`DispatchResolveError`
    naming the map module when unknown - never silently defaults to claude."""
    caps = _HARNESS_CAPS.get(harness)
    if caps is None:
        raise DispatchResolveError(
            f"unknown harness {harness!r}; the harness-capability map "
            f"(fno.agents.harness_map) knows: {', '.join(known_harnesses())}"
        )
    return caps


def substrate_default(harness: str) -> str:
    """Per-harness default substrate: ``bg`` where supported (claude), else
    ``headless``. Never ``pane`` - a pane stalls an autonomous dispatch."""
    return "bg" if capabilities(harness)["bg"] else "headless"


def effort_values(harness: str) -> list[str]:
    """The reasoning-effort value set for ``harness`` (empty if it has no effort
    surface). Sourced from the spawn EFFORT validator's table so the two can
    never drift; a lazy import keeps this leaf free of a load-time dependency."""
    try:
        from fno.agents.mux_spawn import _EFFORT_ALLOWED

        return sorted(_EFFORT_ALLOWED.get(harness, ()))
    except Exception:  # noqa: BLE001 - effort is advisory metadata, never fatal
        return []


_VALID_SUBSTRATES = ("bg", "headless", "pane")
_DEFAULT_COMMAND = "/target no-merge {id}"


def resolve_dispatch(
    *,
    harness: Optional[str] = None,
    substrate: Optional[str] = None,
    node_id: Optional[str] = None,
    command: Optional[str] = None,
    trigger: str = "autonomous",
    settings: object = None,
    dispatch_cfg: Optional[Mapping[str, str]] = None,
) -> dict:
    """Map (config + context) -> the dispatch tuple. Pure; never spawns/claims.

    Precedence (each field independent):
      harness    : explicit > config.dispatch.harness > ``claude``
      substrate  : explicit > config.dispatch.substrate > per-harness default
      command    : explicit > config.dispatch.command   > ``/target no-merge {id}``

    ``trigger`` is ``autonomous`` (fire-and-forget) or ``attended``. An
    autonomous trigger may never resolve ``pane`` (it stalls waiting for a human).

    ``node_id`` when given is substituted into the command's ``{id}`` (exactly
    once, else an error); when absent the template is returned literally (a bare
    ``--harness`` resolution just wants the harness/substrate decision).

    Raises :class:`DispatchResolveError` on: an unknown harness (naming the map),
    an explicit ``bg`` on a non-bg harness (pointing at ``headless``), ``pane``
    under an autonomous trigger, an unknown substrate, or an empty / unsubstituted
    command. ``dispatch_cfg`` overrides the config read (for tests)."""
    cfg = dict(dispatch_cfg) if dispatch_cfg is not None else _load_dispatch_cfg(settings)
    decision: list[str] = []

    # 1. harness
    if harness:
        chosen_harness = harness.strip()
        decision.append(f"harness=explicit({chosen_harness})")
    elif cfg.get("harness"):
        chosen_harness = str(cfg["harness"]).strip()
        decision.append(f"harness=config({chosen_harness})")
    else:
        chosen_harness = "claude"
        decision.append("harness=builtin(claude)")
    caps = capabilities(chosen_harness)  # loud error on unknown (AC1-ERR)

    # 2. substrate. Validate the RESOLVED value once, whatever rung supplied it
    # (explicit flag, config, or per-harness default) - the config rung is a
    # trust boundary too, so a `config.dispatch.substrate` typo must fail loud
    # here, not resolve silently to a launcher.
    if substrate:
        chosen_substrate = substrate.strip()
        decision.append(f"substrate=explicit({chosen_substrate})")
    elif cfg.get("substrate"):
        chosen_substrate = str(cfg["substrate"]).strip()
        decision.append(f"substrate=config({chosen_substrate})")
    else:
        chosen_substrate = substrate_default(chosen_harness)
        decision.append(f"substrate=default({chosen_substrate})")

    if chosen_substrate not in _VALID_SUBSTRATES:
        raise DispatchResolveError(
            f"unknown substrate {chosen_substrate!r}; "
            f"valid: {', '.join(_VALID_SUBSTRATES)}"
        )
    if chosen_substrate == "bg" and not caps["bg"]:
        raise DispatchResolveError(
            f"substrate 'bg' is unsupported on harness {chosen_harness!r} "
            f"(bg is claude-only); use 'headless'"
        )
    # Autonomous triggers never resolve pane (it stalls waiting for a human) -
    # Invariant, fail CLOSED: only an explicit 'attended' trigger opts out, so a
    # malformed/unknown trigger is treated as autonomous and the guard still fires.
    if chosen_substrate == "pane" and (trigger or "").strip().lower() != "attended":
        raise DispatchResolveError(
            "autonomous triggers never resolve substrate 'pane' "
            "(a pane stalls waiting for a human); use 'bg' or 'headless'"
        )

    # 3. command template + substitution
    template = (command or cfg.get("command") or _DEFAULT_COMMAND).strip()
    if not template:
        raise DispatchResolveError("resolved command is empty")
    if node_id:
        if template.count("{id}") != 1:
            raise DispatchResolveError(
                f"command template {template!r} must contain '{{id}}' exactly "
                f"once for substitution; found {template.count('{id}')}"
            )
        resolved_command = template.replace("{id}", node_id.strip())
        decision.append(f"command=substituted({resolved_command})")
    else:
        resolved_command = template
        decision.append(f"command=template({resolved_command})")

    return {
        "map_version": MAP_VERSION,
        "harness": chosen_harness,
        "substrate": chosen_substrate,
        "command": resolved_command,
        "permission_bypass": list(caps["permission_bypass"]),
        "resume": caps["resume"],
        "bg": caps["bg"],
        "effort_values": effort_values(chosen_harness),
        "env": {},  # US3 adds TARGET_BRIEF here
        "decision": decision,
    }


def _load_dispatch_cfg(settings: object) -> dict:
    """Read ``config.dispatch`` (harness/substrate/command) as a plain dict. A
    missing/unreadable config yields ``{}`` so a resolve never bricks on config."""
    if settings is None:
        try:
            from fno.config import load_settings

            settings = load_settings()
        except Exception:  # noqa: BLE001 - a bad config must not brick resolution
            return {}
    try:
        d = settings.dispatch  # type: ignore[attr-defined]
        return {
            "harness": (d.harness or "").strip(),
            "substrate": (d.substrate or "").strip(),
            "command": (d.command or "").strip(),
        }
    except Exception:  # noqa: BLE001
        return {}
