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

Verified facts, each dated where it differs from the 2026-07-13 spike:
- permission_bypass tokens mirror the provider adapters (claude.py,
  codex.py, gemini.py) - the flag a headless/bg worker needs so it never wedges
  on an approval prompt (the concrete cause of the manual-approve pain).
- thread is claude and codex today (``claude --bg`` and the verified Codex
  app-server thread). opencode's serve lane is launch-only (``ask`` refuses,
  no steering over the HTTP API), so its bit reads false until the steering
  lane ships with its own unattended journey test; every false-bit harness
  falls back to ``headless`` (Locked Decision 3, HARNESSES.md).
- loop_participation REPLACED stop_hook (2026-08-28). The old field read
  "native" on every row and had no consumer. The paragraph that stood here
  recorded a 2026-07-13 verification of THREE harnesses, and six rows ended up
  carrying the value - the inheritance in writing, in the same file as the
  field. Each row is now measured against the artifact and the wiring that
  reaches it; gemini and opencode came out wrong. See the table's own comment
  for the per-harness evidence.
"""
from __future__ import annotations

import re
import tomllib
from copy import deepcopy
from functools import cache
from importlib.resources import files
from pathlib import Path
from typing import Mapping, Optional

from fno.config_io import _global_settings_path
from fno.harness_names import KNOWN_HARNESSES

# Command surface: HOW a footnote slash `/verb` is natively invoked on a harness.
# One axis, the single source both dispatch surfaces normalize through
# (autonomous `/target thread` + `/agent spawn`):
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
# `fno do target start` resolving its argument to a bare node id, and unlike the
# token it cannot be manufactured by prose an LLM wrote into a brief.
_AUTONOMOUS_COMMAND = AUTONOMOUS_COMMAND = "/target --no-merge {id}"
_AUTONOMOUS_COMMAND_MERGE = "/target {id}"


@cache
def _carrier_vocab() -> tuple[tuple[str, ...], str, str]:
    """The carrier vocabulary from the ONE canonical merge_posture table
    (x-8151/d-450caaeb): authored in the Rust tree, shipped here as generated
    package data (build.rs byte copy; freshness tripwire in scripts/ci). The
    Rust engine and these readers answer from the same file and cannot
    drift."""
    import tomllib
    from importlib.resources import files

    table = tomllib.loads(
        files("fno.agents").joinpath("merge_posture.toml").read_text(encoding="utf-8")
    )
    return (
        tuple(table["target_family"]["spellings"]),
        str(table["carrier"]["flag"]),
        str(table["carrier"]["legacy_token"]),
    )


_TARGET_FAMILY = _carrier_vocab()[0]



@cache
def footnote_verbs() -> frozenset:
    """The shipped footnote verb roster: every ``skills/<name>/SKILL.md`` and
    every ``commands/<name>.md`` the plugin ships.

    Read from the shipped plugin surface, never a retyped literal: a literal
    copy goes stale the first time a verb ships, and the failure is silent -
    the new verb simply stops normalizing on codex and nothing notices.
    Cached once per process. Empty on any resolution or read failure, which
    on the codex surface leaves every bare ``/verb`` literal - pass-through is
    the safe direction - while the plugin-qualified ``/fno:verb`` spelling
    keeps working, since its namespace alone proves it is a footnote verb."""
    from fno.paths import resolve_plugin_script

    verbs: set[str] = set()
    try:
        skills = resolve_plugin_script("skills")
        verbs.update(p.name for p in skills.iterdir() if (p / "SKILL.md").is_file())
    except OSError:
        pass
    try:
        commands = resolve_plugin_script("commands")
        verbs.update(p.stem for p in commands.glob("*.md") if p.is_file())
    except OSError:
        pass
    return frozenset(verbs)


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
    _spellings, flag, legacy = _carrier_vocab()
    parts = command.split()
    if not parts or parts[0] not in _TARGET_FAMILY:
        return command
    if len(parts) >= 2 and parts[1] == legacy:
        parts[1] = flag
    elif len(parts) >= 3 and parts[-1] == legacy:
        parts[-1] = flag
    else:
        return command
    return " ".join(parts)


def is_target_family(message: str) -> bool:
    """True when the message's first token is a /target-family command
    spelling - the one vocabulary that can carry merge-posture flags.

    A message of only whitespace has no first token. The truthiness guard alone
    did not catch it, so ``"   "`` indexed an empty list and raised rather than
    answering False. Split first, then ask.
    """
    tokens = message.split(maxsplit=1) if message else []
    return bool(tokens) and tokens[0] in _TARGET_FAMILY


def inject_no_merge_into_command(command: str) -> str:
    """Insert the ``--no-merge`` flag into a /target-family command, right
    after the verb token. Skipped when a standalone flag is already present
    (word-padded, so ``--no-merge-guard`` never counts). Non-family commands
    pass through untouched: a prose brief carries its posture in prose (x-9d11)."""
    _spellings, flag, _legacy = _carrier_vocab()
    if not is_target_family(command):
        return command
    if f" {flag} " in f" {command} ":
        return command
    parts = command.split()
    return " ".join([parts[0], flag, *parts[1:]])


def message_carries_no_merge(message: str) -> bool:
    """True when a /target-family message carries the ``--no-merge`` flag.

    The family gate is load-bearing: a /think or /review prompt that MENTIONS
    the flag arms no env carrier, and neither does prose. The word-padded
    match is too: ``--no-merge-guard`` (a different flag) is not the carrier
    (round 8, angle A)."""
    _spellings, flag, _legacy = _carrier_vocab()
    return is_target_family(message) and f" {flag} " in f" {message} "


def apply_merge_posture_env(message: str, *, note_stream=None) -> str | None:
    """Set or clear ``TARGET_NO_MERGE`` in ``os.environ`` from the message,
    and return the prior value (so a caller restoring the process env captured
    it BEFORE the mutation). Flag arms; a family bare token outside flag
    position neither arms nor clears (round 11); a family message with no token
    clears an inherited carrier loudly (round 8); non-family clears NOTHING (a
    leak errs toward refusing merges, the safe side). The binary's spawn lane
    answers from the same table."""
    import os
    import sys

    _spellings, flag, legacy = _carrier_vocab()
    if note_stream is None:
        note_stream = sys.stderr
    prior = os.environ.get("TARGET_NO_MERGE")
    if message_carries_no_merge(message):
        os.environ["TARGET_NO_MERGE"] = "1"
    elif is_target_family(message) and f" {legacy} " in f" {message} ":
        pass
    elif is_target_family(message):
        if prior:
            print(
                "fno agents spawn: inherited TARGET_NO_MERGE cleared; the "
                "/target-family message carries no --no-merge flag and the "
                "message is authoritative",
                file=note_stream,
            )
        os.environ.pop("TARGET_NO_MERGE", None)
    return prior


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
# harness also carries `slash_prefix` (the plugin namespace). `thread` asserts
# fno's OWN driver for that harness (driver + unattended journey test, never a
# bare resume primitive): claude and codex today. `bg` remains a one-release
# input alias.
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
_RESPONSE_ACTIONS = {"allow_once", "allow_always", "deny"}
_SESSION_LANES = {
    "interactive_create",
    "interactive_resume",
    "interactive_attach",
    "headless_create",
    "headless_resume",
}
_RESUME_KINDS = {"flag", "subcommand", "session_flag", "unsupported"}
_MODEL_SWITCH_KINDS = {"direct", "menu_walk", "unsupported"}
_MODEL_SWITCH_PLACEHOLDERS = {"model", "effort", "effort_label"}
_MODEL_SWITCH_EFFORTS = {"low", "medium", "high", "xhigh", "max"}
_KEY_TOKENS = {
    *(str(i) for i in range(1, 10)),
    "enter", "left", "right", "up", "down", "tab", "esc", "y", "a", "d",
    "unsupported",
}
_STOP_STRATEGIES = {"claude-short-id", "registry-noop"}
_REMOVE_STRATEGIES = {"claude-short-id", "codex-session-index", "registry-only"}
# Whether the fno target loop can CLOSE on a harness. Kept identical to the Rust
# validator's LOOP_PARTICIPATION so the two runtimes cannot disagree about which
# contracts are legal. The table's comment carries the per-harness measurement.
_LOOP_PARTICIPATION = {"native", "extension", "none"}


def _contract_error(harness: str, field: str, detail: str) -> "DispatchResolveError":
    return DispatchResolveError(
        f"harness capability contract: harness {harness!r} field {field!r}: {detail}"
    )


def _validate_row(harness: str, caps: dict) -> None:
    """Validate ONE capability row against the contract.

    The per-harness loop body of :func:`parse_capability_contract`,
    extracted so the config-override merge can gate a candidate row
    through the SAME validation the bundled table ships under (x-244c).
    Raises :class:`DispatchResolveError` naming harness + field on the
    first bad field."""
    required = {
        "permission_bypass", "resume", "thread", "autonomous_pane", "route_on_pane",
        "loop_participation", "command_surface", "permission_response", "resume_strategy",
        "model_switch_strategy",
        "ready_marker", "ready_rule_ids", "send_keys_enter_delay_ms", "submit_keys",
        "stop_strategy", "remove_strategy", "manifest_rules", "session_binding",
    }
    if not isinstance(caps, dict) or not required <= caps.keys():
        missing = sorted(required - set(caps or {}))
        raise _contract_error(harness, "contract", f"missing fields: {', '.join(missing)}")
    responses = caps["permission_response"]
    if not isinstance(responses, dict) or set(responses) != _RESPONSE_ACTIONS:
        raise _contract_error(harness, "permission_response", "needs all three actions")
    for action, response in responses.items():
        if not isinstance(response, dict) or not isinstance(response.get("supported"), bool):
            raise _contract_error(harness, f"permission_response.{action}", "bad support flag")
        keys = response.get("keys")
        rules = response.get("rule_ids")
        if not isinstance(keys, list) or not all(key in _KEY_TOKENS for key in keys):
            raise _contract_error(harness, "permission_response", f"bad keys for {action}")
        if not isinstance(rules, list) or not all(isinstance(rule, str) and rule for rule in rules):
            raise _contract_error(harness, "permission_response", f"bad rule ids for {action}")
        if response["supported"] and (not keys or not rules):
            raise _contract_error(harness, "permission_response", f"empty supported {action}")
    marker = caps["ready_marker"]
    ready_rules = caps["ready_rule_ids"]
    manifest_rules = caps["manifest_rules"]
    if not isinstance(marker, str) or not isinstance(ready_rules, list) or not isinstance(
        manifest_rules, list
    ):
        raise _contract_error(harness, "ready_marker", "must name a rule or unsupported")
    parsed_rules = {
        rule.get("id"): rule.get("state")
        for rule in manifest_rules
        if isinstance(rule, dict)
        and isinstance(rule.get("id"), str)
        and rule.get("state") in {"idle", "blocked"}
    }
    if len(parsed_rules) != len(manifest_rules):
        raise _contract_error(harness, "manifest_rules", "contains a malformed rule")
    if marker != "unsupported" and marker not in ready_rules:
        raise _contract_error(harness, "ready_marker", f"unknown rule {marker!r}")
    if marker != "unsupported" and parsed_rules.get(marker) != "idle":
        raise _contract_error(harness, "ready_marker", f"unknown positive rule {marker!r}")
    for action, response in responses.items():
        for rule_id in response["rule_ids"]:
            if parsed_rules.get(rule_id) != "blocked":
                raise _contract_error(
                    harness,
                    "permission_response",
                    f"{action} names unknown blocked rule {rule_id!r}",
                )
    delay = caps["send_keys_enter_delay_ms"]
    submit = caps["submit_keys"]
    if not isinstance(delay, int) or isinstance(delay, bool) or delay < 0:
        raise _contract_error(harness, "send_keys_enter_delay_ms", "must be non-negative")
    if not isinstance(submit, list) or not submit or not all(key in _KEY_TOKENS for key in submit):
        raise _contract_error(harness, "submit_keys", "has an invalid key token")
    # A lane that never submits must not carry a delay: the number would
    # describe a wait nothing performs. The CONVERSE does not hold and was
    # asserted here until codex disproved it. A supported contract may
    # legitimately need no wait - measured against codex 0.148.0, a
    # carriage return sent immediately after the text submits correctly,
    # while claude needs 800ms. The old rule read a coincidence across the
    # then-current harnesses as an invariant.
    if submit == ["unsupported"] and delay != 0:
        raise _contract_error(
            harness,
            "send_keys_enter_delay_ms",
            "an unsupported submit contract cannot carry a nonzero delay",
        )
    strategy = caps["resume_strategy"]
    forms = strategy.get("forms") if isinstance(strategy, dict) else None
    if not isinstance(forms, dict) or set(forms) != _SESSION_LANES:
        raise _contract_error(harness, "resume_strategy", "needs every session lane")
    for lane, form in forms.items():
        kind = form.get("kind") if isinstance(form, dict) else None
        tokens = form.get("tokens") if isinstance(form, dict) else None
        if kind not in _RESUME_KINDS or not isinstance(tokens, list) or not all(
            isinstance(token, str) and token for token in tokens
        ):
            raise _contract_error(harness, "resume_strategy", f"malformed {lane}")
        if kind == "unsupported" and tokens:
            raise _contract_error(harness, "resume_strategy", f"unsupported {lane} has tokens")
        if (
            lane == "interactive_attach"
            and kind != "unsupported"
            and "{short_id}" not in tokens
            and "{session_id}" not in tokens
        ):
            # An attach form must name the id its harness's own attach
            # command takes: claude's short jobId, or a full session id
            # where a short one would collide (a codex UUIDv7 head-8 is a
            # ~65.5s bucket).
            raise _contract_error(harness, "resume_strategy", f"{lane} drops its attach id")
        if lane.endswith("resume") and kind != "unsupported" and "{session_id}" not in tokens:
            raise _contract_error(harness, "resume_strategy", f"{lane} drops session id")
    model_switch = caps["model_switch_strategy"]
    expected_switch_fields = {
        "kind", "tokens", "effort_labels", "status_command", "status_pattern",
    }
    if not isinstance(model_switch, dict) or set(model_switch) != expected_switch_fields:
        raise _contract_error(harness, "model_switch_strategy", "malformed strategy")
    switch_kind = model_switch["kind"]
    switch_tokens = model_switch["tokens"]
    effort_labels = model_switch["effort_labels"]
    status_command = model_switch["status_command"]
    status_pattern = model_switch["status_pattern"]
    if switch_kind not in _MODEL_SWITCH_KINDS:
        raise _contract_error(harness, "model_switch_strategy", "unknown kind")
    if not isinstance(switch_tokens, list) or not all(
        isinstance(token, str) and token for token in switch_tokens
    ):
        raise _contract_error(harness, "model_switch_strategy", "malformed tokens")
    if not isinstance(effort_labels, dict) or not all(
        effort in _MODEL_SWITCH_EFFORTS
        and isinstance(label, str)
        and label
        for effort, label in effort_labels.items()
    ):
        raise _contract_error(harness, "model_switch_strategy", "malformed effort labels")
    placeholders: list[str] = []
    for token in switch_tokens:
        token_placeholders = re.findall(r"\{([^{}]+)\}", token)
        remainder = re.sub(r"\{[^{}]+\}", "", token)
        if "{" in remainder or "}" in remainder:
            raise _contract_error(harness, "model_switch_strategy", "malformed placeholder")
        placeholders.extend(token_placeholders)
    unknown = set(placeholders) - _MODEL_SWITCH_PLACEHOLDERS
    if unknown:
        raise _contract_error(
            harness, "model_switch_strategy", f"unknown placeholder {sorted(unknown)[0]!r}"
        )
    if switch_kind == "unsupported":
        if switch_tokens or effort_labels or status_command or status_pattern:
            raise _contract_error(
                harness, "model_switch_strategy", "unsupported strategy has executable data"
            )
    else:
        if not isinstance(status_command, str) or not status_command.startswith("/"):
            raise _contract_error(harness, "model_switch_strategy", "missing status command")
        if not isinstance(status_pattern, str) or not status_pattern:
            raise _contract_error(harness, "model_switch_strategy", "missing status pattern")
        try:
            compiled_status = re.compile(status_pattern)
        except re.error as exc:
            raise _contract_error(
                harness, "model_switch_strategy", f"invalid status pattern: {exc}"
            ) from exc
        if not {"model", "effort"} <= compiled_status.groupindex.keys():
            raise _contract_error(
                harness, "model_switch_strategy", "status pattern needs model and effort groups"
            )
        if switch_kind == "direct":
            if placeholders.count("model") != 1 or placeholders.count("effort") != 1:
                raise _contract_error(
                    harness, "model_switch_strategy", "direct needs model and effort placeholders"
                )
            if "effort_label" in placeholders or effort_labels:
                raise _contract_error(
                    harness, "model_switch_strategy", "direct cannot carry menu labels"
                )
        elif (
            placeholders.count("model") != 1
            or placeholders.count("effort_label") != 1
            or "effort" in placeholders
            or placeholders.index("model") > placeholders.index("effort_label")
            or set(effort_labels) != _MODEL_SWITCH_EFFORTS
        ):
            raise _contract_error(
                harness,
                "model_switch_strategy",
                "menu_walk needs ordered model and effort targets",
            )
    if caps["loop_participation"] not in _LOOP_PARTICIPATION:
        raise _contract_error(harness, "loop_participation", "unknown member")
    # Only an `extension` row may name an artifact: a `native` row closes its
    # loop through a shell hook and a `none` row closes it through nothing.
    # The converse is legal and load-bearing - an `extension` row with an
    # EMPTY path is a harness whose extension fno has not written yet, and
    # :func:`check_loop_participation` refuses a looping dispatch at it.
    if caps["loop_participation"] != "extension" and caps.get("loop_extension"):
        raise _contract_error(
            harness, "loop_extension",
            "only an extension harness may name a loop artifact",
        )
    if caps["stop_strategy"] not in _STOP_STRATEGIES:
        raise _contract_error(harness, "stop_strategy", "unknown strategy")
    if caps["remove_strategy"] not in _REMOVE_STRATEGIES:
        raise _contract_error(harness, "remove_strategy", "unknown strategy")
    binding = caps["session_binding"]
    if not isinstance(binding, dict) or set(binding) != {"strategy", "required", "timeout_ms"}:
        raise _contract_error(harness, "session_binding", "malformed strategy")
    if binding["strategy"] not in {
        "preassigned-or-session-start", "rollout-fd-or-daemon",
        # caller-assigned-cwd-scoped: the caller mints the id AND the
        # harness scopes its lookup by cwd, so the identity is the PAIR and
        # the id alone addresses nothing. Distinct from "preassigned",
        # where the id is the whole handle.
        "preassigned", "caller-assigned-cwd-scoped", "callee-minted-read-back",
        "store-lookup", "unsupported",
    }:
        raise _contract_error(harness, "session_binding", "unknown strategy")
    if not isinstance(binding["required"], bool) or not isinstance(binding["timeout_ms"], int):
        raise _contract_error(harness, "session_binding", "bad required/timeout values")
    if binding["timeout_ms"] < 0 or (binding["required"] and binding["timeout_ms"] == 0):
        raise _contract_error(harness, "session_binding", "required binding needs a timeout")


def parse_capability_contract(text: str) -> tuple[int, dict[str, dict]]:
    """Parse the packaged per-harness contract and reject partial defaults."""
    try:
        root = tomllib.loads(text)
    except tomllib.TOMLDecodeError as exc:
        raise DispatchResolveError(f"harness capability contract is invalid TOML: {exc}") from exc
    version = root.get("map_version")
    harnesses = root.get("harness")
    if not isinstance(version, int) or version < 1:
        raise DispatchResolveError("harness capability contract field 'map_version' is invalid")
    if not isinstance(harnesses, dict) or not harnesses:
        raise DispatchResolveError(
            "harness capability contract harness set is empty or not a table"
        )
    # Subset, not equality: KNOWN_HARNESSES is the COMPLETE supported roster
    # and a roster entry with no capability row is legal (hermes, openclaw).
    # A capability row naming a harness the roster does not carry is not - it
    # would advertise a dispatch lane for a harness no evidence supports.
    absent = set(harnesses) - set(KNOWN_HARNESSES)
    if absent:
        raise DispatchResolveError(
            "harness capability contract harness set contains names absent "
            f"from KNOWN_HARNESSES: {', '.join(sorted(absent))}"
        )
    for harness, caps in harnesses.items():
        _validate_row(harness, caps)
    _validate_probe_decls(root.get("probe"))
    return version, harnesses


#: The three ways a probe declaration says a field can be settled, kept
#: identical to the Rust validator's PROBE_KINDS.
_PROBE_KINDS = {"declared", "behavioral", "unprobeable"}


def _validate_probe_decls(probe: object) -> None:
    """Validate the ``[probe.*]`` instrument declarations (x-244c): a kind
    may carry only the fields its instrument needs, and a declared pattern
    must compile. A declaration IS an instrument spec; a spec that cannot
    run is a guess with extra steps."""
    if probe is None:
        return
    if not isinstance(probe, dict):
        raise DispatchResolveError("harness capability contract probe table is not a table")
    for field, decl in probe.items():
        if not isinstance(decl, dict) or decl.get("kind") not in _PROBE_KINDS:
            raise _contract_error(field, "probe.kind", "unknown kind")
        kind = decl["kind"]
        need = {
            "declared": ("authority", "pattern"),
            "behavioral": ("marker",),
            "unprobeable": ("reason",),
        }[kind]
        forbid = {
            "declared": ("marker", "reason"),
            "behavioral": ("authority", "pattern", "reason"),
            "unprobeable": ("authority", "pattern", "marker"),
        }[kind]
        for key in need:
            if not str(decl.get(key) or "").strip():
                raise _contract_error(field, f"probe.{key}", f"kind {kind!r} needs {key}")
        for key in forbid:
            if str(decl.get(key) or "").strip():
                raise _contract_error(
                    field, f"probe.{key}", f"kind {kind!r} must not carry {key}"
                )
        if kind == "declared":
            try:
                re.compile(decl["pattern"])
            except re.error as exc:
                raise _contract_error(field, "probe.pattern", f"invalid pattern: {exc}") from exc


def probe_declarations() -> dict[str, dict]:
    """The ``[probe.*]`` instrument table: how each named field can be
    settled. A field absent from it is UNDECLARED, and the probe reports it
    as such instead of guessing an instrument."""
    return deepcopy(_PROBE_DECLS)


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
    to rewrite). So is a slash token with an INTERNAL slash (``/usr/bin/script
    {id}``): an absolute path is nobody's footnote verb, and the guard lives
    HERE rather than at one call site so every caller inherits it - it used to
    live in ``resolve_dispatch`` alone, and the direct callers bypassed it and
    captured an absolute path into a phantom ``$fno:usr/bin/script`` skill.

    On the codex surface a bare ``/verb`` is rewritten only when it names a
    shipped footnote verb (:func:`footnote_verbs`) that is not also a declared
    native verb of the harness (``native_verbs`` in the capability table) -
    native codex verbs (``/review``, ``/model``, ...) pass through untouched.
    The plugin-qualified ``/fno:verb`` spelling is unambiguous by namespace and
    always rewrites. Pure string transform; no config or IO."""
    caps = capabilities(harness)  # loud on an unknown harness, before anything
    cmd = command.strip()
    first_word = cmd.split(maxsplit=1)[0] if cmd else ""
    if first_word.startswith("/") and "/" in first_word[1:]:
        return cmd
    surface = caps["command_surface"]
    if surface == _REFUSED:
        raise DispatchResolveError(_refused_reason(harness))
    if surface == _CODEX_SKILL and cmd.startswith("/"):
        # Operators use both the portable ``/target`` spelling and the
        # advertised plugin-qualified ``/fno:target`` spelling. Codex's native
        # skill surface is ``$fno:target`` in both cases. Strip the optional
        # slash namespace before swapping the surface marker so repeated
        # normalization at independent dispatch choke points is idempotent.
        if cmd.startswith("/fno:"):
            return "$fno:" + cmd[len("/fno:") :]
        verb = first_word[1:]
        # A bare ``/verb`` is rewritten only when it names a shipped footnote
        # verb that is not also a declared native verb of this harness: an
        # unknown or native verb stays literal instead of being captured into
        # a phantom ``$fno:`` skill. ``review`` is the collision case - bare
        # ``/review`` on codex is the NATIVE verb, and the fno lane is reached
        # namespaced, as ``/fno:review``.
        native = {v for v in caps.get("native_verbs") or () if isinstance(v, str)}
        if "/" + verb in native or verb not in footnote_verbs():
            return cmd
        return "$fno:" + verb + cmd[len(first_word):]
    if surface == _SLASH and cmd.startswith("/"):
        # Plugin-namespace prefix swap only (never re-tokenize): claude/agy inject
        # the skill natively (""), opencode's fno plugin exposes it as `/fno:verb`.
        # The single rule renders every verb - no per-verb allowlist (AC4-EDGE).
        prefix = caps.get("slash_prefix", "")
        verb = (
            cmd[len("/fno:") :]
            if harness == "agy" and cmd.startswith("/fno:")
            else cmd[1:]
        )
        # Idempotent over the builtin rung: the resolve seam re-normalizes the
        # already-namespaced `/fno:verb`, so re-applying would double it.
        if prefix and cmd.startswith("/" + prefix):
            return cmd
        return "/" + prefix + verb
    return cmd


def _loop_extension_installed(harness: str) -> bool:
    """Whether this harness's shipped loop artifact is actually installed at
    the harness's own load surface - not merely shipped in the repo.

    A ``loop_extension`` row names a repo path, but the harness only loads
    the copy fno's installer placed at its own extension dir. Advertising a
    closable loop while that copy is absent or stale would dispatch a worker
    with nothing to stop it - the hang the field exists to prevent. A row
    whose harness declares no installer is treated as not installed: an
    extension row ships together with its install arm (opencode and pi both
    did), so the missing arm is a gap to refuse, never a claim to wave
    through.
    """
    try:
        from fno.setup import integration
    except ImportError:
        return False
    checkers = {
        "opencode": integration._opencode_is_installed,
        "pi": integration._pi_is_installed,
    }
    checker = checkers.get(harness)
    if checker is None:
        return False
    try:
        return bool(checker())
    except OSError:
        return False


def check_loop_participation(harness: str, command: str) -> None:
    """Refuse a LOOPING dispatch at a harness that cannot close a loop.

    ``command`` is judged by :func:`is_target_family`, the same vocabulary the
    merge-posture carrier judges, so a one-shot ``/think`` or a bare
    ``opencode run`` passes untouched. A harness
    whose ``loop_participation`` names no reachable boundary would otherwise
    take the dispatch and produce a worker with nothing to stop it: the hang
    this field exists to prevent, not a failure anything reports.

    The refusal text carries the fact rather than a code, because a runtime
    string cannot drift from the behavior it describes the way a doc can.
    """
    if not is_target_family(command):
        return
    caps = capabilities(harness)
    participation = caps["loop_participation"]
    if participation == "native":
        return
    if participation == "extension" and caps.get("loop_extension"):
        if not _loop_extension_installed(harness):
            raise DispatchResolveError(
                f"refused: harness {harness!r} closes its loop through a "
                f"fno-installed extension that is absent or stale on this "
                f"machine. Run 'fno config setup' to install it, then "
                f"dispatch again - a loop whose stop gate is not installed "
                f"would take {command!r} and never stop."
            )
        return
    if participation == "none":
        why = "no lifecycle boundary invokes loop-check"
    else:
        why = (
            "its loop rides a harness-native extension fno has not written yet "
            "and nothing invokes loop-check"
        )
    raise DispatchResolveError(
        f"refused: harness {harness!r} declares loop_participation = "
        f"{participation!r}, so {why} and the looping command {command!r} would "
        f"never stop. Dispatch a one-shot instead."
    )


def dispatch_command(harness: str, allow_merge: bool = False) -> str:
    """Builtin autonomous dispatch command for ``harness``: the per-harness
    normalization of ``/target --no-merge {id}``, or of ``/target {id}`` when
    ``allow_merge``. ``config.dispatch.command`` and a node ``dispatch_verb``
    override this in :func:`resolve_dispatch`, which is also where
    ``config.auto_merge.grant`` is read into ``allow_merge``.

    The default is no-merge, and every error path must land on it: granting
    merge authority is the irreversible direction, so an unreadable config
    fails safe to withholding it, never to handing it out."""
    if not is_declared(harness):
        # The undeclared arm: without a row there is no command_surface to
        # normalize and no refusal text that names the real condition. The
        # deprecated-harness text names agy as a successor - a harness the
        # operator never mentioned - so an undeclared harness must be refused
        # HERE, by its own condition.
        raise DispatchResolveError(
            f"harness {harness!r} has no declared command surface: a native "
            "footnote skill invocation for it must be measured (a row in "
            "harness_capabilities.toml) before one can be generated"
        )
    template = _AUTONOMOUS_COMMAND_MERGE if allow_merge else _AUTONOMOUS_COMMAND
    return normalize_command(template, harness)


class DispatchResolveError(ValueError):
    """A dispatch cannot be resolved (unknown harness, bad substrate, empty
    command). Carries a message naming the offending value AND the map location
    so the failure is loud and actionable (AC1-ERR)."""


_PACKAGED_CONTRACT_TEXT = (
    files("fno.agents").joinpath("harness_capabilities.toml").read_text(encoding="utf-8")
)
MAP_VERSION, _BUNDLED_CAPS = parse_capability_contract(_PACKAGED_CONTRACT_TEXT)
_PROBE_DECLS: dict[str, dict] = tomllib.loads(_PACKAGED_CONTRACT_TEXT).get("probe") or {}
# Non-empty subset of the complete roster, mirroring parse_capability_contract:
# the roster (KNOWN_HARNESSES) is wider than the capability table on purpose.
assert _BUNDLED_CAPS and set(_BUNDLED_CAPS) <= set(KNOWN_HARNESSES)

#: Fail-open report of every override block a reader declined, naming the
#: config file and the reason (AC1-ERR). A warning never un-configures a
#: working harness: the bundled row stays and the mistake is on the record.
OVERRIDE_WARNINGS: list[str] = []

# The x-6678 shallow lane keys an override may still use, mapped into the
# bundled row's nested paths so ONE override shape feeds both readers.
_LANE_ALIAS_PATHS = {
    "attach": ("resume_strategy", "forms", "interactive_attach"),
    "resume": ("resume_strategy", "forms", "interactive_resume"),
}


def _override_config_candidates() -> list[Path]:
    """The same candidate chain the Rust reader uses
    (agents_view.rs ``config_toml_candidates``): ``$PWD/.fno/config.toml``
    first, then the ``config_io`` global settings path's sibling
    ``config.toml`` (``FNO_GLOBAL_SETTINGS_PATH`` when set, else the state
    dir; an empty env var reads as unset there too)."""
    candidates = [Path.cwd() / ".fno" / "config.toml"]
    candidates.append(_global_settings_path().with_name("config.toml"))
    return candidates


def _lane_alias_normalized(override: dict) -> dict:
    """Translate the shallow lane keys into the nested bundled paths, so
    ``[harness.<name>.attach]`` lands on ``resume_strategy.forms.
    interactive_attach`` exactly as it does in the Rust reader."""
    out = {key: value for key, value in override.items() if key not in _LANE_ALIAS_PATHS}
    for alias, path in _LANE_ALIAS_PATHS.items():
        if alias not in override:
            continue
        node = out
        for key in path[:-1]:
            node = node.setdefault(key, {})
        node[path[-1]] = deepcopy(override[alias])
    return out


def _deep_merge_row(base: dict, override: dict) -> dict:
    """Recursive per-field merge, config winning: the ``_DEFAULT_PROVIDERS``
    precedent, one level deeper so a dotted table header
    (``[harness.x.resume_strategy.forms.interactive_attach]``) can correct one
    lane without rewriting the other four. A non-dict value replaces whole."""
    merged = deepcopy(base)
    for key, value in override.items():
        if isinstance(value, dict) and isinstance(merged.get(key), dict):
            merged[key] = _deep_merge_row(merged[key], value)
        else:
            merged[key] = deepcopy(value)
    return merged


def _apply_capability_overrides() -> None:
    """Overlay ``[harness.<name>]`` blocks from the config chain onto
    ``_HARNESS_CAPS``, in place, first candidate wins per harness name
    (project-local before global - the loader's record precedence). Every
    candidate row passes through :func:`_validate_row` BEFORE it lands, so an
    override obeys the same contract the bundled table ships under; a rejected
    override keeps the bundled row and names itself in
    :data:`OVERRIDE_WARNINGS` (AC1-ERR, fail-open). A row for a name the
    roster does not carry is refused by name (AC1-ERR, never advertise an
    unmeasured dispatch lane)."""
    _HARNESS_CAPS.clear()
    _HARNESS_CAPS.update(deepcopy(_BUNDLED_CAPS))
    OVERRIDE_WARNINGS.clear()
    overridden: set[str] = set()
    for path in _override_config_candidates():
        try:
            body = path.read_text(encoding="utf-8")
        except OSError:
            continue
        try:
            doc = tomllib.loads(body)
        except tomllib.TOMLDecodeError as exc:
            OVERRIDE_WARNINGS.append(f"{path}: invalid TOML: {exc}")
            continue
        table = doc.get("harness")
        if not isinstance(table, dict):
            continue
        for name, override in table.items():
            if name in overridden or not isinstance(override, dict):
                continue
            if name not in KNOWN_HARNESSES:
                OVERRIDE_WARNINGS.append(
                    f"{path}: harness {name!r} override rejected: absent from KNOWN_HARNESSES"
                )
                continue
            candidate = _deep_merge_row(
                _HARNESS_CAPS.get(name, {}), _lane_alias_normalized(override)
            )
            try:
                _validate_row(name, candidate)
            except DispatchResolveError as exc:
                OVERRIDE_WARNINGS.append(f"{path}: harness {name!r} override rejected: {exc}")
                continue
            _HARNESS_CAPS[name] = candidate
            overridden.add(name)


def reload_capability_overrides() -> None:
    """Re-read the config chain over the bundled rows. Import time applies it
    once (the Rust reader's contract is the same: its OnceLock resolves a
    config edit at the next process start); this is the test and tool
    re-entry."""
    _apply_capability_overrides()


_HARNESS_CAPS: dict[str, dict] = {}
_apply_capability_overrides()


def known_harnesses() -> list[str]:
    """Sorted names of the harnesses that carry a capability row: the
    loud-error candidate list and the dispatch-capable roster. The COMPLETE
    supported-harness roster is ``fno.harness_names.KNOWN_HARNESSES``, which
    is wider - hermes and openclaw sit on it with no row here."""
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


# The posture for a harness with NO capability row. Every value is the
# fail-closed one, chosen without reading any declared harness's row - the
# whole point is that an unknown harness answers "undeclared" by NAME rather
# than inheriting claude's defaults (the x-ea37 shape).
UNDECLARED_POSTURE: dict = {
    "declared": False,
    "thread": False,
    "autonomous_pane": False,
    "route_on_pane": False,
    "resume": "unsupported",
    "ready_marker": "unsupported",
    "ready_rule_ids": [],
    "manifest_rules": [],
    "loop_participation": "none",
    "loop_extension": "",
    "command_surface": "undeclared",
    "slash_prefix": "",
    "permission_bypass": [],
    "permission_response": {},
    "state_root_grant": {},
    "resume_strategy": {"forms": {}},
    "model_switch_strategy": {"kind": "unsupported"},
    "session_binding": {"required": False},
    "stop_strategy": "registry-noop",
    "remove_strategy": "registry-only",
    # The ONE value that is a default rather than a measurement. `unsupported`
    # here would refuse mail by pane-send, which the undeclared pane lane is
    # supposed to give, and a wrong enter produces a visibly unsubmitted pane
    # (mux's `submitted` marker / exit 22 reports it) rather than a silently
    # wrong answer attributed to the worker.
    "submit_keys": ["enter"],
    "send_keys_enter_delay_ms": 0,
}


def is_declared(harness: str) -> bool:
    """True when ``harness`` carries a measured capability row. The declared/
    undeclared split is a TABLE fact, never a name list, so a new row flips a
    harness to declared with no code edit."""
    return harness in _HARNESS_CAPS


def capabilities_or_undeclared(harness: str) -> dict:
    """Capabilities for ``harness``, or the explicit undeclared posture.

    NOT a softer :func:`capabilities`. That one keeps raising for everyone,
    because a caller that needs a measured value must not receive a guess.
    Only the pane lane - the lane whose whole contract is "fno is the
    viewport" - reads this one, and it branches on the ``declared`` key
    rather than assuming. The key is present on EVERY answer (a declared row
    gets a copy stamped ``declared: True``); a caller must never reach for
    ``capabilities()`` to learn which kind it holds."""
    caps = _HARNESS_CAPS.get(harness)
    if caps is None:
        return dict(UNDECLARED_POSTURE)
    row = dict(caps)
    row["declared"] = True
    return row


def render_session_argv(
    harness: str,
    lane: str,
    session_id: Optional[str] = None,
    *,
    short_id: Optional[str] = None,
) -> list[str]:
    """Render one form with the identity type its contract declares."""
    form = capabilities(harness)["resume_strategy"]["forms"].get(lane)
    if form is None:
        raise DispatchResolveError(f"harness {harness!r} resume_strategy has no lane {lane!r}")
    if form["kind"] == "unsupported":
        raise DispatchResolveError(
            f"harness {harness!r} lane {lane!r} is unsupported by resume_strategy"
        )
    tokens = list(form["tokens"])
    if "{short_id}" in tokens:
        if session_id:
            raise DispatchResolveError(
                f"harness {harness!r} lane {lane!r} needs a short_id, not a session_id"
            )
        if not short_id:
            raise DispatchResolveError(
                f"harness {harness!r} lane {lane!r} needs a non-empty short_id"
            )
        return [short_id if token == "{short_id}" else token for token in tokens]
    if short_id:
        raise DispatchResolveError(
            f"harness {harness!r} lane {lane!r} accepts a session_id, not a short_id"
        )
    if "{session_id}" not in tokens:
        return tokens
    if session_id:
        return [session_id if token == "{session_id}" else token for token in tokens]
    if lane.endswith("create"):
        index = tokens.index("{session_id}")
        start = index - 1 if index > 0 and tokens[index - 1].startswith("-") else index
        return tokens[:start] + tokens[index + 1 :]
    raise DispatchResolveError(f"harness {harness!r} lane {lane!r} needs a non-empty session id")


def permission_response_keys(harness: str, action: str, rule_id: str) -> list[str]:
    """Resolve semantic permission keys only for the manifest rule that matched."""
    response = capabilities(harness)["permission_response"].get(action)
    if response is None:
        raise DispatchResolveError(f"harness {harness!r} has no permission action {action!r}")
    if not response["supported"]:
        raise DispatchResolveError(
            f"harness {harness!r} permission action {action!r} is unsupported"
        )
    if rule_id not in response["rule_ids"]:
        raise DispatchResolveError(
            f"harness {harness!r} permission action {action!r} refuses rule {rule_id!r}"
        )
    return list(response["keys"])


def substrate_default(harness: str) -> str:
    """Per-harness default substrate: ``thread`` where fno's own driver is
    journey-proven (claude, codex), else ``headless``. Pane permission is
    independent from substrate preference."""
    return "thread" if capabilities(harness)["thread"] else "headless"


def thread_lane(harness: str) -> str:
    """Which thread lane this harness needs, from the capability contract
    alone - never from a name list, so a new row lands in its lane with no
    code edit here.

    ``attach``  the harness owns the live session; a client re-attaches to it.
    ``keeper``  the harness persists a transcript only; fno must hold the pty.
    ``none``    no resume form at all, so no lane can be built.
    """
    forms = capabilities(harness)["resume_strategy"]["forms"]
    if forms.get("interactive_attach", {}).get("kind") != "unsupported":
        return "attach"
    if forms.get("interactive_resume", {}).get("kind") != "unsupported":
        return "keeper"
    return "none"


def thread_lane_or_none(harness: str) -> Optional[str]:
    """:func:`thread_lane` for a name the table may not know: ``None`` rather
    than raising.

    The mail send paths read this to route a recipient; a registry row whose
    harness the capability table has dropped keeps its fall-through lanes
    (daemon RPC, durable floor) instead of the send crashing on the way down.
    """
    try:
        return thread_lane(harness)
    except DispatchResolveError:
        return None


def effort_values(harness: str) -> list[str]:
    """Return no static catalog: effort values belong to the provider/model."""
    del harness
    return []


_VALID_SUBSTRATES = ("thread", "headless", "pane")
_LEGACY_SUBSTRATE_ALIASES = {"bg": "thread"}
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
    merge_posture: Optional[str] = None,
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

    ``merge_posture`` (x-8151): ``no-merge`` injects the flag into a
    /target-family command missing it; ``allow`` overrides the builtin rung's
    config read (an explicit template is never edited - a refusal it carries
    wins, every refusal outranks every grant); ``from-config`` resolves
    ``config.auto_merge.grant`` and degrades to no-merge on any error shape.

    Raises :class:`DispatchResolveError` on: an unknown harness (naming the map),
    an explicit ``thread`` on a harness without that lane (pointing at ``headless``), an
    unsupported autonomous ``pane``, an unknown trigger or substrate, or an
    empty / unsubstituted command. ``dispatch_cfg`` overrides the config read
    (for tests)."""
    cfg = (
        dict(dispatch_cfg)
        if dispatch_cfg is not None
        else _load_dispatch_cfg(settings, verb=verb)
    )
    decision: list[str] = []
    chosen_trigger = (trigger or "autonomous").strip().lower() or "autonomous"
    if chosen_trigger not in ("autonomous", "attended"):
        raise DispatchResolveError(
            f"unknown dispatch trigger {trigger!r}; valid: autonomous, attended"
        )
    posture: Optional[str] = merge_posture
    if posture is not None:
        if posture == "from-config":
            posture = "allow" if cfg.get("auto_merge") is True else "no-merge"
            decision.append(f"merge-posture=from-config({posture})")
        elif posture not in ("no-merge", "allow"):
            raise DispatchResolveError(
                f"unknown merge posture {merge_posture!r}; valid: no-merge, allow"
            )

    # 1. harness. An explicit flag is distinguished by ``is not None`` (present
    # vs omitted), NOT truthiness: an empty explicit ``--harness ""`` (e.g. a
    # wrapper interpolating an unset env var) must fail loud, never silently fall
    # through to config/claude - the epic's "never silently default to claude"
    # invariant + the sibling resolve_dispatch_harness contract.
    if harness is not None:
        chosen_harness = harness.strip()
        if not chosen_harness:
            raise DispatchResolveError("explicit --harness must not be empty")
        decision.append(f"harness=explicit({chosen_harness})")
    elif cfg.get("harness"):
        chosen_harness = str(cfg["harness"]).strip()
        decision.append(f"harness=config({chosen_harness})")
        if cfg.get("harness_note"):
            decision.append(str(cfg["harness_note"]))
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

    if chosen_substrate in _LEGACY_SUBSTRATE_ALIASES:
        decision.append("substrate=deprecated-alias(bg->thread)")
        chosen_substrate = _LEGACY_SUBSTRATE_ALIASES[chosen_substrate]
    if chosen_substrate not in _VALID_SUBSTRATES:
        raise DispatchResolveError(
            f"unknown substrate {chosen_substrate!r}; "
            f"valid: {', '.join(_VALID_SUBSTRATES)}"
        )
    if chosen_substrate == "thread" and not caps["thread"]:
        raise DispatchResolveError(
            f"substrate 'thread' is unsupported on harness {chosen_harness!r}: "
            f"fno has not built this harness's {thread_lane(chosen_harness)} lane "
            f"yet, and a false `thread` row records that gap in fno, never a "
            f"harness limitation (bg is a deprecated alias); use 'headless'"
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
            "autonomous_pane capability; use 'headless' (or 'thread' on "
            f"{', '.join(h for h in known_harnesses() if capabilities(h)['thread'])})"
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
        _allow_merge = cfg.get("auto_merge") is True or posture == "allow"
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
    # un-namespaced `/target` instead of `/fno:target`). The first-word guard
    # (absolute paths pass through) lives INSIDE normalize_command, so this
    # call is unguarded by design and every caller shares one implementation.
    # Non-slash templates (`$fno:...`) pass through unchanged, and the call is
    # idempotent over the builtin/verb rungs' output.
    normalized_cmd = normalize_command(template, chosen_harness)
    if normalized_cmd != template:
        template = normalized_cmd
        decision.append(f"command=normalized({chosen_harness})")
    # The loop gate, at the same choke point every spawn surface resolves
    # through. It reads a CAPABILITY, never a harness name, and it fires after
    # normalization so it judges the per-harness /target spelling the worker
    # will actually receive. Deliberately not at registry load: an alien or
    # one-shot dispatch must still resolve fine, matching the existing split
    # where the load gate is a shape check and the dispatch gate is where a
    # capability is required.
    check_loop_participation(chosen_harness, template)
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
    # x-8151: a no-merge posture injects after the legacy rewrite, on EVERY
    # rung. allow never edits a template: a refusal it carries wins.
    if posture == "no-merge":
        injected = inject_no_merge_into_command(resolved_command)
        if injected != resolved_command:
            resolved_command = injected
            decision.append("merge-posture=no-merge(injected)")
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
        "permission_response": deepcopy(caps["permission_response"]),
        "resume_strategy": deepcopy(caps["resume_strategy"]),
        "model_switch_strategy": deepcopy(caps["model_switch_strategy"]),
        "ready_marker": caps["ready_marker"],
        "send_keys_enter_delay_ms": caps["send_keys_enter_delay_ms"],
        "submit_keys": list(caps["submit_keys"]),
        "loop_participation": caps["loop_participation"],
        "stop_strategy": caps["stop_strategy"],
        "remove_strategy": caps["remove_strategy"],
        "session_binding": deepcopy(caps["session_binding"]),
        "thread": caps["thread"],
        "effort_values": effort_values(chosen_harness),
        "env": env,
        "decision": decision,
    }


def _load_dispatch_cfg(settings: object, verb: Optional[str] = None) -> dict:
    """Read the dispatch config rung as a plain dict: the stage-table harness
    (with the deprecated ``dispatch.harness`` folded beneath it) plus
    ``config.dispatch`` substrate/command and the ``config.auto_merge.grant``
    actor key. A missing/unreadable config yields ``{}`` so a resolve never
    bricks on config.

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
    # One home for the harness axis (the stage table) with the deprecated
    # dispatch.harness folded beneath it; the note names the losing spelling
    # when both were set and disagreed.
    from fno.dispatch_flags import configured_dispatch_harness

    harness_value, harness_note = configured_dispatch_harness(settings, verb=verb or "target")
    d = getattr(settings, "dispatch", None)
    # The grant lives in config.auto_merge, NOT under dispatch (x-4be1), so it
    # is read before the dispatch-block gate: a settings object carrying an
    # auto_merge block but no dispatch overlay still resolves its grant, and a
    # stub without either degrades to no-grant.
    am_block = getattr(settings, "auto_merge", None)
    grant = getattr(am_block, "grant", None) == "dispatch"
    if d is None:
        return {
            "harness": harness_value or "",
            "harness_note": harness_note or "",
            "auto_merge": grant,
        }

    def _text(name: str) -> str:
        return (getattr(d, name, None) or "").strip()

    try:
        return {
            "harness": harness_value or "",
            "harness_note": harness_note or "",
            "substrate": _text("substrate"),
            "command": _text("command"),
            "allowed_verbs": list(getattr(d, "allowed_verbs", None) or []),
            # Strict literal compare, not truthiness: only the "dispatch"
            # grant grants (a stray truthy value or a stub block never does).
            "auto_merge": grant,
        }
    except Exception:  # noqa: BLE001
        return {}
