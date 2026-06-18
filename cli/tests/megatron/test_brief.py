"""Megatron Phase 3 Task 3.2: brief assembly tests."""
from __future__ import annotations


def test_brief_concatenates_discoveries_in_msgid_order():
    from fno.megatron.brief import assemble_wave_brief

    completes = [
        {
            "wave": 1,
            "from": "B",
            "msg_id": "msg-bb0001",
            "body": "### Summary\nshipped\n\n### Discoveries\n- B-disc-1\n- B-disc-2",
        },
        {
            "wave": 1,
            "from": "A",
            "msg_id": "msg-aa0001",
            "body": "### Summary\nshipped\n\n### Discoveries\n- A-disc-1",
        },
        {
            "wave": 1,
            "from": "C",
            "msg_id": "msg-cc0001",
            "body": "### Summary\nshipped\n\n### Discoveries\n- C-disc-1",
        },
    ]

    brief = assemble_wave_brief(completes_for_wave=completes, wave=1)

    # Order is by msg_id ascending: aa < bb < cc
    a_idx = brief.find("From A")
    b_idx = brief.find("From B")
    c_idx = brief.find("From C")
    assert -1 < a_idx < b_idx < c_idx
    assert "A-disc-1" in brief
    assert "B-disc-1" in brief
    assert "C-disc-1" in brief
    assert brief.startswith("# Wave 1 brief")


def test_brief_handles_missing_discoveries_section():
    from fno.megatron.brief import assemble_wave_brief

    completes = [
        {
            "wave": 1,
            "from": "A",
            "msg_id": "msg-aa0001",
            "body": "### Summary\nshipped\n\n### Discoveries\n- A-disc-1",
        },
        {
            "wave": 1,
            "from": "B",
            "msg_id": "msg-bb0001",
            "body": "### Summary\nshipped (no discoveries section)",
        },
    ]

    brief = assemble_wave_brief(completes_for_wave=completes, wave=1)

    assert "(no discoveries reported)" in brief
    assert "A-disc-1" in brief


def test_brief_truncates_oversize_complete_body():
    from fno.megatron.brief import assemble_wave_brief

    big_disc = "- " + ("X" * (15 * 1024)) + "\n"
    completes = [
        {
            "wave": 1,
            "from": "A",
            "msg_id": "msg-aa0001",
            "body": f"### Summary\nshipped\n\n### Discoveries\n{big_disc}",
        }
    ]

    brief = assemble_wave_brief(completes_for_wave=completes, wave=1)

    assert "[truncated at 10KB" in brief
    # Output should not contain the full 15KB payload
    assert len(brief.encode("utf-8")) < 12 * 1024


def test_brief_stable_across_runs():
    """Calling assemble_wave_brief twice with the same input is byte-identical."""
    from fno.megatron.brief import assemble_wave_brief

    completes = [
        {
            "wave": 2,
            "from": "ops",
            "msg_id": "msg-op0001",
            "body": "### Discoveries\n- ops thing",
        },
        {
            "wave": 2,
            "from": "data",
            "msg_id": "msg-da0001",
            "body": "### Discoveries\n- data thing",
        },
    ]

    a = assemble_wave_brief(completes_for_wave=completes, wave=2, now_iso="2026-05-06T15:00:00Z")
    b = assemble_wave_brief(completes_for_wave=completes, wave=2, now_iso="2026-05-06T15:00:00Z")
    assert a == b


def test_inject_brief_into_wave_prepends_to_each_body():
    from fno.megatron.brief import inject_brief_into_bodies

    bodies = ["original A body", "original B body"]
    brief = "# Wave 1 brief\n\nstuff"
    result = inject_brief_into_bodies(bodies, brief)
    assert len(result) == 2
    for body in result:
        assert body.startswith("# Prior wave context:")
        assert brief in body
        assert "---" in body
    assert "original A body" in result[0]
    assert "original B body" in result[1]


def test_inject_brief_with_empty_brief_returns_originals():
    from fno.megatron.brief import inject_brief_into_bodies

    bodies = ["x", "y"]
    assert inject_brief_into_bodies(bodies, "") == bodies
    assert inject_brief_into_bodies(bodies, None) == bodies


# ── Discoveries-field back-compat (plan ab-bc919f7f) ────────────────────
# The completion JSON now carries a `discoveries` field sourced from the
# session's HANDOFF.md by the target stop hook. The brief assembler reads
# it first and falls back to `body` for in-flight missions whose
# completion JSONs predate this change.


def test_brief_reads_discoveries_field_when_present():
    """New completion shape: discoveries field drives the brief.

    The producer (hooks/target-stop-hook.sh) writes the EXTRACTED SECTION
    BODY (no `### Discoveries` header) into this field. The fixture
    deliberately matches that shape so the consumer is exercised against
    real producer output, not a re-headered convenience string. A prior
    fixture that included the header masked a critical extraction bug on
    PR #256 (Codex P1 / Gemini critical).
    """
    from fno.megatron.brief import assemble_wave_brief

    completes = [
        {
            "wave": 1,
            "from": "alpha",
            "msg_id": "msg-001",
            "discoveries": "- Found A.\n- Found B.\n",
        }
    ]
    brief = assemble_wave_brief(completes_for_wave=completes, wave=1)
    assert "Found A" in brief
    assert "Found B" in brief
    assert "(no discoveries reported)" not in brief
    # Header is supplied by the consumer (`## From alpha (msg-001):`); it
    # MUST NOT also include a `### Discoveries` line because the producer
    # never writes one.
    assert "### Discoveries" not in brief


def test_brief_falls_back_to_body_when_no_discoveries_field():
    """Back-compat: completion records with body: still render."""
    from fno.megatron.brief import assemble_wave_brief

    completes = [
        {
            "wave": 1,
            "from": "alpha",
            "msg_id": "msg-001",
            "body": "### Discoveries\n- Legacy bullet.\n",
        }
    ]
    brief = assemble_wave_brief(completes_for_wave=completes, wave=1)
    assert "Legacy bullet" in brief


def test_brief_discoveries_takes_precedence_over_body():
    """When both fields exist, discoveries wins.

    `discoveries` carries producer-extracted body (header-less); `body`
    is the legacy full markdown including the heading. The consumer
    treats `discoveries` as already-extracted and skips re-running the
    heading-anchored regex.
    """
    from fno.megatron.brief import assemble_wave_brief

    completes = [
        {
            "wave": 1,
            "from": "alpha",
            "msg_id": "msg-001",
            "discoveries": "- New world.\n",
            "body": "### Discoveries\n- Legacy world.\n",
        }
    ]
    brief = assemble_wave_brief(completes_for_wave=completes, wave=1)
    assert "New world" in brief
    assert "Legacy world" not in brief


def test_brief_explicit_empty_discoveries_does_not_fall_back_to_body():
    """An explicit empty `discoveries` from the new producer is RESPECTED.

    Pins the back-compat contract documented at the producer site
    (hooks/target-stop-hook.sh): post-spec completion JSONs always carry
    the field so consumers can distinguish "field present, intentionally
    empty" (the producer ran, found no discoveries) from "field absent"
    (legacy record predating the spec). A falsy `or` chain would silently
    fall back to a stale `body` and erase that distinction; this test
    locks the explicit key-presence semantics.
    """
    from fno.megatron.brief import assemble_wave_brief

    completes = [
        {
            "wave": 1,
            "from": "alpha",
            "msg_id": "msg-001",
            "discoveries": "",  # new producer: ran, extracted nothing
            "body": "### Discoveries\n- Stale legacy bullet should NOT leak.\n",
        }
    ]
    brief = assemble_wave_brief(completes_for_wave=completes, wave=1)
    assert "Stale legacy bullet" not in brief
    assert "(no discoveries reported)" in brief


def test_brief_empty_discoveries_and_absent_field_both_render_fallback():
    """Empty discoveries string and absent field both render the fallback."""
    from fno.megatron.brief import assemble_wave_brief

    completes = [
        {"wave": 1, "from": "alpha", "msg_id": "msg-001", "discoveries": ""},
        {"wave": 1, "from": "beta", "msg_id": "msg-002"},  # no field at all
    ]
    brief = assemble_wave_brief(completes_for_wave=completes, wave=1)
    assert brief.count("(no discoveries reported)") == 2
