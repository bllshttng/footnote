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

from fno.harness_names import KNOWN_HARNESSES

# Bump when a capability KEY is added/removed or a value's meaning changes, so a
# consumer can assert the shape it was written against.
MAP_VERSION = 6  # autonomous_pane + route_on_pane capabilities

# Command surface: HOW a footnote slash `/verb` is natively invoked on a harness.
# One axis, the single source both dispatch surfaces normalize through
# (autonomous `/target bg` + `/agent spawn`):
#   "slash"       claude, agy, opencode -> "/[slash_prefix]verb ..." native slash
#                 command; per-harness `slash_prefix` ("" for claude/agy, "fno:"
#                 for opencode's plugin-namespaced palette + `run --command`)
#   "codex-skill" codex                 -> "$fno:verb ..." plugin skill expansion
#   "refused"     gemini                -> a loud error naming agy (deprecated)
_SLASH, _CODEX_SKILL, _REFUSED = "slash", "codex-skill", "refused"

# The canonical (claude-syntax) autonomous dispatch command. normalize_command
# maps it per-harness for the builtin `dispatch_command`, so the per-harness
# spelling lives in ONE place (command_surface), not five literal strings.
#
# The `--no-merge` flag is the merge POSTURE, resolved from
# config.auto_merge.grant rather than baked in. It was the free-text
# `no-merge` token until x-8e59 (config-deaf) and x-9d11 (free text stopped
# being a control input): the flag is the deterministic carrier that survives
# `fno target start` resolving its argument to a bare node id, and unlike the
# token it cannot be manufactured by prose an LLM wrote into a brief.
_AUTONOMOUS_COMMAND = "/target --no-merge {id}"
_AUTONOMOUS_COMMAND_MERGE = "/target {id}"

# The /target-family first tokens (the per-harness spellings normalize_command
# emits). The refusal carrier's vocabulary is scoped to these: rewriting the
# args of another slash verb would mutate its instruction text, and prose is
# not a control input (x-9d11).
_TARGET_FAMILY = ("/target", "/fno:target", "$fno:target")


def normalize_legacy_no_merge(command: str) -> str:
    """Rewrite the legacy bare ``no-merge`` token to the flag in a
    /target-family command. Scoped to the two positions the legacy injectors
    actually produced (round 12): the token directly after the verb
    (``/target no-merge <arg>``, the documented spelling) or trailing
    (``/target <arg> no-merge``, the old normalize.sh append and keep_going
    build). A MID-STRING token is left alone on purpose: a /target argument is
    free text (``/target fix the no-merge carrier bug`` is a real feature
    description), and rewriting the word anywhere would mutate prompt text the
    operator typed and arm a refusal from prose (round 10)."""
    parts = command.split()
    if not parts or parts[0] not in _TARGET_FAMILY:
        return command
    if len(parts) >= 2 and parts[1] == "no-merge":
        parts[1] = "--no-merge"
    elif len(parts) >= 3 and parts[-1] == "no-merge":
        parts[-1] = "--no-merge"
    else:
        return command
    return " ".join(parts)


def is_target_family(message: str) -> bool:
    """True when the message's first token is a /target-family command
    spelling - the one vocabulary that can carry merge-posture flags."""
    first = message.split(maxsplit=1)[0] if message else ""
    return first in _TARGET_FAMILY


def message_carries_no_merge(message: str) -> bool:
    """True when a /target-family message carries the ``--no-merge`` flag.

    The family gate is load-bearing: a /think or /review prompt that MENTIONS
    the flag arms no env carrier, and neither does prose. The word-padded
    match is too: ``--no-merge-guard`` (a different flag) is not the carrier
    (round 8, angle A)."""
    return is_target_family(message) and " --no-merge " in f" {message} "


def _refused_reason(harness: str) -> str:
    """The loud-refusal message for a deprecated harness with no dispatch lane -
    names the successor (agy) so the failure is actionable (AC2-ERR / AC3-UI)."""
    return (
        f"harness {harness!r} has no maintained footnote dispatch lane and is "
        f"deprecated; route this work to its successor 'agy' (or a "
        f"claude/codex/opencode harness) - no prose build brief is generated"
    )

# capability -> per-harness value, keyed by the READABLE_PROVIDERS set. Each
# harness carries a `command_surface` (x-a5e4): the invocation form its native
# footnote skill takes, or `refused` where the harness is deprecated. A slash
# harness also carries `slash_prefix` (the plugin namespace). `bg` is claude-only.
#
# Two pane capabilities, both EVIDENCE-GATED and read fail-closed (a missing key
# reads false), because each one used to be a blanket rule that was true of at
# most one harness:
#   autonomous_pane  Does a fire-and-forget pane worker on this harness run to
#                    completion without an operator? The old guard refused pane
#                    for EVERY harness on the claim that "a pane stalls waiting
#                    for a human". That claim is per-harness, not universal, and
#                    each true value here must be backed by an unattended journey
#                    test (see cli/tests/agents/test_spawn_pane.py) - never by
#                    another harness's result.
#   route_on_pane    Can a model route's complete environment reach a pane child
#                    on this harness? The pane launcher materializes a route via
#                    mux_spawn._mesh_env_wrapper, whose endpoint/auth/model
#                    assignment + inherited-credential scrub is claude-shaped;
#                    a harness whose route env semantics are not covered end to
#                    end stays false.
# Neither key changes `substrate_default`: permission to use pane is a separate
# decision from preferring it, and the defaults are unchanged.
_HARNESS_CAPS: dict[str, dict] = {
    "claude": {
        "permission_bypass": ["--dangerously-skip-permissions"],
        "resume": "native-session",  # session store + --resume <uuid>
        "bg": True,  # claude --bg
        "autonomous_pane": False,
        "route_on_pane": True,
        "stop_hook": "native",
        # Native slash-command invocation of the target skill (verified).
        "command_surface": _SLASH,
        "slash_prefix": "",
    },
    "codex": {
        "permission_bypass": ["--dangerously-bypass-approvals-and-sandbox"],
        "resume": "native-thread",  # CODEX_THREAD_ID + exec resume
        "bg": False,  # -> headless
        "autonomous_pane": True,
        "route_on_pane": False,
        "stop_hook": "native",
        # `$fno:target` invokes the footnote plugin skill. VERIFIED: `codex exec`
        # injects the fno skill definitions and expands `$fno:verb` (not a
        # literal prompt). Supersedes the old "prose brief only" guidance.
        "command_surface": _CODEX_SKILL,
    },
    "gemini": {
        "permission_bypass": ["--yolo"],
        "resume": "native-continue",
        "bg": False,
        "autonomous_pane": False,
        "route_on_pane": False,
        "stop_hook": "native",
        # gemini CLI is deprecated (agy is its successor); its build lane is a
        # loud refusal, not a maintained brief (x-de43). No dispatch surface.
        "command_surface": _REFUSED,
    },
    "agy": {
        # Antigravity CLI (gemini's successor). Its migration guide converts
        # legacy commands to skills and recognizes `.agents/skills/` entries as
        # active slash commands, so the target skill invokes as `/target`
        # (grounded in antigravity.google/docs/cli/gcli-migration; live-verify
        # before relying on it for production dispatch).
        "permission_bypass": ["--dangerously-skip-permissions"],
        "resume": "native-continue",
        "bg": False,
        "autonomous_pane": False,
        "route_on_pane": False,
        "stop_hook": "native",
        "command_surface": _SLASH,
        "slash_prefix": "",
    },
    "opencode": {
        # The fno opencode plugin exposes the footnote verbs as `/fno:verb` in the
        # command palette (operator-verified live on GLM) AND headlessly: probed
        # x-de43 against opencode v1.14.50, `opencode run --command fno:target`
        # resolves the plugin command registry (the full `fno:*` namespace is
        # registered), so a slash surface expands in both lanes - not a no-op.
        # Plugin-namespaced, so `slash_prefix` is "fno:" (palette `/fno:verb`).
        "permission_bypass": ["--dangerously-skip-permissions"],
        # x-830c: session store + `--session <ses_id>`, the same strict id-keyed
        # shape as claude. NOT `--continue`, which creates a NEW session when the
        # project has none rather than refusing - never a resume.
        "resume": "native-session",
        "bg": False,
        "autonomous_pane": False,
        "route_on_pane": False,
        "stop_hook": "native",
        "command_surface": _SLASH,
        "slash_prefix": "fno:",
    },
}

# Anti-drift: the capability table's keys must agree with the canonical name
# list (fno.harness_names), which platform-layer code reads without reaching into
# this runtime module (x-cec8). Adding a harness is one coupled change: the name
# lands in KNOWN_HARNESSES and a capability dict lands here, or this import fails
# loudly. Keeps the single-source-of-truth property the old derivation had.
assert set(_HARNESS_CAPS) == set(KNOWN_HARNESSES), (
    "harness capability table keys diverge from fno.harness_names.KNOWN_HARNESSES; "
    "add the harness in both places"
)


def normalize_command(command: str, harness: str) -> str:
    """Translate a claude-syntax footnote slash command to ``harness``'s native
    invocation - the single normalizer both dispatch surfaces route through.

    ``/target --no-merge {id}`` becomes, per the harness ``command_surface``:
      - ``slash`` (claude, agy, opencode) -> ``/[slash_prefix]target --no-merge {id}``
        (prefix ``""`` for claude/agy -> verbatim; ``"fno:"`` for opencode's
        plugin-namespaced palette + ``opencode run --command`` -> ``/fno:target``)
      - ``codex-skill`` (codex)           -> ``$fno:target --no-merge {id}`` (swap the
        leading ``/verb`` for ``$fno:verb``; codex exec expands the plugin skill)
      - ``refused`` (gemini)              -> a loud :class:`DispatchResolveError`
        naming agy; the harness is deprecated and has no dispatch lane.

    ``command`` is expected to lead with ``/`` (a footnote slash command); a
    non-slash string is returned unchanged for the slash/codex surfaces (nothing
    to rewrite). Pure string transform; no config or IO."""
    caps = capabilities(harness)
    surface = caps["command_surface"]
    cmd = command.strip()
    if surface == _REFUSED:
        raise DispatchResolveError(_refused_reason(harness))
    if surface == _CODEX_SKILL and cmd.startswith("/"):
        # Operators use both the portable ``/target`` spelling and the
        # advertised plugin-qualified ``/fno:target`` spelling. Codex's native
        # skill surface is ``$fno:target`` in both cases. Strip the optional
        # slash namespace before swapping the surface marker so repeated
        # normalization at independent dispatch choke points is idempotent.
        verb = cmd[len("/fno:") :] if cmd.startswith("/fno:") else cmd[1:]
        return "$fno:" + verb
    if surface == _SLASH and cmd.startswith("/"):
        # Plugin-namespace prefix swap only (never re-tokenize): claude/agy inject
        # the skill natively (""), opencode's fno plugin exposes it as `/fno:verb`.
        # The single rule renders every verb - no per-verb allowlist (AC4-EDGE).
        prefix = caps.get("slash_prefix", "")
        # Idempotent over the builtin rung: the resolve seam re-normalizes the
        # already-namespaced `/fno:verb`, so re-applying would double it.
        if prefix and cmd.startswith("/" + prefix):
            return cmd
        return "/" + prefix + cmd[1:]
    return cmd


def dispatch_command(harness: str, allow_merge: bool = False) -> str:
    """Builtin autonomous dispatch command for ``harness``: the per-harness
    normalization of ``/target --no-merge {id}``, or of ``/target {id}`` when
    ``allow_merge``. ``config.dispatch.command`` and a node ``dispatch_verb``
    override this in :func:`resolve_dispatch`, which is also where
    ``config.auto_merge.grant`` is read into ``allow_merge``.

    The default is no-merge, and every error path must land on it: granting
    merge authority is the irreversible direction, so an unreadable config
    fails safe to withholding it, never to handing it out."""
    template = _AUTONOMOUS_COMMAND_MERGE if allow_merge else _AUTONOMOUS_COMMAND
    return normalize_command(template, harness)


class DispatchResolveError(ValueError):
    """A dispatch cannot be resolved (unknown harness, bad substrate, empty
    command). Carries a message naming the offending value AND the map location
    so the failure is loud and actionable (AC1-ERR)."""


def known_harnesses() -> list[str]:
    """Sorted harness names the map knows (the loud-error candidate list)."""
    return sorted(KNOWN_HARNESSES)


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
    ``headless``. Pane permission is independent from substrate preference."""
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
# US3: the built-in verb allowlist (config.dispatch.allowed_verbs overrides).
_DEFAULT_ALLOWED_VERBS = ("/target", "/think")
# The env budget a brief must fit; 8 KB, measured in UTF-8 bytes (Locked
# Decision 9 / epic Boundaries). Oversized -> explicit error, never truncation.
_BRIEF_MAX_BYTES = 8192
# The default command is per-harness now (each harness's `dispatch_command` in
# _HARNESS_CAPS), not a single template - see the resolve builtin branch.


def resolve_dispatch(
    *,
    harness: Optional[str] = None,
    substrate: Optional[str] = None,
    node_id: Optional[str] = None,
    command: Optional[str] = None,
    verb: Optional[str] = None,
    brief: Optional[str] = None,
    trigger: str = "autonomous",
    settings: object = None,
    dispatch_cfg: Optional[Mapping[str, object]] = None,
) -> dict:
    """Map (config + context) -> the dispatch tuple. Pure; never spawns/claims.

    Precedence (each field independent):
      harness    : explicit > config.dispatch.harness > ``claude``
      substrate  : explicit > config.dispatch.substrate > per-harness default
      command    : explicit > node ``verb`` > config.dispatch.command > builtin
      merge      : builtin rung only; ``config.auto_merge.grant`` picks
                   ``/target {id}`` over the default ``/target --no-merge {id}``

    ``verb`` is a node's ``dispatch_verb`` (US3): validated against the allowlist
    (``config.dispatch.allowed_verbs`` > built-in ``/target``, ``/think``) and
    assembled as ``<verb> {id}`` - a graph field is a trust boundary, so an
    out-of-allowlist verb is refused. ``brief`` is a node's ``dispatch_brief``:
    it rides ``env['TARGET_BRIEF']`` only (never the command line) and is capped
    at 8 KB with an explicit error, never truncated.

    ``trigger`` is ``autonomous`` (fire-and-forget) or ``attended``. An
    autonomous pane requires evidence-backed per-harness capability.

    ``node_id`` when given is substituted into the command's ``{id}`` (exactly
    once, else an error); when absent the template is returned literally (a bare
    ``--harness`` resolution just wants the harness/substrate decision).

    Raises :class:`DispatchResolveError` on: an unknown harness (naming the map),
    an explicit ``bg`` on a non-bg harness (pointing at ``headless``), an
    unsupported autonomous ``pane``, an unknown trigger or substrate, or an
    empty / unsubstituted command. ``dispatch_cfg`` overrides the config read
    (for tests)."""
    cfg = dict(dispatch_cfg) if dispatch_cfg is not None else _load_dispatch_cfg(settings)
    decision: list[str] = []
    chosen_trigger = (trigger or "autonomous").strip().lower() or "autonomous"
    if chosen_trigger not in ("autonomous", "attended"):
        raise DispatchResolveError(
            f"unknown dispatch trigger {trigger!r}; valid: autonomous, attended"
        )

    # 1. harness. An explicit flag is distinguished by ``is not None`` (present
    # vs omitted), NOT truthiness: an empty explicit ``--harness ""`` (e.g. a
    # wrapper interpolating an unset env var) must fail loud, never silently fall
    # through to config/claude - the epic's "never silently default to claude"
    # invariant + the sibling resolve_dispatch_provider contract.
    if harness is not None:
        chosen_harness = harness.strip()
        if not chosen_harness:
            raise DispatchResolveError("explicit --harness must not be empty")
        decision.append(f"harness=explicit({chosen_harness})")
    elif cfg.get("harness"):
        chosen_harness = str(cfg["harness"]).strip()
        decision.append(f"harness=config({chosen_harness})")
    else:
        chosen_harness = "claude"
        decision.append("harness=builtin(claude)")
    caps = capabilities(chosen_harness)  # loud error on unknown (AC1-ERR)
    if caps["command_surface"] == _REFUSED:
        # A deprecated harness has no dispatch lane - refuse the WHOLE resolve up
        # front (every command shape, slash or non-slash prose template), not only
        # the rendering seam, so a non-slash explicit template can't slip through
        # (AC2-ERR). Names the successor (agy) so the refusal is actionable.
        raise DispatchResolveError(_refused_reason(chosen_harness))

    # 2. substrate. Validate the RESOLVED value once, whatever rung supplied it
    # (explicit flag, config, or per-harness default) - the config rung is a
    # trust boundary too, so a `config.dispatch.substrate` typo must fail loud
    # here, not resolve silently to a launcher. An empty explicit flag rejects
    # for the same reason as harness above.
    if substrate is not None:
        chosen_substrate = substrate.strip()
        if not chosen_substrate:
            raise DispatchResolveError("explicit --substrate must not be empty")
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
    # Only an explicit attended trigger bypasses the autonomy capability check.
    # A missing key is false so newly added or partially specified harnesses stay
    # closed until an unattended journey proves the pane can complete by itself.
    if (
        chosen_substrate == "pane"
        and chosen_trigger != "attended"
        and not caps.get("autonomous_pane", False)
    ):
        raise DispatchResolveError(
            f"harness {chosen_harness!r} does not have the evidence-backed "
            "autonomous_pane capability; use 'bg' or 'headless'"
        )

    # 3. command template. Precedence: explicit --command > node verb > config
    # template > per-harness builtin (dispatch_command). A node verb is validated
    # against the allowlist (a graph field is a trust boundary) and assembled as
    # `<verb> {id}`; the merge posture (no-merge) is NOT part of the verb string -
    # it stays a launcher flag.
    if command is not None and command.strip():
        template = command.strip()
        decision.append("command=explicit")
    elif verb is not None:
        chosen_verb = verb.strip()
        if not chosen_verb:
            raise DispatchResolveError("explicit dispatch verb must not be empty")
        # A plugin-qualified verb (`/fno:target`) canonicalizes to its bare form
        # (`/target`) before the allowlist check. The allowlist and the stored
        # command are canonical; the per-harness command_surface re-adds the
        # `/fno:` prefix at render (opencode) or leaves it bare (claude/agy). So a
        # court that follows the "every dispatched verb is plugin-qualified"
        # contract can set `--dispatch-verb /fno:target` without tripping the
        # bare-only allowlist and breaking the encode-before-exit tail (US7 review).
        if chosen_verb.startswith("/fno:"):
            chosen_verb = "/" + chosen_verb[len("/fno:"):]
        _av = cfg.get("allowed_verbs")
        allowed = list(_av) if isinstance(_av, list) else list(_DEFAULT_ALLOWED_VERBS)
        if chosen_verb not in allowed:
            raise DispatchResolveError(
                f"dispatch verb {chosen_verb!r} is not in the allowlist "
                f"({', '.join(allowed)}); set config.dispatch.allowed_verbs to extend it"
            )
        # Slash-leading; the post-ladder seam normalizes it per-harness.
        template = f"{chosen_verb} {{id}}"
        decision.append(f"command=verb({chosen_verb})")
    else:
        # Per-harness builtin (x-a5e4): the normalize of `/target --no-merge {id}` -
        # codex `$fno:target`, claude/agy `/target`, opencode `/fno:target`, gemini
        # refused. config.dispatch.command overrides.
        #
        # The merge posture comes from config.auto_merge.grant (x-8e59/x-4be1).
        # It applies to the builtin only: an explicit `command` or a node
        # `dispatch_verb` already spells out what to run, and silently editing
        # a caller's own template would be the surprising read.
        _cmd = cfg.get("command")
        _allow_merge = cfg.get("auto_merge") is True
        template = (
            _cmd if isinstance(_cmd, str) and _cmd
            else dispatch_command(chosen_harness, allow_merge=_allow_merge)
        ).strip()
        if cfg.get("command"):
            decision.append("command=config")
        else:
            decision.append(
                f"command=builtin({'merge' if _allow_merge else 'no-merge'})"
            )

    if not template:
        raise DispatchResolveError("resolved command is empty")
    # Single normalization seam (x-f0e2): a footnote slash command (`/verb ...`)
    # is canonical claude syntax on EVERY rung - normalize it once here, per the
    # chosen harness, before `{id}` substitution. This stops the config and
    # explicit rungs handing a codex worker a raw `/target` (or opencode an
    # un-namespaced `/target` instead of `/fno:target`). Gate on the FIRST word
    # being a single slash-led token with no internal slash: that admits
    # `/target`/`/think`/`/custom` but NOT an absolute-path template like
    # `/usr/bin/script {id}`, which must pass through literally. Non-slash
    # templates (`$fno:...`) also pass through, and the call is idempotent over
    # the builtin/verb rungs' output.
    first_word = template.split(maxsplit=1)[0]
    if first_word.startswith("/") and "/" not in first_word[1:]:
        template = normalize_command(template, chosen_harness)
        decision.append(f"command=normalized({chosen_harness})")
    if node_id:
        # `{id}` must appear at least once; a template may reference it more than
        # once (str.replace substitutes every occurrence).
        if "{id}" not in template:
            raise DispatchResolveError(
                f"command template {template!r} must contain '{{id}}' at least "
                f"once for substitution"
            )
        resolved_command = template.replace("{id}", node_id.strip())
        decision.append(f"command=substituted({resolved_command})")
    else:
        resolved_command = template
        decision.append(f"command=template({resolved_command})")

    # 4. brief -> TARGET_BRIEF env only (never the command line). Byte-capped at
    # the 8 KB env budget; an oversized brief is an explicit error, not truncation.
    # x-9d11 refusal carrier, at the ONE choke point every spawn surface resolves
    # through (skill spawn.sh, dispatch.py pane, advance/recovery/keep_going bg):
    # when the command carries the refusal, the env carries it too, so a worker
    # that drops the flag post-compaction still folds the refusal at init.
    # The /target-family gate and the legacy-token rewrite live in
    # normalize_legacy_no_merge / message_carries_no_merge so every spawn lane
    # (including direct `fno agents spawn` messages that never reach this
    # resolver) judges the SAME vocabulary.
    normalized = normalize_legacy_no_merge(resolved_command)
    if normalized != resolved_command:
        resolved_command = normalized
        decision.append("command=legacy-no-merge->--no-merge")
    env: dict[str, str] = {}
    if message_carries_no_merge(resolved_command):
        env["TARGET_NO_MERGE"] = "1"
        decision.append("no-merge->TARGET_NO_MERGE")
    if brief:
        n_bytes = len(brief.encode("utf-8"))
        if n_bytes > _BRIEF_MAX_BYTES:
            raise DispatchResolveError(
                f"dispatch brief is {n_bytes} bytes, over the {_BRIEF_MAX_BYTES}-byte "
                f"(8 KB) env budget; shorten it (no silent truncation)"
            )
        env["TARGET_BRIEF"] = brief
        decision.append(f"brief={n_bytes}B->TARGET_BRIEF")

    return {
        "map_version": MAP_VERSION,
        "harness": chosen_harness,
        "substrate": chosen_substrate,
        "command": resolved_command,
        "command_surface": caps["command_surface"],
        "permission_bypass": list(caps["permission_bypass"]),
        "resume": caps["resume"],
        "bg": caps["bg"],
        "effort_values": effort_values(chosen_harness),
        "env": env,
        "decision": decision,
    }


def _load_dispatch_cfg(settings: object) -> dict:
    """Read ``config.dispatch`` (harness/substrate/command) plus the
    ``config.auto_merge.grant`` actor key as a plain dict. A missing/unreadable
    config yields ``{}`` so a resolve never bricks on config.

    Every field is read through ``getattr`` with a default, per field. Attribute
    access on a partial settings object (a caller's stub, an older config model)
    used to raise and drop the WHOLE dict on the floor, so one missing key
    silently disabled every other one - the failure mode that let
    ``auto_merge.grant`` be set and ignored. A field that is absent is now
    just absent."""
    if settings is None:
        try:
            from fno.config import load_settings

            settings = load_settings()
        except Exception:  # noqa: BLE001 - a bad config must not brick resolution
            return {}
    d = getattr(settings, "dispatch", None)
    # The grant lives in config.auto_merge, NOT under dispatch (x-4be1), so it
    # is read before the dispatch-block gate: a settings object carrying an
    # auto_merge block but no dispatch overlay still resolves its grant, and a
    # stub without either degrades to no-grant.
    am_block = getattr(settings, "auto_merge", None)
    grant = getattr(am_block, "grant", None) == "dispatch"
    if d is None:
        return {"auto_merge": grant}

    def _text(name: str) -> str:
        return (getattr(d, name, None) or "").strip()

    try:
        return {
            "harness": _text("harness"),
            "substrate": _text("substrate"),
            "command": _text("command"),
            "allowed_verbs": list(getattr(d, "allowed_verbs", None) or []),
            # Strict literal compare, not truthiness: only the "dispatch"
            # grant grants (a stray truthy value or a stub block never does).
            "auto_merge": grant,
        }
    except Exception:  # noqa: BLE001
        return {}
