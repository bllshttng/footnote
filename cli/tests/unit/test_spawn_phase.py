"""The spawn phase inference: label the work from the verb table, never a
guess (x-70e1, x-007c)."""

from __future__ import annotations

from fno.agents.spawn_phase import infer_phase


def test_table_maps_every_shipped_work_verb_in_all_three_spellings():
    """AC1: every shipped work verb labels its phase in the bare (/verb),
    plugin (/fno:verb) and codex ($fno:verb) spellings."""
    cases = [
        ("/blueprint x-1", "blueprint"), ("/fno:blueprint x-1", "blueprint"),
        ("$fno:blueprint x-1", "blueprint"),
        ("/think q", "think"), ("$fno:think q", "think"),
        ("/fix", "do"), ("/fno:fix t", "do"), ("$fno:tdd x", "do"),
        ("/execute p.md", "do"), ("/fno:execute waves p.md", "do"), ("/do x", "do"),
        ("/pr create", "ship"), ("/fno:pr check 12", "ship"), ("/ship pr", "ship"),
        ("/target", "do"), ("/fno:target", "do"), ("$fno:target", "do"),
        ("/review", "review"), ("/fno:review", "review"), ("/code-review", "review"),
    ]
    for message, expected in cases:
        assert infer_phase(message) == expected, message


def test_prefixless_and_unmapped_payloads_stay_unlabeled():
    """AC2: a first token with no leading / or $, an unmapped verb, or an
    empty message answers ""."""
    assert infer_phase("review this") == ""
    assert infer_phase("blueprint x-1") == ""
    assert infer_phase("/fno:triage deep") == ""
    assert infer_phase("") == ""
    assert infer_phase(None) == ""


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
