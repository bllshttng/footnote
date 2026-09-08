"""The spawn phase inference: label the work, never the guess (x-70e1)."""

from __future__ import annotations

from fno.agents.spawn_phase import infer_phase


def test_harness_qualified_review_spellings_all_stamp_review():
    """The codex spelling travels on the normalized form: `$fno:review` and
    the slash spellings all name a reviewer, prose never does."""
    assert infer_phase("$fno:review xhigh branch HEAD against main") == "review"
    assert infer_phase("/fno:review xhigh branch HEAD against main") == "review"
    assert infer_phase("/code-review <level> --comment") == "review"
    assert infer_phase("review this diff") == ""
    assert infer_phase("/fno:triage deep") == ""
    assert infer_phase("/fno:think deep") == "think"
    assert infer_phase("$fno:blueprint doc.md") == "blueprint"
