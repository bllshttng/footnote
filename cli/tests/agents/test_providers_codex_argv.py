"""Pure-function tests for codex argv builders.

Locks in the CLI surface we pass to ``codex exec`` / ``codex exec resume``
so a future refactor cannot silently change the codex invocation contract.

Plan ACs covered:
- 'inject_from_name(prompt, "orchestrator") returns "[from: orchestrator]\\n\\n<prompt>"'
- 'sandbox_flag(yolo=False) returns ["--sandbox", "workspace-write"]'
- 'sandbox_flag(yolo=True) returns ["--dangerously-bypass-approvals-and-sandbox"]'
- 'Mutually exclusive in any argv builder output'
- 'from_name validation reuses the US2 validator (no fresh regex)'
"""
from __future__ import annotations

import re

from fno.agents.providers import codex as codex_mod


# ---------------------------------------------------------------------------
# inject_from_name
# ---------------------------------------------------------------------------


def test_inject_from_name_bracket_prefix():
    out = codex_mod.inject_from_name("write the schema", "orchestrator-main")
    assert out == "[from: orchestrator-main]\n\nwrite the schema"


def test_inject_from_name_default_fno():
    # The US2 default from_name is "fno"; this function is a pure
    # string operation, so the caller passes whatever they validated.
    out = codex_mod.inject_from_name("msg", "fno")
    assert out.startswith("[from: fno]\n\n")


def test_inject_from_name_all_allowed_characters_round_trip():
    # AC4-EDGE: ABI-orchestrator_main.1 passes through verbatim.
    name = "ABI-orchestrator_main.1"
    out = codex_mod.inject_from_name("do it", name)
    assert f"[from: {name}]" in out
    assert out.endswith("do it")


def test_inject_from_name_preserves_multi_line_prompt():
    prompt = "line one\nline two\nline three"
    out = codex_mod.inject_from_name(prompt, "fno")
    assert out == f"[from: fno]\n\n{prompt}"


# ---------------------------------------------------------------------------
# sandbox_flag (create path)
# ---------------------------------------------------------------------------


def test_sandbox_flag_default_is_bounded():
    # Sandbox tokens only (workspace sandbox); approval is a separate global
    # flag emitted before `exec` - see approval_flag.
    assert codex_mod.sandbox_flag(yolo=False) == ["--sandbox", "workspace-write"]


def test_sandbox_flag_yolo_is_dangerous_bypass():
    assert codex_mod.sandbox_flag(yolo=True) == [
        "--dangerously-bypass-approvals-and-sandbox"
    ]


def test_sandbox_flag_never_carries_approval_token():
    # --ask-for-approval is a GLOBAL flag; sandbox_flag tokens are spliced
    # AFTER `exec`, where codex rejects --ask-for-approval. It must never leak
    # into sandbox_flag's output. (Regression: pr704 codex spawn abort.)
    for yolo in (True, False):
        assert "--ask-for-approval" not in codex_mod.sandbox_flag(yolo)


def test_sandbox_flag_yolo_and_default_never_overlap():
    # Domain pitfall: --sandbox and --dangerously-bypass-... are mutually
    # exclusive. The function MUST NOT emit both even by accident.
    yolo = codex_mod.sandbox_flag(yolo=True)
    safe = codex_mod.sandbox_flag(yolo=False)
    assert "--sandbox" not in yolo
    assert "workspace-write" not in yolo
    assert "--dangerously-bypass-approvals-and-sandbox" not in safe


# ---------------------------------------------------------------------------
# approval_flag (create path — GLOBAL flag, emitted before `exec`)
# ---------------------------------------------------------------------------


def test_approval_flag_default_is_never_prompt():
    assert codex_mod.approval_flag(yolo=False) == ["--ask-for-approval", "never"]


def test_approval_flag_yolo_is_empty():
    # The bypass flag from sandbox_flag already disables approval; emitting
    # --ask-for-approval alongside it would be redundant.
    assert codex_mod.approval_flag(yolo=True) == []


# ---------------------------------------------------------------------------
# Bounded-posture amendment (US1/US3). The headless autonomous exec lane
# (create/resume) defaults to the BOUNDED posture (sandboxed + never-prompt),
# so a headless codex worker cannot hang AND keeps the workspace sandbox. Full
# yolo (unsandboxed bypass) is reachable only via the explicit yolo opt-in
# (the `yolo` bareword) or config.agents.codex.headless_yolo: true.
# ---------------------------------------------------------------------------

# sandbox_flag output (approval is asserted separately via approval_flag).
_BOUNDED = ["--sandbox", "workspace-write"]
_FULL_YOLO = ["--dangerously-bypass-approvals-and-sandbox"]


def _hermetic_config(tmp_path, monkeypatch, content: str = "schema_version: 1\n") -> None:
    from fno import config as config_mod

    f = tmp_path / "settings.yaml"
    f.write_text(content, encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(f))
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]


def test_headless_default_is_bounded():
    """AC1-HP: a headless codex exec (not yolo) is BOUNDED - sandbox + never
    prompt - NOT the full bypass."""
    eff = codex_mod._effective_yolo(yolo=False, headless_yolo=False)
    assert codex_mod.sandbox_flag(eff) == _BOUNDED


def test_config_full_yolo_opt_in_yields_bypass():
    """headless_yolo=true opts into the full unsandboxed bypass."""
    eff = codex_mod._effective_yolo(yolo=False, headless_yolo=True)
    assert codex_mod.sandbox_flag(eff) == _FULL_YOLO


def test_explicit_yolo_forces_bypass():
    """AC1-EDGE: the explicit yolo bareword forces the full bypass."""
    eff = codex_mod._effective_yolo(yolo=True, headless_yolo=False)
    assert codex_mod.sandbox_flag(eff) == _FULL_YOLO


def test_headless_default_resolves_from_config_to_bounded(tmp_path, monkeypatch):
    """headless_yolo=None reads config.agents.codex.headless_yolo. Default
    config (no agents block) -> BOUNDED."""
    _hermetic_config(tmp_path, monkeypatch)
    eff = codex_mod._effective_yolo(yolo=False, headless_yolo=None)
    assert codex_mod.sandbox_flag(eff) == _BOUNDED


def test_headless_config_full_yolo_resolves_to_bypass(tmp_path, monkeypatch):
    _hermetic_config(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  agents:\n    codex:\n      headless_yolo: true\n",
    )
    eff = codex_mod._effective_yolo(yolo=False, headless_yolo=None)
    assert codex_mod.sandbox_flag(eff) == _FULL_YOLO


# ---------------------------------------------------------------------------
# sandbox_flag_resume (resume path — restricted surface)
# ---------------------------------------------------------------------------


def test_sandbox_flag_resume_default_is_empty():
    # codex exec resume inherits sandbox from the original session;
    # without --yolo we emit nothing so the inherited mode applies.
    assert codex_mod.sandbox_flag_resume(yolo=False) == []


def test_sandbox_flag_resume_yolo_only_emits_dangerous_bypass():
    assert codex_mod.sandbox_flag_resume(yolo=True) == [
        "--dangerously-bypass-approvals-and-sandbox"
    ]


def test_sandbox_flag_resume_never_emits_sandbox_flag():
    # `codex exec resume` does not accept --sandbox; ensure it is never
    # emitted from this helper under any input.
    for yolo in (True, False):
        assert "--sandbox" not in codex_mod.sandbox_flag_resume(yolo)


# ---------------------------------------------------------------------------
# Event-type constants locked at value (regression test for Locked Decision 13)
# ---------------------------------------------------------------------------


def test_event_type_constants_pinned():
    # If codex's vocabulary moves, the smoke test (Wave 2.2) is the
    # discriminator; pinning here forces a deliberate update via the
    # smoke script's captured fixture rather than a guess in this module.
    assert codex_mod._EVENT_TYPES == {
        "session": "thread.started",
        "complete": "turn.completed",
        "item_envelope": "item.completed",
    }
    assert codex_mod._ITEM_TYPES == {
        "message": "agent_message",
        "error": "error",
    }


def test_no_event_type_literal_strings_outside_constants_block():
    """Regression guard: parser must reference _EVENT_TYPES by key.

    Acceptance criterion 1.0: 'The parser references _EVENT_TYPES by key,
    never the literal string'. We walk the AST and assert the captured
    values only appear as Constant string nodes inside the _EVENT_TYPES
    and _ITEM_TYPES dict assignments. Docstrings and comments are exempt
    (docstrings are intentional natural-language references to the codex
    vocabulary; the test guards control-flow code paths).
    """
    import ast
    import pathlib

    source = pathlib.Path(codex_mod.__file__).read_text(encoding="utf-8")
    tree = ast.parse(source)

    sentinel_values = {
        "thread.started",
        "turn.completed",
        "item.completed",
        "agent_message",
    }
    allowed_dict_names = {"_EVENT_TYPES", "_ITEM_TYPES"}

    allowed_nodes: set[int] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Assign):
            target_names = {
                t.id for t in node.targets if isinstance(t, ast.Name)
            }
            if target_names & allowed_dict_names and isinstance(node.value, ast.Dict):
                for v in node.value.values:
                    if isinstance(v, ast.Constant):
                        allowed_nodes.add(id(v))

    # Collect every string Constant whose value is one of the sentinels
    # AND that is not part of a module/function/class docstring (handled
    # by ast.get_docstring conventions: docstrings live as the first
    # Expr.value of a body, and we exempt them).
    docstring_nodes: set[int] = set()
    for module_node in ast.walk(tree):
        if isinstance(
            module_node, (ast.Module, ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)
        ):
            body = getattr(module_node, "body", [])
            if (
                body
                and isinstance(body[0], ast.Expr)
                and isinstance(body[0].value, ast.Constant)
                and isinstance(body[0].value.value, str)
            ):
                docstring_nodes.add(id(body[0].value))

    offenders: list[tuple[int, str]] = []
    for node in ast.walk(tree):
        if (
            isinstance(node, ast.Constant)
            and isinstance(node.value, str)
            and node.value in sentinel_values
            and id(node) not in allowed_nodes
            and id(node) not in docstring_nodes
        ):
            offenders.append((node.lineno, node.value))

    assert not offenders, (
        f"event-type literal(s) found outside the constants block: "
        f"{offenders}; reference _EVENT_TYPES / _ITEM_TYPES by key instead"
    )
