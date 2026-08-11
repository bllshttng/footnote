"""Tests for the harness-capability map + shared dispatch resolver (US1)."""
from __future__ import annotations

import pytest

from fno.agents.harness_map import (
    DispatchResolveError,
    known_harnesses,
    resolve_dispatch,
    substrate_default,
)

# Config read is stubbed to empty in every resolve so the tests exercise the
# built-in precedence, not the ambient project config.
_NO_CFG: dict = {}


def _resolve(**kw):
    kw.setdefault("dispatch_cfg", _NO_CFG)
    return resolve_dispatch(**kw)


def test_default_harness_is_claude_bg_with_bypass():
    """AC1-HP: a node with no dispatch fields resolves to /target no-merge <id>
    on claude/bg with the permission-bypass flag (so the worker never hangs on
    an approval prompt)."""
    out = _resolve(node_id="x-4d85")
    assert out["harness"] == "claude"
    assert out["substrate"] == "bg"
    assert out["command"] == "/target no-merge x-4d85"
    assert out["permission_bypass"] == ["--dangerously-skip-permissions"]


def test_codex_defaults_to_headless():
    """Verify line: --harness codex resolves to the headless substrate."""
    out = _resolve(harness="codex")
    assert out["substrate"] == "headless"
    assert out["bg"] is False
    assert out["permission_bypass"] == ["--dangerously-bypass-approvals-and-sandbox"]


def test_unknown_harness_fails_loud_naming_the_map():
    """AC1-ERR: an unknown harness raises, naming the harness AND the map."""
    with pytest.raises(DispatchResolveError) as exc:
        _resolve(harness="nonexistent")
    msg = str(exc.value)
    assert "nonexistent" in msg
    assert "fno.agents.harness_map" in msg


def test_explicit_bg_on_non_claude_is_rejected():
    """bg is claude-only; an explicit bg on codex is a hard error -> headless."""
    with pytest.raises(DispatchResolveError, match="headless"):
        _resolve(harness="codex", substrate="bg")


def test_autonomous_pane_is_rejected():
    """Invariant: an autonomous trigger never resolves a stalling pane."""
    with pytest.raises(DispatchResolveError, match="pane"):
        _resolve(harness="claude", substrate="pane", trigger="autonomous")


def test_attended_pane_is_allowed():
    """A pane is valid for an attended trigger (a human drives it)."""
    out = _resolve(harness="claude", substrate="pane", trigger="attended")
    assert out["substrate"] == "pane"


def test_template_without_node_is_literal():
    """No node id -> the template is returned verbatim ({id} unsubstituted).
    codex normalizes to its `$fno:` skill surface (x-a5e4)."""
    out = _resolve(harness="codex")
    assert out["command"] == "$fno:target no-merge {id}"


def test_bad_template_rejected_when_substituting():
    """A template lacking exactly one {id} cannot substitute a node id."""
    with pytest.raises(DispatchResolveError, match="{id}"):
        _resolve(node_id="x-1", command="/target no-merge")


def test_empty_explicit_harness_fails_loud():
    """An empty explicit --harness (unset env var interpolated into a flag) must
    fail loud, not silently fall through to config/claude."""
    with pytest.raises(DispatchResolveError, match="must not be empty"):
        _resolve(harness="")
    with pytest.raises(DispatchResolveError, match="must not be empty"):
        _resolve(harness="claude", substrate="  ")


def test_config_substrate_typo_fails_loud():
    """A config.dispatch.substrate typo is a trust boundary too - it must raise,
    not resolve silently to a launcher."""
    with pytest.raises(DispatchResolveError, match="unknown substrate"):
        resolve_dispatch(harness="claude", dispatch_cfg={"substrate": "panel"})


def test_pane_guard_fails_closed_on_unknown_trigger():
    """A typo is refused, while an omitted trigger keeps autonomous semantics."""
    with pytest.raises(DispatchResolveError, match="unknown dispatch trigger"):
        _resolve(harness="claude", substrate="pane", trigger="autonamous")
    with pytest.raises(DispatchResolveError, match="pane"):
        _resolve(harness="claude", substrate="pane", trigger=None)


def test_config_overlay_precedence():
    """config.dispatch overlays the built-in but loses to an explicit flag.

    The config command template is canonical claude slash syntax, normalized
    per-harness at resolve (x-f0e2): `/think` becomes `$fno:think` on codex."""
    cfg = {"harness": "codex", "substrate": "", "command": "/think {id}"}
    out = resolve_dispatch(node_id="x-9", dispatch_cfg=cfg)
    assert out["harness"] == "codex"
    assert out["command"] == "$fno:think x-9"
    # explicit flag beats config
    out2 = resolve_dispatch(harness="claude", node_id="x-9", dispatch_cfg=cfg)
    assert out2["harness"] == "claude"


def test_config_command_normalized_per_harness():
    """x-f0e2: a slash-leading config template is normalized on the chosen
    harness, exactly like the builtin rung - config stops being literal."""
    cfg = {"command": "/target no-merge {id}"}
    # codex: leading /verb -> $fno:verb
    assert resolve_dispatch(harness="codex", node_id="x-1234", dispatch_cfg=cfg)[
        "command"
    ] == "$fno:target no-merge x-1234"
    # claude: byte-identical to today (slash surface normalizes to itself)
    assert resolve_dispatch(harness="claude", node_id="x-1234", dispatch_cfg=cfg)[
        "command"
    ] == "/target no-merge x-1234"
    # opencode: plugin-namespaced slash surface -> `/fno:target ...` (x-de43)
    assert resolve_dispatch(harness="opencode", node_id="x-1234", dispatch_cfg=cfg)[
        "command"
    ] == "/fno:target no-merge x-1234"


def test_explicit_command_normalized_per_harness():
    """AC2-HP: the explicit `command=` rung normalizes too (x-0676 --reconcile
    passes an explicit template)."""
    # `_resolve`, not a bare `resolve_dispatch`: this was the one call in the
    # file that read the ambient config, so on a machine whose
    # `[dispatch] substrate = "bg"` it resolved bg on codex and died
    # ("bg is claude-only") instead of testing the normalization it names.
    # Green in CI, red on a configured developer machine.
    out = _resolve(command="/target no-merge {id}", harness="codex", node_id="x-1")
    assert out["command"] == "$fno:target no-merge x-1"


def test_codex_normalization_accepts_plugin_qualified_slash_and_native_skill():
    """A direct CLI caller naturally sends the advertised ``/fno:`` form.

    Codex needs the equivalent ``$fno:`` skill reference, and repeating the
    normalization at multiple dispatch choke points must stay idempotent.
    """
    from fno.agents.harness_map import normalize_command

    assert normalize_command("/fno:target x-81ad", "codex") == "$fno:target x-81ad"
    assert normalize_command("$fno:target x-81ad", "codex") == "$fno:target x-81ad"


def test_opencode_default_dispatch_renders_fno_slash():
    """AC1-HP: a default opencode dispatch renders the plugin-namespaced palette
    invocation `/fno:target ...` on the headless substrate - no prose brief."""
    out = _resolve(harness="opencode", node_id="x-4d85")
    assert out["command"] == "/fno:target no-merge x-4d85"
    assert out["substrate"] == "headless"
    assert out["command_surface"] == "slash"


def test_opencode_arbitrary_verb_renders_fno_prefix():
    """AC4-EDGE: the single prefix-swap rule renders ANY verb (no allowlist in the
    render path) - `/blueprint quick <doc>` and an arbitrary `/zzz` both namespace."""
    out = resolve_dispatch(
        harness="opencode", node_id="x-9", dispatch_cfg={"command": "/blueprint quick {id}"}
    )
    assert out["command"] == "/fno:blueprint quick x-9"
    out2 = resolve_dispatch(
        harness="opencode", node_id="x-9", dispatch_cfg={"command": "/zzz args {id}"}
    )
    assert out2["command"] == "/fno:zzz args x-9"


def test_normalize_fallback_table_mirrors_harness_map():
    """Parity: normalize.sh's static command-surface fallback (used only when
    `fno dispatch resolve` is unreachable) MUST mirror harness_map, the SoT - a
    drift would dispatch the wrong spelling. Parses the shell case block and
    compares each provider's surface to capabilities()."""
    import re
    from pathlib import Path

    from fno.agents.harness_map import capabilities

    root = Path(__file__).resolve()
    norm = None
    for _ in range(8):
        cand = root / "skills/agent/scripts/normalize.sh"
        if cand.exists():
            norm = cand
            break
        root = root.parent
    assert norm is not None, "normalize.sh not found from test location"

    m = re.search(
        r"resolve_command_surface\(\).*?case \"\$_prov\" in(.*?)esac",
        norm.read_text(),
        re.S,
    )
    assert m, "resolve_command_surface case block not found"
    shell_map: dict[str, str] = {}
    for arm in re.finditer(r"([a-z|*]+)\)\s*printf '([a-z-]+)'", m.group(1)):
        for prov in arm.group(1).split("|"):
            if prov != "*":
                shell_map[prov] = arm.group(2)
    assert shell_map, "no provider arms parsed from normalize.sh"
    for prov, surface in shell_map.items():
        assert capabilities(prov)["command_surface"] == surface, (
            f"normalize.sh fallback maps {prov!r}->{surface!r} but harness_map "
            f"says {capabilities(prov)['command_surface']!r}"
        )
    for prov in ("claude", "agy", "opencode", "codex", "gemini"):
        assert prov in shell_map, f"normalize.sh fallback omits {prov!r}"

    # slash_prefix must mirror too, or opencode would render `/target` not
    # `/fno:target`. The shell helper lists only non-empty prefixes explicitly
    # (opencode); claude/agy fall to `*` -> "", matching harness_map's default.
    mp = re.search(r"slash_prefix\(\).*?case \"\$1\" in(.*?)esac", norm.read_text(), re.S)
    assert mp, "slash_prefix case block not found in normalize.sh"
    shell_prefix: dict[str, str] = {}
    for arm in re.finditer(r"([a-z|*]+)\)\s*printf '([a-z:-]*)'", mp.group(1)):
        for prov in arm.group(1).split("|"):
            if prov != "*":
                shell_prefix[prov] = arm.group(2)
    for prov in ("claude", "agy", "opencode"):
        expected = capabilities(prov).get("slash_prefix", "")
        assert shell_prefix.get(prov, "") == expected, (
            f"normalize.sh slash_prefix maps {prov!r}->{shell_prefix.get(prov, '')!r} "
            f"but harness_map says {expected!r}"
        )


def test_gemini_dispatch_is_refused_naming_agy():
    """AC2-ERR: a deprecated gemini harness has no dispatch lane; EVERY resolve
    refuses loudly and names the successor (agy) - default, slash template, and
    even a non-slash prose template that would otherwise pass the seam."""
    for cfg in (None, {"command": "/think {id}"}, {"command": "implement node {id}"}):
        with pytest.raises(DispatchResolveError, match="agy"):
            resolve_dispatch(harness="gemini", node_id="x-1", dispatch_cfg=cfg or {})


def test_config_non_slash_prose_template_untouched():
    """AC1-EDGE: a non-slash template is never rewritten - byte-identical on every
    slash/codex harness (the startswith('/') gate is the opt-out). gemini is
    excluded: it refuses outright (test_gemini_dispatch_is_refused_naming_agy)."""
    cfg = {"command": "implement node {id} and open a PR"}
    for h in ("opencode", "codex", "claude"):
        assert resolve_dispatch(harness=h, node_id="x-9", dispatch_cfg=cfg)[
            "command"
        ] == "implement node x-9 and open a PR"


def test_config_absolute_path_template_untouched():
    """An absolute-path template leads with `/` but is NOT a footnote slash
    command (its first word carries internal slashes), so it must pass through
    literally on every harness - never rewritten to `$fno:usr/...` on codex or
    rejected on a prose harness."""
    cfg = {"command": "/usr/bin/custom-script {id}"}
    for h in ("opencode", "codex", "claude"):  # gemini refuses outright (x-de43)
        assert resolve_dispatch(harness=h, node_id="x-9", dispatch_cfg=cfg)[
            "command"
        ] == "/usr/bin/custom-script x-9"


def test_config_already_native_template_not_double_prefixed():
    """AC2-EDGE: an already-codex-native `$fno:` template is not slash-leading,
    so it passes through unchanged - normalization is idempotent."""
    out = resolve_dispatch(
        harness="codex", node_id="x-9", dispatch_cfg={"command": "$fno:target {id}"}
    )
    assert out["command"] == "$fno:target x-9"


def test_substrate_default_table():
    assert substrate_default("claude") == "bg"
    for h in ("codex", "gemini", "agy", "opencode"):
        assert substrate_default(h) == "headless"


def test_known_harnesses_covers_readable_set():
    """The map covers the readable-provider set so US4 can wire opencode."""
    assert set(known_harnesses()) == {"claude", "codex", "gemini", "agy", "opencode"}


# --- US3: configurable dispatch verb + brief ------------------------------


def test_node_verb_assembles_command():
    """AC2-HP: a node verb resolves to `<verb> <id>` (not the /target default)."""
    out = _resolve(node_id="x-1", verb="/think")
    assert out["command"] == "/think x-1"
    assert out["env"] == {}


def test_node_brief_rides_env_never_command():
    """AC2-HP: the brief reaches the worker via TARGET_BRIEF env, and no brief
    text is shell-interpolated into the command line."""
    out = _resolve(node_id="x-1", verb="/think", brief="brainstorm the retry design")
    assert out["command"] == "/think x-1"
    assert out["env"]["TARGET_BRIEF"] == "brainstorm the retry design"
    assert "brainstorm" not in out["command"]


def test_out_of_allowlist_verb_rejected():
    """AC3-EDGE: an injection-shaped verb is refused, naming the verb + allowlist."""
    with pytest.raises(DispatchResolveError) as exc:
        _resolve(node_id="x-1", verb="rm -rf; /target")
    msg = str(exc.value)
    assert "rm -rf" in msg
    assert "/target" in msg  # the allowlist is named


def test_empty_verb_rejected():
    """An explicit empty verb fails loud rather than silently defaulting."""
    with pytest.raises(DispatchResolveError):
        _resolve(node_id="x-1", verb="   ")


def test_brief_over_8kb_rejected():
    """Verify 4: a brief larger than the 8 KB env budget is an explicit error,
    never silent truncation."""
    with pytest.raises(DispatchResolveError, match="8"):
        _resolve(node_id="x-1", verb="/think", brief="x" * 8193)


def test_brief_at_8kb_ok():
    out = _resolve(node_id="x-1", verb="/think", brief="x" * 8192)
    assert out["env"]["TARGET_BRIEF"] == "x" * 8192


def test_no_verb_leaves_default_and_empty_env():
    """Verify 3 (regression): no dispatch fields -> /target no-merge <id>, env empty."""
    out = _resolve(node_id="x-1")
    assert out["command"] == "/target no-merge x-1"
    assert out["env"] == {}


def test_config_extends_allowlist():
    """A per-project allowlist admits a domain workflow verb."""
    cfg = {"allowed_verbs": ["/target", "/think", "/marketing"]}
    out = _resolve(node_id="x-1", verb="/marketing", dispatch_cfg=cfg)
    assert out["command"] == "/marketing x-1"


def test_node_verb_wins_over_config_command():
    """Precedence: node verb > config.dispatch.command > builtin."""
    cfg = {"command": "/foo {id}"}
    out = _resolve(node_id="x-1", verb="/think", dispatch_cfg=cfg)
    assert out["command"] == "/think x-1"


def test_brief_without_verb_still_rides_env():
    """A brief on a default (/target) dispatch still travels via env."""
    out = _resolve(node_id="x-1", brief="ship carefully")
    assert out["command"] == "/target no-merge x-1"
    assert out["env"]["TARGET_BRIEF"] == "ship carefully"


def _normalize_sh():
    """Locate skills/agent/scripts/normalize.sh from this test's location."""
    from pathlib import Path

    root = Path(__file__).resolve()
    for _ in range(8):
        cand = root / "skills/agent/scripts/normalize.sh"
        if cand.exists():
            return cand
        root = root.parent
    raise AssertionError("normalize.sh not found from test location")


def test_normalize_reads_the_same_merge_posture_key_as_harness_map():
    """Parity: the merge posture has TWO independent readers and nothing else
    pins them together.

    normalize.sh gates arbitrary `/fno:agent spawn` payloads, which never reach
    resolve_dispatch, so it legitimately keeps its own read rather than being
    deleted as a workaround (x-8e59). That independence is the hazard: if one
    side is repointed at a renamed key and the other is not, the two silently
    disagree about whether a worker may merge.

    The sibling test above mirrors command_surface and slash_prefix. It does not
    cover posture, which is how the pair went unpinned in the first place.
    """
    import re
    import types

    text = _normalize_sh().read_text()
    m = re.search(r"fno config get ([\w.]+)", text)
    assert m, "normalize.sh no longer reads a config key for the merge posture"
    key = m.group(1)

    # The key must be a declared config key, not a plausible-looking typo.
    from fno.config.registry import meta_for

    assert meta_for(key) is not None, (
        f"normalize.sh reads {key!r}, which the config registry does not declare"
    )

    # Then EXERCISE the Python reader with the key the shell actually reads,
    # rather than comparing the shell to a literal spelled out here. A literal
    # only pins the shell to this test file: repoint _load_dispatch_cfg at a
    # renamed field and a string comparison stays green while the two readers
    # have silently diverged - the same "guard that never touches the path it
    # claims to cover" shape as the bug this node fixed (codex P2 on PR #640).
    #
    # Build the settings stub FROM the shell's key. If Python is repointed, this
    # stub no longer grants and the assertion fails; if the shell is repointed,
    # the registry check above fails. Neither side can move alone.
    section, _, field = key.partition(".")
    assert section and field, f"expected a dotted config key, got {key!r}"
    stub = types.SimpleNamespace(**{section: types.SimpleNamespace(**{field: True})})
    out = resolve_dispatch(harness="claude", node_id="x-1", settings=stub)
    assert out["command"] == "/target x-1", (
        f"normalize.sh reads {key!r}, but setting that field does not grant merge "
        f"on the Python path (got {out['command']!r}) - the two readers have diverged"
    )


def test_both_merge_posture_readers_default_to_deny():
    """Parity: no key, no grant - on BOTH sides.

    Granting merge authority is the irreversible direction, so the default and
    every error path must land on no-merge. normalize.sh states this as
    `ALLOW_MERGE=0` before its config read (so `fno` absent or erroring degrades
    to deny); harness_map states it as `is True`, which refuses a truthy
    non-boolean. A change that flips either default to allow is the one drift
    worth failing a build over.
    """
    import re

    text = _normalize_sh().read_text()
    # The assignment guarding the config read must be the deny value.
    m = re.search(r"if \[\[ -z \"\$ALLOW_MERGE\" \]\]; then\s*\n\s*ALLOW_MERGE=(\d)", text)
    assert m, "normalize.sh's ALLOW_MERGE default block changed shape; re-verify it still denies"
    assert m.group(1) == "0", "normalize.sh now defaults to GRANTING merge"

    # harness_map: absent key, and a truthy non-boolean, both deny.
    assert resolve_dispatch(harness="claude", node_id="x-1", dispatch_cfg={})[
        "command"
    ] == "/target no-merge x-1"
    assert resolve_dispatch(
        harness="claude", node_id="x-1", dispatch_cfg={"auto_merge": "true"}
    )["command"] == "/target no-merge x-1"
