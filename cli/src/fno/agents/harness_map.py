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
- the thread seat is derived, never stored: ``features.spawn`` reads ``native``
  where fno's own launch arm is wired and journey-proven (claude, codex,
  opencode, pi, cursor-agent, grok), and every other harness falls back to
  ``headless`` (Locked Decision 3, HARNESSES.md). opencode's serve lane stays
  launch-only (``ask`` refuses, no steering over the HTTP API); its spawn row
  is what seats the lane.
- loop_participation REPLACED stop_hook (2026-08-28). The old field read
  "native" on every row and had no consumer. The paragraph that stood here
  recorded a 2026-07-13 verification of THREE harnesses, and six rows ended up
  carrying the value - the inheritance in writing, in the same file as the
  field. Each row is now measured against the artifact and the wiring that
  reaches it; gemini and opencode came out wrong. See the table's own comment
  for the per-harness evidence.
"""
from __future__ import annotations

import json
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
    (x-8151): authored in the Rust tree, shipped here as generated package
    data. The Rust engine and these readers cannot drift."""
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
def _shipped_verbs() -> frozenset:
    """One cached read of the plugin surface; `footnote_verbs` is the caller."""
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


def footnote_verbs() -> frozenset:
    """The shipped footnote verb roster: every ``skills/<name>/SKILL.md`` and
    every ``commands/<name>.md`` the plugin ships.

    Read from the shipped plugin surface, never a retyped literal: a literal
    copy goes stale the first time a verb ships, and the failure is silent -
    the new verb simply stops normalizing on codex and nothing notices.

    Empty on any resolution or read failure, which on the codex surface leaves
    every bare ``/verb`` literal - pass-through is the safe direction - while
    the plugin-qualified ``/fno:verb`` spelling keeps working, since its
    namespace alone proves it is a footnote verb.

    A good answer caches; an EMPTY one does not. Empty means the plugin
    surface did not RESOLVE, never that footnote ships no verbs, and the
    resolver reads an env hint first, so the next call can succeed. Caching
    the failure froze the degraded answer for the whole process, silently: a
    codex worker gets `/target`, which codex reads as prose, not a verb."""
    verbs = _shipped_verbs()
    if not verbs:
        _shipped_verbs.cache_clear()
    return verbs


footnote_verbs.cache_clear = _shipped_verbs.cache_clear  # type: ignore[attr-defined]


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
# harness also carries `slash_prefix` (the plugin namespace). The thread seat
# is not stored: it derives from `features.spawn` reading `native` (fno's own
# driver + unattended journey test, never a bare resume primitive). `bg`
# remains a one-release input alias.
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
        "permission_bypass", "resume", "autonomous_pane", "route_on_pane",
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
        if "/" + verb in native:
            return cmd
        # The dispatch verb is footnote's own: a STATIC fact. The roster read
        # below needs a resolvable plugin root, and an unresolvable one returns
        # an empty roster indistinguishable from "not ours", which rendered
        # `/target` for a codex worker as an ordinary pass-through. Not caching
        # that empty answer stops it freezing; this bypass makes it impossible.
        if first_word in _TARGET_FAMILY:
            return "$fno:" + verb + cmd[len(first_word):]
        if verb not in footnote_verbs():
            return cmd
        return "$fno:" + verb + cmd[len(first_word):]
    if surface == _SLASH and cmd.startswith("$fno:"):
        # Reverse rewrite (x-413d): the sigil says who WROTE the seed, never
        # which harness runs it. Swap the sigil, keep the namespace, and let
        # the /fno: handling below render it per surface.
        cmd = "/fno:" + cmd[len("$fno:"):]
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
        # A native verb of the harness stays literal: `/undo` on opencode is
        # opencode's own palette verb, and namespacing it would mint a phantom
        # `/fno:undo` plugin skill. Same `native_verbs` roster the codex-skill
        # branch reads; the claude/agy rows are inert here only because their
        # prefix is empty. The roster names verbs, so the guard reads the
        # first token of the remainder, never the message tail.
        native = {v for v in caps.get("native_verbs") or () if isinstance(v, str)}
        if "/" + verb.split(maxsplit=1)[0] in native:
            return cmd
        return "/" + prefix + verb
    return cmd


# zsh reads the `:t` in "$fno:target" as a modifier on the empty `$fno`, so the
# verb loses its first letter too. Measured: these letters vanish without an
# error, `s` fails loudly, and every other letter survives as `:verb`.
_ZSH_EATEN_LETTERS = frozenset("acelqrtu")


def lost_verb_refusal(message: str) -> Optional[str]:
    """The refusal for a payload whose ``$fno:`` prefix the calling shell ate,
    or None when the payload is intact.

    Inside double quotes a shell expands ``$fno`` to nothing before fno runs.
    bash leaves ``:target``; zsh leaves ``arget``. The worker reads either as
    prose, and the spawn still returns a live receipt, so the loss shows up
    an hour later as a worker that did not run the verb.

    The zsh shape is matched only for verbs longer than four letters: the
    short remainders (``dd``, ``aw``) are ordinary words."""
    verbs = set(footnote_verbs()) | {v[1:] for v in _TARGET_FAMILY_VERBS}
    lost = {":" + v: v for v in verbs}
    lost.update({v[1:]: v for v in verbs if len(v) > 4 and v[0] in _ZSH_EATEN_LETTERS})
    for token in message.split():
        verb = lost.get(token)
        if verb is not None:
            return (
                f"the payload has {token!r} where '$fno:{verb}' belongs. The shell "
                "expanded $fno to nothing inside double quotes, and zsh also drops "
                "the first letter of the verb. Single-quote the payload: "
                f"'$fno:{verb} ...'"
            )
    return None


def cannot_fire_refusal(message: str, harness: str) -> Optional[str]:
    """The refusal for a verb-shaped seed whose verb cannot expand, or None.

    An intact ``$fno:verb`` seed still lands as prose when the footnote plugin
    is not enabled in the codex home this machine resolves. ``missing`` and
    ``wrong-channel`` are the measured-absent states and refuse; an unreadable
    state (no codex CLI on PATH) fails open, because the spawn fails on its
    own there.
    """
    if harness != "codex" or not message.strip().startswith(("/", "$fno:")):
        return None
    from fno.setup.codex_plugin import CodexPluginError, inspect_freshness

    try:
        status = inspect_freshness().get("status")
    except (CodexPluginError, OSError, ValueError):
        return None
    if status in ("missing", "wrong-channel"):
        return (
            f"the seed invokes {message.strip().split()[0]!r} but the footnote "
            "plugin is not enabled for codex on this machine, so the verb would "
            "not fire and the worker would read the seed as prose. Install it "
            "with 'fno config plugin install codex' and spawn again."
        )
    return None


def verb_fired_marker(message: str) -> Optional[str]:
    """The command that proves a ``/fno:target <node>`` seed actually fired.

    The marker is the claim naming the spawned session as holder; a busy worker fired nothing.
    """
    first = message.strip().splitlines()[0].split()
    if len(first) < 2 or first[1].startswith(("-", "/", "$")):
        return None
    if first[0].lstrip("/$") != "fno:target":
        return None
    return f"fno agents claim status node:{first[1]}"


def render_seed(message: str, harness: str) -> str:
    """Prose verbatim; a verb-shaped seed gate-checked then normalized by the
    one shared fire-test predicate (``is_verb_seed``, x-413d)."""
    from fno.agents.spawn_defaults import is_verb_seed

    if not is_verb_seed(message):
        return message
    refusal = cannot_fire_refusal(message, harness)
    if refusal:
        raise DispatchResolveError(refusal)
    return normalize_command(message, harness)


def spawn_seed_receipt_fields(effective_message: str) -> dict[str, str]:
    """The receipt fields a delivered seed contributes; prose seeds get none.

    ``verb_fired`` is ``pending`` because a busy worker fired nothing; the
    marker names the command whose pass settles it.
    """
    fields = {"effective_message": effective_message, "verb_fired": "pending"}
    marker = verb_fired_marker(effective_message)
    if marker:
        fields["verb_marker"] = marker
    return fields


def spawn_seed_receipt_fragment(effective_message: Optional[str]) -> str:
    """The JSON fragment form of :func:`spawn_seed_receipt_fields`; "" for prose."""
    if effective_message is None:
        return ""
    return "".join(
        f", {json.dumps(key)}: {json.dumps(value)}"
        for key, value in spawn_seed_receipt_fields(effective_message).items()
    )


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


def spawn_state(harness: str) -> str:
    """The features dimension's spawn claim for ``harness``: ``native`` (fno's
    launch arm is wired and journey-proven), ``capable`` (real on the harness,
    no fno arm), ``absent``, or ``unmeasured``. A row without a features
    stanza reads ``unmeasured`` - the fail-closed answer, never a guess."""
    features = capabilities(harness).get("features") or {}
    claim = features.get("spawn") or {}
    return claim.get("state") or "unmeasured"


def thread_seatable(harness: str) -> bool:
    """Whether fno's thread lane exists for ``harness``: the features
    dimension's spawn claim reads ``native``. The seat is DERIVED, never
    stored - the routing boolean this answer replaced sat beside resume
    mechanics it had no relationship to and drifted (opencode measured
    native in its spawn row while its boolean still read false)."""
    return spawn_state(harness) == "native"


def thread_uncarried(
    harness: str,
    axes: dict[str, object],
    passthrough: list[str] | None,
) -> str | None:
    """The first launch flag this harness's thread lane cannot carry, or None.

    Reads the ``[harness.<name>.thread]`` carrier row, else the ``keeper``
    row. ``axes`` maps ``keeper_thread.LAUNCH_AXES`` axis names to set values;
    ``passthrough`` is the fenced ``--`` token list. A non-None answer
    demotes the spawn to the pane; it never refuses on the substrate.
    """
    caps = _BUNDLED_CAPS.get(harness) or {}
    arm = caps.get("thread") or caps.get("keeper")
    if not arm:
        return None
    carries = set(arm.get("carries") or [])
    from fno.agents.keeper_thread import LAUNCH_AXES

    for flag, axis in LAUNCH_AXES:
        if axis not in carries and axes.get(axis):
            return flag
    covered = set(arm.get("passthrough") or [])
    if "*" in covered:
        return None
    skip_value = False
    for token in passthrough or []:
        if skip_value:
            skip_value = False
            continue
        if token in covered:
            # A carried spelling consumes its value token too: `-c` and
            # `model_reasoning_effort=high` arrive as two fenced tokens.
            skip_value = True
        elif token.split("=", 1)[0] in covered:
            continue
        else:
            return token
    return None


def substrate_default(harness: str) -> str:
    """Per-harness default substrate: ``thread`` where the spawn claim reads
    ``native`` (a journey-proven launch seam), else ``headless``. Pane
    permission is independent from substrate preference."""
    return "thread" if thread_seatable(harness) else "headless"


def thread_lane(harness: str) -> str:
    """Which thread lane this harness needs, from the capability contract
    alone - never from a name list, so a new row lands in its lane with no
    code edit here.

    ``attach``  the harness owns the live session; a client re-attaches to it.
    ``keeper``  the harness persists a transcript only; fno must hold the pty.
    ``none``    no resume form at all, so no lane can be built.

    Two independent signals answer ``attach``: a declared ``interactive_attach``
    form (a CLI attach subcommand fno can shell out to), OR ``features.attach``
    reading ``native`` (fno can reach a live session some other way - a
    daemon-owned process, a keeper-hosted portal - even with no such
    subcommand). This split exists for a harness that ships no attach
    subcommand at all, yet has a real, working thread destination reached
    through the daemon-kept lane - a fact its own ``features.attach`` claim
    already records (x-df08). A row with no ``features.attach`` stanza at
    all reads as absent, never as a claim, so this never promotes a row
    silently.
    """
    caps = capabilities(harness)
    attach_claim = (caps.get("features") or {}).get("attach") or {}
    if attach_claim.get("state") == "native":
        return "attach"
    forms = caps["resume_strategy"]["forms"]
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


#: claude's own --permission-mode vocabulary, its --help being the authority
#: (x-8975); the CLI help and the doctor readout spell it from here.
CLAUDE_PERMISSION_MODES = frozenset(
    {"default", "acceptEdits", "auto", "dontAsk", "plan", "bypassPermissions"}
)

CLAUDE_PERMISSION_HELP = (
    "claude " + "|".join(sorted(CLAUDE_PERMISSION_MODES)) + " (exact passthrough)"
)

#: The full --permission-mode help text: every harness's answer vocabulary is
#: a harness_map question, so the whole option help lives beside the maps.
PERMISSION_MODE_HELP = (
    "Permission/approval mode forwarded to the provider (x-dfa4). "
    f"Provider-native values, fail-closed: {CLAUDE_PERMISSION_HELP}; "
    "gemini --approval-mode "
    "(or 'yolo'); codex a shortcut (full-auto|yolo) or <sandbox>:"
    "<approval> (e.g. workspace-write:on-request); opencode 'auto'; agy "
    "'skip'; cursor-agent 'force' or 'yolo'. An unmappable value errors "
    "before spawn. Mutually exclusive "
    "with --yolo. Honored on claude thread/headless (Rust or Python "
    "fallback); codex/gemini thread/headless one-shots reject it (use "
    "--substrate pane)."
)


_VALID_SUBSTRATES = ("thread", "headless", "pane")
_LEGACY_SUBSTRATE_ALIASES = {"bg": "thread"}
# US3: the built-in verb allowlist (config.dispatch.allowed_verbs overrides).
_DEFAULT_ALLOWED_VERBS = ("/target", "/think", "/blueprint")
# The env budget a brief must fit; 8 KB, measured in UTF-8 bytes (Locked
# Decision 9 / epic Boundaries). Oversized -> explicit error, never truncation.
_BRIEF_MAX_BYTES = 8192
# The default command is per-harness now (each harness's `dispatch_command` in
# _HARNESS_CAPS), not a single template - see the resolve builtin branch.


#: The verbs the x-ebd2 lifecycle table owns; anything else abstains.
_TARGET_FAMILY_VERBS = ("/target", "/blueprint")
#: Intake keys on difficulty (law d-834b6ff1); re-dispatch on the plan's rung.
_DIFFICULTY_ANSWERS = {"low": "/target", "medium": "/blueprint", "high": "/blueprint"}
_RUNG_ANSWERS = {
    "idea": "/blueprint",
    "design": "/blueprint",
    "ready": "/target",
    "in_progress": "/target",
    "in_review": "/target",
}


def resolve_effective_verb(
    *,
    verb: Optional[str] = None,
    difficulty: Optional[str] = None,
    plan_rung: Optional[str] = None,
    node_id: Optional[str] = None,
) -> tuple[Optional[str], str]:
    """The target/blueprint lifecycle conditional; full table:
    docs/architecture/backlog-graph-verb-contracts.md. Intake (rung "none"):
    difficulty decides. Re-dispatch: the plan rung decides. The stored
    ``verb`` reconciles through the table; out-of-family abstains to declared
    precedence. Returns ``(canonical_verb, decision)``; ``None`` = abstain.
    Raises :class:`DispatchResolveError` on a refusal rung, or planless
    without low/medium/high difficulty. ``plan_rung`` is a Rung value. The
    refusal leads with ``node_id`` when the caller holds one, so the subject
    of the failure is never read off a citation."""
    raw_verb = (verb or "").strip()
    if raw_verb.startswith("/fno:"):
        raw_verb = "/" + raw_verb[len("/fno:"):]
    if raw_verb and raw_verb not in _TARGET_FAMILY_VERBS:
        return None, f"verb=lifecycle(out-of-family {raw_verb}; declared precedence holds)"
    if plan_rung is None:
        return None, "verb=lifecycle(no-node-context)"
    rung = plan_rung.strip().lower()
    d = (difficulty or "").strip().lower()
    if rung == "none" and d in _DIFFICULTY_ANSWERS:
        answer = _DIFFICULTY_ANSWERS[d]
        note = f"verb=lifecycle(intake difficulty={d} -> {answer}"
    elif rung in _RUNG_ANSWERS:
        answer = _RUNG_ANSWERS[rung]
        note = f"verb=lifecycle(plan {rung} -> {answer}"
    else:
        who = f" for node {node_id}" if node_id else ""
        raise DispatchResolveError(
            f"dispatch verb cannot be derived{who}: plan rung {rung!r} with "
            f"difficulty {d!r} answers no lifecycle rung"
        )
    if raw_verb and raw_verb != answer:
        note += f"; stored dispatch_verb {raw_verb} reconciled"
    return answer, note + ")"


def resolve_dispatch(
    *,
    harness: Optional[str] = None,
    substrate: Optional[str] = None,
    node_id: Optional[str] = None,
    command: Optional[str] = None,
    verb: Optional[str] = None,
    difficulty: Optional[str] = None,
    plan_rung: Optional[str] = None,
    brief: Optional[str] = None,
    merge_posture: Optional[str] = None,
    trigger: str = "autonomous",
    settings: object = None,
    dispatch_cfg: Optional[Mapping[str, object]] = None,
) -> dict:
    """Map (config + context) -> the dispatch tuple. Pure; never spawns/claims.

    Full contract: docs/architecture/backlog-graph-verb-contracts.md. Field
    precedence (each independent): harness explicit > stage table >
    ``claude``; substrate explicit > config > per-harness default; command
    explicit > x-ebd2 lifecycle derivation > node ``verb`` (allowlist-checked;
    a graph field is a trust boundary) > ``config.dispatch.command`` >
    per-harness builtin. ``difficulty``/``plan_rung`` feed the lifecycle
    derivation (see :func:`resolve_effective_verb`), which runs BEFORE the
    stage-table read so ``agents.profiles.<derived-verb>`` drives the harness;
    an explicit command bypasses it (reconcile and the other explicit doors
    spell their own verb). ``brief`` rides ``env['TARGET_BRIEF']`` only, capped
    at 8 KB, never truncated. ``route`` is the stage table's vendor lane beside
    the harness ("" when unset), returned so a caller forwarding the harness
    can forward the vendor too. ``trigger`` is autonomous or attended (pane
    needs the capability). ``node_id`` substitutes the command's ``{id}``.
    ``merge_posture`` (x-8151): no-merge injects, allow overrides the config
    read (an explicit template is never edited), from-config reads the grant.

    Raises :class:`DispatchResolveError` on an unknown/refused harness, a
    missing substrate lane, an unsupported autonomous pane, an unknown trigger
    or substrate, an out-of-allowlist verb, an oversized brief, an empty or
    unsubstituted command, or an unanswerable node lifecycle.
    ``dispatch_cfg`` overrides the config read (for tests)."""
    decision: list[str] = []
    # The lifecycle rung derives the effective verb BEFORE the config read so
    # the stage table resolves the DERIVED verb's profile row.
    lifecycle_verb: Optional[str] = None
    if command is None or not command.strip():
        lifecycle_verb, lifecycle_note = resolve_effective_verb(
            verb=verb, difficulty=difficulty, plan_rung=plan_rung, node_id=node_id
        )
        decision.append(lifecycle_note)
    cfg = (
        dict(dispatch_cfg)
        if dispatch_cfg is not None
        else _load_dispatch_cfg(settings, verb=lifecycle_verb or verb)
    )
    # The verb lane vendor rides the same stage-table row the harness does:
    # an autonomous dispatch that names the harness but not the route sends a
    # routed model to the default endpoint (x-14d4: HTTP 404 model_not_found).
    route_value = str(cfg.get("route", "") or "")
    if route_value:
        decision.append(f"route=config({route_value})")
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
    if chosen_substrate == "thread" and not thread_seatable(chosen_harness):
        raise DispatchResolveError(
            f"substrate 'thread' is unsupported on harness {chosen_harness!r}: "
            f"its features.spawn state reads {spawn_state(chosen_harness)!r}, "
            f"so fno has not built the {thread_lane(chosen_harness)} lane yet "
            f"(bg is a deprecated alias); use 'headless'"
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
            f"{', '.join(h for h in known_harnesses() if thread_seatable(h))})"
        )

    # 3. command template. Precedence: explicit --command > lifecycle > node
    # verb (allowlist-checked; a graph field is a trust boundary) > config
    # template > per-harness builtin. A derived /target renders through the
    # SAME builtin rungs (suppress the raw verb and fall through); a derived
    # /blueprint renders its own verb: the target template is target-phase.
    # A registry verb sets skip_normalize and, with takes_node_id=false,
    # verb_declares_no_id (both consumed below).
    skip_normalize = False
    verb_declares_no_id = False
    derived_blueprint = lifecycle_verb == "/blueprint"
    if lifecycle_verb == "/target":
        verb = None
    if command is not None and command.strip():
        template = command.strip()
        decision.append("command=explicit")
    elif derived_blueprint:
        template = f"{lifecycle_verb} {{id}}"
        decision.append(f"command=derived({lifecycle_verb})")
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
        from fno.config import resolvable_verbs
        from fno.review_capability import resolve_skill_presence

        _vr = cfg.get("verb_registry")
        registry = resolvable_verbs(_vr if isinstance(_vr, Mapping) else None, allowed)
        descriptor = registry.get(chosen_verb)
        if chosen_verb not in allowed and descriptor is None:
            raise DispatchResolveError(
                f"dispatch verb {chosen_verb!r} is in neither the allowlist "
                f"({', '.join(allowed)}) nor config.dispatch.verb_registry "
                f"({', '.join(sorted(registry)) or 'empty'}); extend one of them"
            )
        if descriptor is not None:
            # Registry verb: descriptor carries spelling, capability, claim.
            if descriptor.requires == "skill":
                # First token only (the verb may carry args; same contract as
                # the reviewer probe); malformed falls back to the key.
                head = descriptor.invocation.split()
                skill_name = (head[0] if head else chosen_verb).lstrip("/").split(":")[-1]
                status, reason = resolve_skill_presence(
                    skill_name, chosen_harness, context="config.dispatch.verb_registry"
                )
                if status == "unavailable":
                    raise DispatchResolveError(reason)
            if descriptor.invocations and chosen_harness not in descriptor.invocations:
                raise DispatchResolveError(
                    f"dispatch verb {chosen_verb!r} is not declared on harness "
                    f"{chosen_harness!r}; config.dispatch.verb_registry declares "
                    f"it on: {', '.join(sorted(descriptor.invocations))}"
                )
            template = (descriptor.invocations or {}).get(chosen_harness, descriptor.invocation)
            if descriptor.takes_node_id:
                template = f"{template} {{id}}"
            else:
                verb_declares_no_id = True
            # The descriptor already spells the verb natively; normalizing
            # would mint a phantom `$fno:` skill from it.
            skip_normalize = True
            decision.append(
                f"command=registry-verb({chosen_verb}, asserts={descriptor.asserts})"
            )
        else:
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
    normalized_cmd = template if skip_normalize else normalize_command(template, chosen_harness)
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
    # `{id}` must appear at least once; a template may reference it more than
    # once. A registry verb declaring takes_node_id=false is exempt: ignoring
    # the id is declared, not a dropped substitution.
    if node_id and "{id}" in template:
        resolved_command = template.replace("{id}", node_id.strip())
        decision.append(f"command=substituted({resolved_command})")
    elif node_id and "{id}" not in template and not verb_declares_no_id:
        raise DispatchResolveError(
            f"command template {template!r} must contain '{{id}}' at least "
            f"once for substitution"
        )
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
        "route": route_value,
        "command": resolved_command,
        # x-ebd2: the lifecycle-derived canonical verb, or None when the table
        # abstained (bare resolve, explicit command, out-of-family declared
        # verb) - the raw source state stays in the caller's verb_source.
        "verb": lifecycle_verb,
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
        "thread": thread_seatable(chosen_harness),
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
    from fno.dispatch_flags import configured_dispatch_harness, configured_dispatch_route

    harness_value, harness_note = configured_dispatch_harness(settings, verb=verb or "target")
    route_value = configured_dispatch_route(settings, verb=verb or "target")
    d = getattr(settings, "dispatch", None)
    # The grant lives in config.auto_merge, NOT under dispatch (x-4be1), so it
    # is read before the dispatch-block gate: a settings object carrying an
    # auto_merge block but no dispatch overlay still resolves its grant, and a
    # stub without either degrades to no-grant.
    from fno.config.grant import auto_merge_grant

    grant = auto_merge_grant(settings)
    if d is None:
        return {
            "harness": harness_value or "",
            "harness_note": harness_note or "",
            "route": route_value,
            "auto_merge": grant,
        }

    def _text(name: str) -> str:
        return (getattr(d, name, None) or "").strip()

    try:
        return {
            "harness": harness_value or "",
            "harness_note": harness_note or "",
            "route": route_value,
            "substrate": _text("substrate"),
            "command": _text("command"),
            "allowed_verbs": list(getattr(d, "allowed_verbs", None) or []),
            "verb_registry": dict(getattr(d, "verb_registry", None) or {}),
            # Strict literal compare, not truthiness: only the "dispatch"
            # grant grants (a stray truthy value or a stub block never does).
            "auto_merge": grant,
        }
    except Exception:  # noqa: BLE001
        return {}
