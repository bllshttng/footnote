"""The shipped-verb roster behind codex command normalization.

Its siblings live beside the module in `cli/src/fno/agents/test_harness_map.py`.
This one is here because `cli/src/fno` is shrink-only as a tree and a test
file inside it counts against that budget; `cli/tests/unit/` already holds
`test_harness_identity.py` and `test_harness_names.py`.
"""

from pathlib import Path


def test_an_unresolved_roster_is_never_cached(monkeypatch, tmp_path):
    """A failed plugin-surface read must not freeze the degraded answer.

    Empty roster -> every bare `/verb` stays literal -> a codex worker gets
    prose, and the dispatch quietly does nothing. The resolver reads an env
    hint first, so the next call must be free to succeed. The mirror leg
    proves a GOOD answer still caches.

    The specimen is `/blueprint`, not `/target`: the target family bypasses the
    roster entirely, so it cannot show a degraded read."""
    from fno.agents import harness_map

    repo_root = Path(harness_map.__file__).resolve().parents[4]
    harness_map.footnote_verbs.cache_clear()
    try:
        # A plugin root with no skills/ and no commands/: it resolves, the
        # read finds nothing, the roster is empty.
        monkeypatch.setenv("CLAUDE_PLUGIN_ROOT", str(tmp_path))
        assert harness_map.footnote_verbs() == frozenset()
        assert harness_map.normalize_command("/blueprint x-1", "codex") == "/blueprint x-1"

        # Same process, real plugin root. Nothing cleared by hand.
        monkeypatch.setenv("CLAUDE_PLUGIN_ROOT", str(repo_root))
        assert "target" in harness_map.footnote_verbs()
        assert harness_map.normalize_command("/blueprint x-1", "codex") == "$fno:blueprint x-1"

        hits = harness_map._shipped_verbs.cache_info().hits
        harness_map.footnote_verbs()
        assert harness_map._shipped_verbs.cache_info().hits == hits + 1
    finally:
        harness_map.footnote_verbs.cache_clear()


def test_fno_me_ships_as_a_skill():
    """The roster join must live under skills/, the only surface every harness ships.

    Codex declares `skills/` only (.codex-plugin/plugin.json) and the agy bundle
    copies `skills/` and `agents/` (scripts/install/agy-plugin.sh), so a file
    under commands/ never reaches those harnesses and their agents cannot
    invoke it. The verb matrix is generated from skills/*/SKILL.md, so the row
    proves the projection sees the skill too."""
    from fno.agents import harness_map

    repo_root = Path(harness_map.__file__).resolve().parents[4]
    assert (repo_root / "skills/fno-me/SKILL.md").is_file(), (
        "skills/fno-me/SKILL.md missing: fno-me must ship as a skill, not a command"
    )
    matrix = (repo_root / "docs/harnesses/verb-matrix.md").read_text(encoding="utf-8")
    assert "| fno-me |" in matrix, "verb-matrix.md has no fno-me row: regenerate it"
