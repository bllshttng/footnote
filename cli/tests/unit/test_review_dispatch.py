"""Regression tests for the sigma-review worker dispatch path.

Tracks AC4-HP / AC4-ERR / AC4-EDGE from the events + test hygiene cleanup
spec (ab-a1118224). The legacy dispatcher forwarded the agent definition
file's full text (including a leading ``---\\n`` YAML frontmatter fence)
to ``claude -p`` as an argv element. claude's argument parser saw the
leading ``---`` and rejected it with ``unknown option '---'``, causing
every worker on every sigma-review invocation to ``spawn_failed`` while
the panel still wrote a zero-finding artifact with verdict
``done-with-concerns`` indistinguishable from a real clean review.

The fix: strip the YAML frontmatter at prompt-load time so the prompt
body passed to ``claude -p`` no longer starts with ``---``.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.review.orchestrator import (
    AGENT_NAMES,
    PromptMissingError,
    _strip_frontmatter,
    load_prompts,
)


# ---------------------------------------------------------------------------
# AC4-EDGE: bundled prompts have no leading '---' substring (regression pin)
# ---------------------------------------------------------------------------


def test_loaded_prompts_strip_yaml_frontmatter() -> None:
    """AC4-EDGE: every prompt returned by ``load_prompts`` has the YAML
    frontmatter stripped, so the prompt body passed to ``claude -p`` does
    not start with the ``---`` fence claude's argument parser interprets
    as an unknown option."""
    prompts = load_prompts()
    assert set(prompts.keys()) == set(AGENT_NAMES)
    for name, body in prompts.items():
        assert not body.lstrip().startswith("---"), (
            f"Loaded prompt for {name!r} still has YAML frontmatter at the "
            f"start; this regresses to the spawn_failed: unknown option "
            f"'---' bug. Body starts: {body[:80]!r}"
        )
        # And the body should contain the actual instructions (some plausible
        # substring shared across agents).
        assert body.strip(), f"Loaded prompt for {name!r} is empty after strip"


# ---------------------------------------------------------------------------
# AC4-EDGE: _strip_frontmatter contract
# ---------------------------------------------------------------------------


def test_strip_frontmatter_removes_yaml_block() -> None:
    """The helper strips a well-formed YAML frontmatter and returns the body."""
    text = (
        "---\n"
        "name: code-reviewer\n"
        "description: Reviews code.\n"
        "---\n"
        "\n"
        "You are an expert code reviewer.\n"
    )
    result = _strip_frontmatter(text, Path("agent.md"))
    assert result.startswith("\nYou are an expert"), (
        f"frontmatter strip should leave only the body; got: {result!r}"
    )
    assert "name: code-reviewer" not in result


def test_strip_frontmatter_missing_opening_fence_raises() -> None:
    """A file with no leading ``---\\n`` is a malformed agent definition; raise."""
    text = "name: code-reviewer\n\nBody here.\n"
    with pytest.raises(PromptMissingError, match="frontmatter"):
        _strip_frontmatter(text, Path("agent.md"))


def test_strip_frontmatter_unclosed_fence_raises() -> None:
    """An opening ``---\\n`` with no closing fence is malformed; raise."""
    text = "---\nname: code-reviewer\n\nBody but no closing fence."
    with pytest.raises(PromptMissingError, match="frontmatter"):
        _strip_frontmatter(text, Path("agent.md"))


def test_strip_frontmatter_preserves_internal_dashes() -> None:
    """An internal ``---`` (e.g. inside a code fence in the body) is preserved."""
    text = (
        "---\n"
        "name: code-reviewer\n"
        "---\n"
        "Body with internal separator:\n"
        "\n"
        "---\n"
        "\n"
        "More body.\n"
    )
    result = _strip_frontmatter(text, Path("agent.md"))
    assert "Body with internal separator" in result
    assert "More body" in result
    # The internal separator stays in the body; only the opening fence pair is removed.
    assert "\n---\n" in result


# ---------------------------------------------------------------------------
# AC4-EDGE: no '---' substring at the head of the composed argv string
# ---------------------------------------------------------------------------


def test_composed_prompt_does_not_start_with_dashes(monkeypatch) -> None:
    """The string that lands in ``argv[2]`` for ``claude -p <prompt>`` must not
    start with ``---``. This pins the regression so a future refactor that
    reintroduces frontmatter-in-prompt is caught at unit-test time."""
    from fno.review import runners as _runners_pkg
    from fno.review.runners import claude_runner

    prompts = load_prompts()
    diff_context = "diff --git a/foo b/foo\n+x\n"

    # Capture the prompt passed to the canonical dispatch seam.
    captured: dict[str, str] = {}

    class _StubDispatch:
        def __call__(self, **kwargs: object) -> object:
            captured["prompt"] = str(kwargs["message"])
            return type("Reply", (), {"reply": ""})()

    dispatch = _StubDispatch()
    # Run one agent through the runner; we only care about the argv shape.
    name = next(iter(prompts))
    claude_runner.run_via_claude_code(
        name,
        prompts[name],
        diff_context,
        dispatch=dispatch,
    )

    composed = captured["prompt"]
    assert not composed.lstrip().startswith("---"), (
        f"composed prompt for {name!r} still starts with '---'; "
        f"this would crash claude -p with `unknown option '---'`. "
        f"Head of composed: {composed[:120]!r}"
    )
