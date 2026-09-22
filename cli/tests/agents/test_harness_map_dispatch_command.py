"""Per-harness dispatch_command resolution (x-567d).

Each harness resolves to the right worker command: a native skill invocation
where one is verified (claude/agy ``/target``, opencode ``/fno:target``, codex
``$fno:target``). gemini is deprecated (successor: agy) and its dispatch lane is
a loud refusal - no prose brief (x-de43).
"""
from __future__ import annotations

import pytest

from fno.agents.harness_map import (
    MAP_VERSION,
    DispatchResolveError,
    capabilities,
    dispatch_command,
    normalize_command,
    resolve_dispatch,
)


@pytest.fixture(autouse=True)
def _installed_loop_extensions(monkeypatch):
    """Command-normalization tests are not about the loop gate's install
    state, which is machine environment: the gate itself is exercised in
    test_harness_loop_participation.py."""
    import fno.agents.harness_map as harness_map

    monkeypatch.setattr(harness_map, "_loop_extension_installed", lambda h: True)


@pytest.mark.parametrize(
    "harness,expected_prefix",
    [
        ("claude", "/target --no-merge "),
        ("codex", "$fno:target --no-merge "),
        ("agy", "/target --no-merge "),
    ],
)
def test_skill_invoking_harnesses_get_native_command(harness, expected_prefix):
    out = resolve_dispatch(harness=harness, node_id="x-abcd")
    assert out["command"] == f"{expected_prefix}x-abcd"


def test_opencode_gets_native_fno_slash_command():
    # opencode's fno plugin expands `/fno:verb` (palette + `run --command`), so
    # dispatch renders the native slash form, not a prose brief (x-de43).
    out = resolve_dispatch(harness="opencode", node_id="x-abcd")
    assert out["command"] == "/fno:target --no-merge x-abcd"


def test_gemini_dispatch_refused_naming_agy():
    # gemini is deprecated (successor: agy); its dispatch lane is a loud refusal.
    with pytest.raises(DispatchResolveError, match="agy"):
        resolve_dispatch(harness="gemini", node_id="x-abcd")


def test_config_command_overrides_the_per_harness_builtin():
    out = resolve_dispatch(
        harness="codex",
        node_id="x-abcd",
        dispatch_cfg={"command": "$fno:execute {id}"},
    )
    assert out["command"] == "$fno:execute x-abcd"


def test_explicit_command_wins_over_config_and_builtin():
    # The explicit slash template is canonical claude syntax, normalized on the
    # chosen harness (x-f0e2). Precedence is unchanged - explicit still beats
    # the config `$fno:execute`. `custom` names no shipped footnote verb, so
    # the codex normalizer leaves the token literal instead of capturing it
    # into a phantom `$fno:custom` skill.
    out = resolve_dispatch(
        harness="codex",
        node_id="x-abcd",
        command="/custom {id}",
        dispatch_cfg={"command": "$fno:execute {id}"},
    )
    assert out["command"] == "/custom x-abcd"


def test_qualified_dispatch_verb_canonicalizes_before_the_allowlist():
    # US7 review: the court contract sets `--dispatch-verb /fno:target` (every
    # dispatched verb is plugin-qualified). The bare-only allowlist must not
    # refuse it - it canonicalizes to `/target`, then renders per-harness.
    out = resolve_dispatch(harness="claude", node_id="x-abcd", verb="/fno:target")
    assert out["command"] == "/target x-abcd"
    # opencode's surface re-adds the /fno: prefix at render.
    out_oc = resolve_dispatch(harness="opencode", node_id="x-abcd", verb="/fno:target")
    assert out_oc["command"] == "/fno:target x-abcd"
    # /fno:think canonicalizes the same way.
    out_think = resolve_dispatch(harness="claude", node_id="x-abcd", verb="/fno:think")
    assert out_think["command"] == "/think x-abcd"
    # a bare verb still works unchanged.
    assert resolve_dispatch(harness="claude", node_id="x-abcd", verb="/target")["command"] == "/target x-abcd"


def test_template_without_id_is_rejected():
    with pytest.raises(DispatchResolveError):
        resolve_dispatch(harness="claude", node_id="x-abcd", command="no placeholder here")


def test_map_version_bumped_for_dispatch_command():
    # A consumer asserting the shape it was written against must see the bump.
    assert MAP_VERSION >= 3
    assert resolve_dispatch(harness="claude")["map_version"] == MAP_VERSION


# --- autonomous pane capabilities ----------------------------------------- #


def test_codex_autonomous_pane_is_capability_allowed_with_thread_default():
    explicit = resolve_dispatch(
        harness="codex",
        substrate="pane",
        node_id="x-abcd",
        trigger="autonomous",
    )

    assert capabilities("codex")["autonomous_pane"] is True
    assert explicit["substrate"] == "pane"
    assert resolve_dispatch(harness="codex", node_id="x-abcd")["substrate"] == "thread"


@pytest.mark.parametrize("harness", ["claude", "agy", "opencode"])
def test_unverified_harness_autonomous_pane_fails_closed(harness):
    assert capabilities(harness)["autonomous_pane"] is False

    with pytest.raises(
        DispatchResolveError,
        match=rf"harness {harness!r}.*autonomous_pane",
    ):
        resolve_dispatch(
            harness=harness,
            substrate="pane",
            node_id="x-abcd",
            trigger="autonomous",
        )


def test_missing_autonomous_pane_capability_fails_closed(monkeypatch):
    import fno.agents.harness_map as harness_map

    monkeypatch.delitem(harness_map._HARNESS_CAPS["opencode"], "autonomous_pane")

    with pytest.raises(
        DispatchResolveError,
        match=r"harness 'opencode'.*autonomous_pane",
    ):
        resolve_dispatch(
            harness="opencode",
            substrate="pane",
            node_id="x-abcd",
            trigger="autonomous",
        )


def test_malformed_trigger_fails_closed_on_capability_enabled_pane():
    with pytest.raises(DispatchResolveError, match="unknown dispatch trigger"):
        resolve_dispatch(
            harness="codex",
            substrate="pane",
            node_id="x-abcd",
            trigger="autonamous",
        )


def test_opencode_bg_resolves_on_the_spawn_claim_not_the_pane_bit():
    """opencode's thread seat comes from its spawn claim (native, measured
    2026-09-03), never from a pane capability: its autonomous_pane bit is
    false while the bg alias still resolves. A capability on one substrate
    must not leak into another."""
    assert capabilities("opencode")["autonomous_pane"] is False
    out = resolve_dispatch(
        harness="opencode",
        substrate="bg",
        node_id="x-abcd",
        trigger="autonomous",
    )
    assert out["substrate"] == "thread"


def test_codex_thread_capability_allows_bg_alias():
    """Codex's live six-step journey earns the deprecated bg alias."""
    out = resolve_dispatch(
        harness="codex",
        substrate="bg",
        node_id="x-abcd",
        trigger="autonomous",
    )
    assert out["substrate"] == "thread"
    assert out["thread"] is True


# --- the normalizer (x-a5e4) ------------------------------------------------ #


@pytest.mark.parametrize(
    "harness,expected",
    [
        ("claude", "/target --no-merge {id}"),
        ("agy", "/target --no-merge {id}"),
        ("codex", "$fno:target --no-merge {id}"),
    ],
)
def test_normalize_command_slash_and_codex(harness, expected):
    assert normalize_command("/target --no-merge {id}", harness) == expected


def test_normalize_command_opencode_namespaces():
    # opencode: `/verb` -> `/fno:verb` (plugin palette + `run --command`).
    assert normalize_command("/target --no-merge {id}", "opencode") == "/fno:target --no-merge {id}"


def test_normalize_command_gemini_refused():
    with pytest.raises(DispatchResolveError, match="agy"):
        normalize_command("/target --no-merge {id}", "gemini")


@pytest.mark.parametrize(
    "verb_cmd,expected",
    [
        ("/blueprint {id}", "$fno:blueprint {id}"),
        ("/pr create", "$fno:pr create"),
        ("/think {id}", "$fno:think {id}"),
    ],
)
def test_normalize_command_is_verb_agnostic_for_codex(verb_cmd, expected):
    # ANY footnote /verb -> $fno:verb on codex, not just /target.
    assert normalize_command(verb_cmd, "codex") == expected


def test_dispatch_command_builtin_matches_normalize():
    # The builtin is exactly the normalize of the canonical autonomous command.
    # gemini excluded: it refuses (test_normalize_command_gemini_refused).
    for h in ("claude", "codex", "agy", "opencode"):
        assert dispatch_command(h) == normalize_command("/target --no-merge {id}", h)


def test_command_surface_is_reported():
    assert resolve_dispatch(harness="codex")["command_surface"] == "codex-skill"
    assert resolve_dispatch(harness="claude")["command_surface"] == "slash"
    assert resolve_dispatch(harness="opencode")["command_surface"] == "slash"


# --- the verb-path fix (the codex P1 the handoff names) --------------------- #
# A node's `dispatch_verb=/target` must be NORMALIZED per-harness, not left as
# claude-syntax `/target` for every harness (which handed codex a slash command
# it cannot run).


def test_verb_path_normalizes_to_codex_skill():
    out = resolve_dispatch(harness="codex", node_id="x-abcd", verb="/target")
    assert out["command"] == "$fno:target x-abcd"


@pytest.mark.parametrize("harness", ["claude", "agy"])
def test_verb_path_keeps_slash_for_slash_harnesses(harness):
    out = resolve_dispatch(harness=harness, node_id="x-abcd", verb="/target")
    assert out["command"] == "/target x-abcd"


def test_verb_path_normalizes_to_opencode_slash():
    out = resolve_dispatch(harness="opencode", node_id="x-abcd", verb="/target")
    assert out["command"] == "/fno:target x-abcd"


def test_opencode_renders_any_verb():
    # opencode's single prefix-swap renders ANY verb, not just /target.
    out = resolve_dispatch(harness="opencode", node_id="x-abcd", verb="/think")
    assert out["command"] == "/fno:think x-abcd"


def test_gemini_verb_path_refused():
    with pytest.raises(DispatchResolveError, match="agy"):
        resolve_dispatch(harness="gemini", node_id="x-abcd", verb="/target")


def test_normalize_command_opencode_renders_any_verb():
    assert normalize_command("/think {id}", "opencode") == "/fno:think {id}"
    assert normalize_command("/blueprint quick x", "opencode") == "/fno:blueprint quick x"


# ---------------------------------------------------------------------------
# x-8e59: the builtin rung reads config.dispatch.auto_merge
#
# x-4391 shipped the key to 2 of 3 dispatch paths. The builtin here was the
# deaf one, so an operator who set the key still got `/target --no-merge <id>`,
# a manifest frozen at auto_merge_approved: false, and a refused `fno do pr merge`.
# The two callers that honored it did so by passing an explicit command around
# the builtin rather than fixing it.
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "harness,expected",
    [
        ("claude", "/target x-abcd"),
        ("agy", "/target x-abcd"),
        ("codex", "$fno:target x-abcd"),
        ("opencode", "/fno:target x-abcd"),
    ],
)
def test_builtin_drops_no_merge_when_auto_merge_configured(harness, expected):
    out = resolve_dispatch(
        harness=harness, node_id="x-abcd", dispatch_cfg={"auto_merge": True}
    )
    assert out["command"] == expected


@pytest.mark.parametrize("harness", ["claude", "codex", "opencode", "agy"])
def test_builtin_defaults_to_no_merge(harness):
    """No key, no grant. The default posture is unchanged for a fresh install."""
    out = resolve_dispatch(harness=harness, node_id="x-abcd", dispatch_cfg={})
    assert " --no-merge " in out["command"]


@pytest.mark.parametrize("bad", ["true", "True", 1, [1], object()])
def test_non_boolean_auto_merge_never_grants_merge(bad):
    """Granting merge authority is the irreversible direction, so the read is a
    strict identity check. Every one of these is truthy; none is a grant."""
    out = resolve_dispatch(
        harness="claude", node_id="x-abcd", dispatch_cfg={"auto_merge": bad}
    )
    assert out["command"] == "/target --no-merge x-abcd"


def test_unreadable_config_fails_safe_to_no_merge(monkeypatch):
    import fno.config as _config

    def _boom(*a, **k):
        raise RuntimeError("config is a smoking crater")

    monkeypatch.setattr(_config, "load_settings", _boom)
    out = resolve_dispatch(harness="claude", node_id="x-abcd")
    assert out["command"] == "/target --no-merge x-abcd"


def test_auto_merge_does_not_touch_an_explicit_command():
    """The posture applies to the builtin only: an explicit command already says
    what to run, and rewriting a caller's own template would be the surprise."""
    out = resolve_dispatch(
        harness="claude",
        node_id="x-abcd",
        command="/target --no-merge --reconcile /tmp/m.md {id}",
        dispatch_cfg={"auto_merge": True},
    )
    assert out["command"] == "/target --no-merge --reconcile /tmp/m.md x-abcd"


def test_auto_merge_does_not_touch_the_verb_rung():
    out = resolve_dispatch(
        harness="claude", node_id="x-abcd", verb="/think",
        dispatch_cfg={"auto_merge": True},
    )
    assert out["command"] == "/think x-abcd"


def test_decision_receipt_names_the_merge_posture():
    """The receipt is how an operator confirms the key was read at all - the
    bug this closes was invisible precisely because nothing said so."""
    allow = resolve_dispatch(
        harness="claude", node_id="x-abcd", dispatch_cfg={"auto_merge": True}
    )
    deny = resolve_dispatch(harness="claude", node_id="x-abcd", dispatch_cfg={})
    assert "command=builtin(merge)" in allow["decision"]
    assert "command=builtin(no-merge)" in deny["decision"]


def test_partial_settings_object_does_not_drop_auto_merge():
    """A settings stub carrying only `.auto_merge.grant` must still yield it.

    Field access used to be one try block over `d.harness`/`d.substrate`/
    `d.command`, so a settings object missing any one of them raised and threw
    the WHOLE dict away - one absent key silently disabling every other. That is
    the shape of the bug being fixed, so it gets its own test. x-4be1: the
    grant lives OUTSIDE the dispatch block, so a stub with an auto_merge block
    and no dispatch overlay still resolves the grant."""
    import types

    from fno.agents.harness_map import _load_dispatch_cfg

    stub = types.SimpleNamespace(
        auto_merge=types.SimpleNamespace(grant="dispatch")
    )
    assert _load_dispatch_cfg(stub)["auto_merge"] is True


def test_settings_without_dispatch_section_yields_empty_cfg():
    """x-4be1: no dispatch overlay and no grant -> only the (no-)grant key plus
    the harness fold, both EMPTY: the harness rung is unset so the builtin
    claude rung runs, and no shadowing note exists."""
    import types

    from fno.agents.harness_map import _load_dispatch_cfg

    assert _load_dispatch_cfg(types.SimpleNamespace()) == {
        "auto_merge": False,
        "harness": "",
        "harness_note": "",
        "route": "",
    }


@pytest.mark.parametrize("allow,expected", [(True, "/target {id}"), (False, "/target --no-merge {id}")])
def test_dispatch_command_posture_argument(allow, expected):
    assert dispatch_command("claude", allow_merge=allow) == expected


def test_dispatch_command_defaults_to_no_merge():
    assert dispatch_command("claude") == "/target --no-merge {id}"


# ---------------------------------------------------------------------------
# x-ebd2: the lifecycle rung. Law d-834b6ff1: difficulty decides at intake
# (no plan, no status to read); the plan's rung decides at re-dispatch. The
# stored dispatch_verb is audit input, never truth.
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "harness,expected",
    [
        ("claude", "/target --no-merge x-abcd"),
        ("codex", "$fno:target --no-merge x-abcd"),
        ("opencode", "/fno:target --no-merge x-abcd"),
    ],
)
def test_intake_lifecycle_answer_resolves_target_and_rides_the_note(harness, expected):
    # The table lives in backlog_ready.rs; the resolver is its client and the
    # note rides the decision trail verbatim.
    out = resolve_dispatch(
        harness=harness,
        node_id="x-abcd",
        lifecycle=("/target", "verb=lifecycle(intake difficulty=low -> /target)"),
    )
    assert out["command"] == expected
    assert out["verb"] == "/target"
    assert any("intake difficulty=low" in d for d in out["decision"])


@pytest.mark.parametrize(
    "answer,note",
    [
        ("/blueprint", "verb=lifecycle(intake difficulty=high -> /blueprint)"),
        ("/blueprint", "verb=lifecycle(plan ready not a blueprint -> /blueprint)"),
    ],
)
def test_lifecycle_blueprint_answer_renders_blueprint(answer, note):
    out = resolve_dispatch(harness="claude", node_id="x-abcd", lifecycle=(answer, note))
    assert out["command"] == "/blueprint x-abcd"
    assert out["verb"] == answer
    assert any(note in d for d in out["decision"])


@pytest.mark.parametrize(
    "harness,expected",
    [("codex", "$fno:blueprint x-abcd"), ("opencode", "/fno:blueprint x-abcd")],
)
def test_derived_blueprint_renders_harness_native(harness, expected):
    out = resolve_dispatch(
        harness=harness,
        node_id="x-abcd",
        lifecycle=("/blueprint", "verb=lifecycle(intake difficulty=medium -> /blueprint)"),
    )
    assert out["command"] == expected


def test_redispatch_design_rung_declared_target_wins_naming_lifecycle_answer():
    # A declared verb is never silently reconciled away. The decision trail
    # still names what the lifecycle would have answered.
    out = resolve_dispatch(
        harness="claude",
        node_id="x-abcd",
        verb="/fno:target",
        lifecycle=(
            "/target",
            "verb=declared(/target; lifecycle answers /blueprint: plan design)",
        ),
    )
    assert out["command"] == "/target --no-merge x-abcd"
    assert out["verb"] == "/target"
    assert any(
        "verb=declared(/target; lifecycle answers /blueprint: plan design)" in d
        for d in out["decision"]
    )


def test_redispatch_build_rung_declared_blueprint_wins():
    out = resolve_dispatch(
        harness="claude",
        node_id="x-abcd",
        verb="/blueprint",
        lifecycle=(
            "/blueprint",
            "verb=declared(/blueprint; lifecycle answers /target: plan ready)",
        ),
    )
    assert out["command"] == "/blueprint x-abcd"
    assert out["verb"] == "/blueprint"
    assert any(
        "verb=declared(/blueprint; lifecycle answers /target: plan ready)" in d
        for d in out["decision"]
    )


def _stub_door(monkeypatch, answers):
    """Stub the store door: ``answers`` is the reply row list, or a callable
    replacing ``request_effective_verb`` itself (a raising door)."""
    import fno.graph.store as store

    if callable(answers):
        monkeypatch.setattr(store, "request_effective_verb", answers)
    else:
        monkeypatch.setattr(store, "request_effective_verb", lambda entries: answers)


@pytest.mark.parametrize("rung", ["unreadable", "done", "superseded"])
def test_unanswerable_plan_rungs_refuse(monkeypatch, rung):
    from fno.agents.node_dispatch import node_effective_verb

    _stub_door(
        monkeypatch,
        [
            {
                "refusal": (
                    f"dispatch verb cannot be derived for node x-abcd: plan rung "
                    f"{rung!r} with difficulty '' answers no lifecycle rung"
                )
            }
        ],
    )
    with pytest.raises(DispatchResolveError, match=rung) as exc_info:
        node_effective_verb({"id": "x-abcd"})
    assert_refusal_names_subject_and_cites_no_node(exc_info.value)


def test_planless_node_without_difficulty_refuses_naming_the_field(monkeypatch):
    from fno.agents.node_dispatch import node_effective_verb

    _stub_door(
        monkeypatch,
        [
            {
                "refusal": (
                    "dispatch verb cannot be derived for node x-abcd: plan rung "
                    "'none' with difficulty '' answers no lifecycle rung"
                )
            }
        ],
    )
    with pytest.raises(DispatchResolveError, match="difficulty") as exc_info:
        node_effective_verb({"id": "x-abcd"})
    assert_refusal_names_subject_and_cites_no_node(exc_info.value)


def test_planless_node_with_invalid_difficulty_refuses(monkeypatch):
    from fno.agents.node_dispatch import node_effective_verb

    _stub_door(
        monkeypatch,
        [
            {
                "refusal": (
                    "dispatch verb cannot be derived for node x-abcd: plan rung "
                    "'none' with difficulty 'spicy' answers no lifecycle rung"
                )
            }
        ],
    )
    with pytest.raises(DispatchResolveError, match="difficulty") as exc_info:
        node_effective_verb({"id": "x-abcd", "difficulty": "spicy"})
    assert_refusal_names_subject_and_cites_no_node(exc_info.value)


def test_refusal_without_node_id_names_unknown_not_a_citation(monkeypatch):
    # No node id anywhere in scope: the sentence names "unknown" and still
    # carries no citation for a reader to mistake for the subject.
    from fno.agents.node_dispatch import node_effective_verb

    _stub_door(
        monkeypatch,
        [
            {
                "refusal": (
                    "dispatch verb cannot be derived for node unknown: plan rung "
                    "'none' with difficulty '' answers no lifecycle rung"
                )
            }
        ],
    )
    with pytest.raises(DispatchResolveError) as exc_info:
        node_effective_verb({})
    message = str(exc_info.value)
    assert "for node unknown" in message
    assert "(x-" not in message


def test_missing_runtime_refuses_naming_the_remedy(monkeypatch):
    # The binary being absent is a refusal, never a guessed verb and never a
    # Python fallback table.
    from fno.agents.node_dispatch import node_effective_verb
    from fno.graph.store import STATE_SPAWN_FAILED, StoreUnavailable

    def absent(entries):
        raise StoreUnavailable(
            STATE_SPAWN_FAILED,
            "fno-agents-worker not found (set FNO_AGENTS_WORKER or install the runtime)",
        )

    _stub_door(monkeypatch, absent)
    with pytest.raises(DispatchResolveError, match="fno doctor update --rust"):
        node_effective_verb({"id": "x-abcd"})


def assert_refusal_names_subject_and_cites_no_node(exc: DispatchResolveError) -> None:
    # A king reads the refusal cold: the failing node must be the first
    # identifier in the sentence, and a trailing citation invites a misread
    # (the node that shipped this assertion went to fix the cited node).
    message = str(exc)
    assert "for node x-abcd" in message
    assert message.index("for node x-abcd") < message.index("plan rung")
    assert "(x-" not in message


def test_bare_resolve_without_node_context_keeps_target_template():
    out = resolve_dispatch(harness="claude", node_id="x-abcd")
    assert out["command"] == "/target --no-merge x-abcd"
    assert out["verb"] is None


def test_out_of_family_declared_verb_keeps_declared_precedence():
    # /think is outside the lifecycle table: the door abstains and the node's
    # own declaration rides the allowlist-checked verb rung as declared.
    out = resolve_dispatch(
        harness="claude",
        node_id="x-abcd",
        verb="/think",
        lifecycle=(None, "verb=lifecycle(out-of-family /think; declared precedence holds)"),
    )
    assert out["command"] == "/think x-abcd"
    assert out["verb"] is None
    assert any("out-of-family /think" in d for d in out["decision"])


def test_explicit_command_bypasses_the_lifecycle_refusal():
    # Reconcile (and any other explicit-command door) spells its own verb and
    # never consults the lifecycle at all.
    out = resolve_dispatch(
        harness="claude",
        node_id="x-abcd",
        command="/target --reconcile /tmp/m.md {id}",
    )
    assert out["command"] == "/target --reconcile /tmp/m.md x-abcd"


def test_stored_blueprint_verb_passes_the_default_allowlist():
    # dispatch_cfg={} pins the BUILT-IN default allowlist; the ambient config
    # may override allowed_verbs either way and is not this test's subject.
    out = resolve_dispatch(
        harness="claude", node_id="x-abcd", verb="/blueprint", dispatch_cfg={}
    )
    assert out["command"] == "/blueprint x-abcd"


def test_derived_target_reads_the_auto_merge_grant():
    out = resolve_dispatch(
        harness="claude",
        node_id="x-abcd",
        lifecycle=("/target", "verb=lifecycle(intake difficulty=low -> /target)"),
        dispatch_cfg={"auto_merge": True},
    )
    assert out["command"] == "/target x-abcd"


def test_derived_blueprint_ignores_the_operator_target_template():
    # config.dispatch.command is a target-phase contract; a derived blueprint
    # is a different phase and renders its own verb.
    out = resolve_dispatch(
        harness="claude",
        node_id="x-abcd",
        lifecycle=("/blueprint", "verb=lifecycle(intake difficulty=high -> /blueprint)"),
        dispatch_cfg={"command": "/target --special {id}"},
    )
    assert out["command"] == "/blueprint x-abcd"


def test_stage_table_resolves_the_derived_verb_profile():
    # Locked Decision 4: the resolved verb is the profile key, so a medium
    # planless node reaches agents.profiles.blueprint.provider even though its
    # stored verb (or the default) says target.
    import types

    stub = types.SimpleNamespace(
        agents=types.SimpleNamespace(
            profiles={
                "blueprint": types.SimpleNamespace(provider="codex"),
            }
        ),
        dispatch=None,
    )
    out = resolve_dispatch(
        node_id="x-abcd",
        lifecycle=("/blueprint", "verb=lifecycle(intake difficulty=medium -> /blueprint)"),
        settings=stub,
    )
    assert out["harness"] == "codex"
    assert out["command"] == "$fno:blueprint x-abcd"
