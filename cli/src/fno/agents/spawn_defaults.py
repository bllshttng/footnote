"""Config-sourced spawn defaults, injected argv-level at the dispatch seam.

Every `fno agents spawn` / `/agent spawn` passes the Python dispatch seam
(`rust_runtime.make_context`) before the Rust/Python routing fork. Injecting
`config.agents.defaults` field-by-field on argv HERE covers pane, bg, headless,
and the Rust route with zero Rust changes (Locked Decision 9).

Precedence per field: explicit CLI flag > `agents.defaults` > built-in. Fields
resolve independently, so `-p claude` still inherits a config `model`. Scope is
the operator-initiated spawn surface only; autonomous dispatch computes its own
routing and reaches the seam as explicit flags, never displaced by these.
"""
from __future__ import annotations

import sys
from typing import IO, List, Mapping, Optional, Sequence, Tuple

# Flags that consume the FOLLOWING token. Scanning for our three flags skips a
# value flag's value so a value that looks like `--model` / `--effort` can never
# masquerade as one of ours. Mirrors client.rs VALUE_FLAGS + the short aliases
# typer exposes on the spawn verb.
_VALUE_FLAGS = frozenset(
    {
        "--provider", "-p", "--model", "-m", "--effort", "--from", "--cwd", "-c",
        "--message", "--session-id", "--cc-session-id", "--channel-id", "--status",
        "--from-name", "--timeout", "-t", "--mode", "--substrate", "--permission-mode",
    }
)

_PROVIDER_FLAGS = ("--provider", "-p")
_MODEL_FLAGS = ("--model", "-m")
_EFFORT_FLAGS = ("--effort",)


def _scan(args: Sequence[str]) -> Tuple[bool, Optional[str], bool, bool]:
    """One pass over a spawn argv (verb already stripped by the caller).

    Returns ``(provider_present, provider_value, model_present, effort_present)``.
    Handles both `--flag value` and `--flag=value`; stops at the `--argv`
    payload boundary; skips a value flag's value token.
    """
    provider_present = model_present = effort_present = False
    provider_value: Optional[str] = None
    it = iter(args)
    for a in it:
        if a == "--argv":
            break
        key, eq, val = a.partition("=")
        if key in _PROVIDER_FLAGS:
            provider_present = True
            provider_value = val if eq else next(it, None)
        elif key in _MODEL_FLAGS:
            model_present = True
            if not eq:
                next(it, None)
        elif key in _EFFORT_FLAGS:
            effort_present = True
            if not eq:
                next(it, None)
        elif key in _VALUE_FLAGS and not eq:
            next(it, None)  # skip this flag's value so it can't be misread
    return provider_present, provider_value, model_present, effort_present


def inject_spawn_defaults(
    args: Sequence[str],
    *,
    settings: object = None,
    env: Optional[Mapping[str, str]] = None,
    stderr: Optional[IO[str]] = None,
) -> List[str]:
    """Return ``args`` with `agents.defaults` fields injected where absent.

    Only acts on a `spawn` verb (``args[0] == "spawn"``). Returns the input
    unchanged for any other verb, or when the config load fails (a bad config
    must never brick spawning). Raises ``SystemExit(2)`` on an unknown config
    provider (AC4-ERR). Config-sourced effort degrades open on a provider with
    no reasoning-effort surface (AC6-ERR); an explicit ``--effort`` is left for
    x-a0e0's fail-closed validation downstream.
    """
    out = list(args)
    if not out or out[0] != "spawn":
        return out
    # `spawn --help`/`-h` must always render help, even under a broken config
    # (a bad provider would otherwise exit 2 here before help prints). Stop at
    # the --argv boundary so a payload's own --help is not consumed.
    for a in out[1:]:
        if a == "--argv":
            break
        if a in ("-h", "--help"):
            return out

    if settings is None:
        try:
            from fno.config import load_settings

            settings = load_settings()
        except Exception:
            # A malformed config never bricks spawning (the ONE degrade-open
            # path). A successful load yields a valid SettingsModel whose
            # `.agents.defaults` always exists, so field access below is NOT
            # wrapped: a schema/wiring bug there must surface, not be masked
            # into an invisible no-op (AC5-FR).
            return out
    defaults = settings.agents.defaults  # type: ignore[attr-defined]
    cfg_provider = (defaults.provider or "").strip()
    cfg_model = (defaults.model or "").strip()
    cfg_effort = (defaults.effort or "").strip()
    if not (cfg_provider or cfg_model or cfg_effort):
        return out

    err = stderr if stderr is not None else sys.stderr
    has_provider, explicit_provider, has_model, has_effort = _scan(out[1:])

    inject: List[str] = []
    from_config: List[str] = []

    if cfg_provider and not has_provider:
        from fno.agents.providers import READABLE_PROVIDERS

        if cfg_provider not in READABLE_PROVIDERS:
            print(
                "fno agents spawn: config.agents.defaults.provider = "
                f"{cfg_provider!r} is not a known provider; valid: "
                f"{', '.join(READABLE_PROVIDERS)}",
                file=err,
            )
            raise SystemExit(2)
        inject += ["--provider", cfg_provider]
        from_config.append("provider")

    if cfg_model and not has_model:
        inject += ["--model", cfg_model]
        from_config.append("model")

    if cfg_effort and not has_effort:
        # Effort surface depends on the RESOLVED provider: an explicit -p flag,
        # else the config provider, else harness inference / builtin claude.
        eff_provider = (explicit_provider or "").strip() or cfg_provider
        if not eff_provider:
            from fno.agents.provider_resolve import resolve_dispatch_provider

            # `None` = no explicit provider, so resolve_dispatch_provider does
            # harness inference (env-based via infer_invoking_harness) then the
            # builtin claude. Its first arg is the explicit provider STRING, not
            # argv, and inference reads env markers, not command-line args.
            eff_provider, _ = resolve_dispatch_provider(None, env=env)
        from fno.agents.mux_spawn import _EFFORT_ALLOWED

        allowed = _EFFORT_ALLOWED.get(eff_provider)
        if allowed and cfg_effort in allowed:
            inject += ["--effort", cfg_effort]
            from_config.append("effort")
        else:
            # Config-sourced effort degrades open on BOTH a no-surface provider
            # AND a value the resolved provider can't map (e.g. codex + "xhigh"):
            # an ambient default must never hard-fail a bare spawn. An explicit
            # --effort keeps x-a0e0's fail-closed exit 2 (has_effort short-circuits
            # this whole branch).
            reason = (
                f"no {eff_provider} effort surface"
                if not allowed
                else f"{eff_provider} does not support effort {cfg_effort!r}"
            )
            print(
                f"fno agents spawn: effort skipped ({reason}); "
                f"config.agents.defaults.effort = {cfg_effort!r} ignored",
                file=err,
            )

    if from_config:
        # AC5-FR: config-sourced routing is never invisible.
        print(
            "fno agents spawn: applied config.agents.defaults: "
            + ", ".join(from_config),
            file=err,
        )
    if inject:
        out = [out[0], *inject, *out[1:]]
    return out
