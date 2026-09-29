"""Tests for the eight-rule style checker (``cli/src/fno/style.py``).

One table test per rule family, one row per distinct branch; regression
tests that pin a measured false positive keep their own function.
"""

from __future__ import annotations

import re

import pytest
import typer

from fno import style


def rule_set(text: str) -> set[int]:
    return {v.rule for v in style.check(text, surface="pr-body")}


# --- Rule 1: word caps by block type -----------------------------------------

def test_word_cap_boundaries_by_block_type():
    rows = [
        # (text builder, cap, must_fire)
        (lambda n: " ".join("w" for _ in range(n)) + ".", 25, False),
        (lambda n: " ".join("w" for _ in range(n)) + ".", 26, True),
        (lambda n: "- " + " ".join("w" for _ in range(n)) + ".", 20, False),
        (lambda n: "- " + " ".join("w" for _ in range(n)) + ".", 21, True),
        (lambda n: "1. " + " ".join("w" for _ in range(n)) + ".", 20, False),
        (lambda n: "1) " + " ".join("w" for _ in range(n)) + ".", 21, True),
    ]
    for build, cap, must_fire in rows:
        got = rule_set(build(cap + (1 if must_fire else 0)))
        assert (1 in got) is must_fire, build(1)


def test_block_type_not_sentence_mood():
    body = " ".join("w" for _ in range(24))
    assert not rule_set(body + ".")
    assert 1 in rule_set("- " + body + ".")


def test_no_ending_punctuation_is_still_counted():
    body = " ".join("w" for _ in range(30))
    assert 1 in rule_set(body)


# --- Rule 7: mail word cap ----------------------------------------------------

def test_mail_word_cap_boundaries():
    rows = [
        (" ".join("word" for _ in range(80)) + ".", False),
        (" ".join("word" for _ in range(81)) + ".", True),
        ("under the cap.\n```\n" + "\n".join("word" for _ in range(200)) + "\n```", False),
    ]
    for body, must_fire in rows:
        got = {v.rule for v in style.check(body, surface="mail")}
        assert (7 in got) is must_fire


def test_mail_cap_names_both_counts():
    body = " ".join("word" for _ in range(81)) + "."
    wordcap = next(v for v in style.check(body, surface="mail") if v.rule == 7)
    assert "81" in wordcap.detail
    assert "80" in wordcap.detail


def test_word_cap_is_not_used_on_other_surfaces_or_added_lines():
    body = " ".join("word" for _ in range(81)) + "."
    assert 7 not in {v.rule for v in style.check(body, surface="pr-body")}
    assert 7 not in {v.rule for v in style.check_lines(body, {1})}


# --- Word rules: fail and pass rows per rule ----------------------------------

def test_word_rules_fail_and_pass_rows():
    rows = [
        (2, "do one thing; do another.", ["do one thing. do another."]),
        (
            3,
            "that may break.",
            [
                "the release shipped in May.",
                "run `should` as a literal token.",
                "you can run it.",
                "you will run it.",
                "you must run it.",
            ],
        ),
        (
            4,
            "don't leave it open.",
            ["the agent's body is the surface.", "parse the agents' rows."],
        ),
        (
            5,
            "run the check if the build is green.",
            [
                "if the build is green, run the check.",
                "If the build is green, run the check.",
                "an if-branch is conditional.",
                "a when-clause gates the run.",
            ],
        ),
        (5, "stop when the queue is empty.", []),
        (
            8,
            "please rerun the suite.",
            [
                "I think the claim is stale.",
                "I just resumed the worker.",
                "make sure CI is green.",
                "confirm CI actually ran green.",
                "the claim is almost certainly stale.",
                "only resume what the shutdown really killed.",
                "run it clearly labeled as such.",
            ],
        ),
    ]
    for rule, bad, good in rows:
        assert rule in rule_set(bad), bad
        for text in good:
            assert rule not in rule_set(text), text


def test_banned_modals_and_filler_each_report():
    for word in ("should", "would", "might", "could"):
        assert 3 in rule_set(f"you {word} run it."), word
    for word in ("please", "thanks", "basically"):
        assert 8 in rule_set(f"{word} rerun the suite."), word
    for phrase in ("thank you", "of course", "happy to", "feel free"):
        assert 8 in rule_set(f"and {phrase} the merge waits."), phrase


def test_curly_apostrophe_contraction_fails():
    assert 4 in rule_set("it is ready".replace("it is", "it’s"))


def test_filler_match_is_case_insensitive():
    assert 8 in rule_set("Please re-attest the new head.")
    assert 8 in rule_set("Of course the merge waits.")


def test_filler_rule_runs_on_every_surface():
    for surface in ("mail", "pr-body", "comment", "markdown"):
        assert 8 in {v.rule for v in style.check("thanks for merging 1520.", surface=surface)}, surface


def test_fix_does_not_delete_a_filler():
    fixed, residue = style.fix("please rerun the suite", surface="pr-body")
    assert fixed == "please rerun the suite"
    assert [v.rule for v in residue] == [8]


# --- Mail runs the full rule set ----------------------------------------------

def test_mail_runs_full_rule_set_under_the_cap():
    body = " ".join("word" for _ in range(77)) + " a; b; c"
    assert 2 in {v.rule for v in style.check(body, surface="mail")}
    body = "you should always run it; and don't stop if it fails."
    assert {2, 3, 4, 5} <= {v.rule for v in style.check(body, surface="mail")}
    body = " ".join("word" for _ in range(26)) + "."
    assert 1 in {v.rule for v in style.check(body, surface="mail")}


# --- Rule 6: a paragraph is one physical line ---------------------------------

def test_rule_6_wraps_fail_and_one_line_passes():
    assert 6 in rule_set("the gate refuses a break\ninside this paragraph.")
    assert 6 not in rule_set("the gate refuses a break inside this paragraph.")
    assert not rule_set("first sentence here. second sentence here. third one here.")
    assert 6 in rule_set("- the item starts here\nand runs onto this line.")
    assert 6 in rule_set("the gate refuses this: a paragraph\nbroken across lines.")


def test_rule_6_legal_breaks_one_row_per_block_type():
    rows = [
        "first paragraph here.\n\nsecond paragraph here.",
        "- first item here.\n- second item here.\n- third item here.",
        "1) first item.\n2) second item.\n3) third item.",
        "# The heading\nthe paragraph under it.",
        "intro line here.\n| a | b |\n|---|---|\nclosing line here.",
        "intro here.\n```\ncode()\n```\nclosing here.",
        "---\nkey: value\n---\nthe body line.",
        "---\r\nkey: value\r\n---\r\nthe body line.\r\n",
        "first paragraph.\n---\nsecond paragraph.",
        "> quoted line one.\n> quoted line two.",
        "<details>\n<summary>the summary</summary>\nthe body line.\n</details>",
        "The heading\n===\nthe paragraph under it.",
        "The heading\n-\nthe paragraph under it.",
        "The heading\n--\nthe paragraph under it.",
        "The heading\n---\nthe paragraph under it.",
        "[one]: https://a.example\n[two]: https://b.example",
        "RESULT: SUCCESS\nTASK: 2.1",
        "status: green\npr: 123\nbranch: feature/x",
    ]
    for body in rows:
        assert 6 not in rule_set(body), body.splitlines()[0]


def test_pipeless_table_rows_are_exempt_from_rule_6_only():
    # THREE body rows on purpose: a one-row fixture pinned nothing.
    body = "intro.\n\na | b\n--- | ---\n1 | 2\n3 | 4\n5 | 6\n\nafter."
    assert 6 not in rule_set(body)
    # The header needs the delimiter row BELOW it (lookahead).
    body = "Intro paragraph.\n\nflag | what it does\n--- | ---\nx | y\n"
    assert 6 not in rule_set(body)
    # Leading pipes are per-row optional GFM.
    assert 6 not in rule_set("| a | b |\n| --- | --- |\n1 | 2\n3 | 4\n")


def test_prose_after_a_pipeless_table_keeps_every_other_rule():
    # A pipeless row is shaped like a sentence carrying a pipe; the waiver is
    # rule 6 only, and the sentence reports the same rules as standing alone.
    bad = (
        "You should use a | b here and it is a very long sentence with lots and "
        "lots and lots of extra words beyond the cap; really."
    )
    assert rule_set("a | b\n--- | ---\n1 | 2\n" + bad + "\n") == rule_set(bad + "\n")
    assert {1, 2, 3} <= rule_set(bad + "\n")


def test_table_state_does_not_leak_past_a_fence_or_indented_code():
    body = "a | b\n--- | ---\nc | d\n```\ncode\n```\nUse a | b; you should stop.\n"
    assert {2, 3} <= rule_set(body)
    body = "a | b\n--- | ---\nc | d\n    indented\nUse a | b; you should stop.\n"
    assert {2, 3} <= rule_set(body)


def test_a_table_stops_exempting_once_it_ends():
    body = "a | b\n--- | ---\n1 | 2\n\nprose that wraps\nonto a second line."
    assert 6 in rule_set(body)


def test_prose_carrying_a_pipe_is_still_checked():
    assert 4 in rule_set("run a | b and don't stop.")


def test_frontmatter_does_not_misalign_block_type():
    body = "---\nkey: value\n---\n- " + " ".join("w" for _ in range(22)) + "."
    assert 1 in rule_set(body)


# --- Masking: code does not count ----------------------------------------------

def test_masking_removes_nonprose_constructs():
    rows = [
        "intro line.\n\n```\n" + ("word " * 60) + "\n```\n\nclosing line.",
        "intro.\n    code_line_with_semicolon; and_modal should\noutro.",
        "---\nkey: don't do this\n---\nbody line.",
        "intro. <!-- don't should; x --> outro.",
        "intro.\n| don't | should |\n|---|---|\noutro.",
        "intro.\n[ERROR] don't should; failed\noutro.",
        "see the [style rules](docs/style-rules.md) page.",
        "edit cli/src/fno/style.py to add the rule.",
        "pass --style-exception with a reason to bypass.",
        "the " * 20 + "span `one two three four five six seven eight nine ten` ends.",
    ]
    for body in rows:
        assert not rule_set(body), body.splitlines()[0]


def test_markdown_link_line_is_not_a_log_line():
    assert 4 in rule_set("[See](docs/x.md) the docs have don't in them.")


def test_same_line_html_comment_keeps_trailing_prose():
    assert 4 in rule_set("<!-- note --> trailing prose has don't in it.")


# --- check_lines: added-line mode ----------------------------------------------

def test_check_lines_never_charges_a_line_it_was_not_given():
    text = "this added line starts it\nan untouched continuation.\n"
    assert style.check_lines(text, {1}) == []
    reported = {v.sentence_index + 1 for v in style.check_lines(text, {2}) if v.rule == 6}
    assert reported == {2}


def test_check_lines_sees_state_from_untouched_lines():
    text = "an unchanged paragraph line\nthis added line continues it.\n"
    assert 6 in {v.rule for v in style.check_lines(text, {2})}
    text = "unchanged paragraph.\n\nthis added line starts its own.\n"
    assert 6 not in {v.rule for v in style.check_lines(text, {3})}


def test_check_lines_skips_an_added_line_inside_an_existing_fence():
    text = (
        "intro prose here.\n\n"
        "```\nif ready; then echo hi; fi\n```\n\n"
        "close prose.\n"
    )
    assert style.check_lines(text, {4}) == []


def test_check_lines_still_checks_added_prose():
    text = "intro.\nthis added line uses should trip the modal rule.\noutro.\n"
    assert 3 in {v.rule for v in style.check_lines(text, {2})}


# --- format_violations ---------------------------------------------------------

def test_format_names_each_rule_and_groups_by_number():
    msg = style.format_violations(style.check("you should run it.", surface="pr-body"))
    assert "rule 3" in msg
    assert "modal" in msg
    assert '"you should run it."' in msg
    assert "fno doctor lint style --stdin" in msg
    msg = style.format_violations(style.check("you should try it and you would run it.", surface="pr-body"))
    first = msg.index("rule 3")
    assert first < msg.index("rule 3", first + 1)


def test_format_reports_every_violation_class_in_one_pass():
    body = (
        " ".join("w" for _ in range(26)) + ".\n\n"
        "do one thing; do another.\n\n"
        "you should run it.\n\n"
        "stop when the queue is empty.\n"
    )
    msg = style.format_violations(style.check(body, surface="pr-body"))
    for rule in (1, 2, 3, 5):
        assert f"rule {rule}" in msg


def test_format_caps_the_excerpt_at_twelve_words():
    words = " ".join("w" for _ in range(26))
    msg = style.format_violations(style.check(words + ".", surface="pr-body"))
    assert "..." in msg
    assert " ".join(["w"] * 13) not in msg


def test_format_points_rule_7_at_a_check_that_sees_the_cap():
    body = " ".join("word" for _ in range(81)) + "."
    msg = style.format_violations(style.check(body, surface="mail"))
    assert "--surface mail" in msg
    assert "--surface pr-body" not in msg
    msg = style.format_violations(style.check(body, surface="encounter"), surface="encounter")
    assert "--surface encounter" in msg
    assert "--surface mail" not in msg
    msg = style.format_violations(style.check("you should run it.", surface="pr-body"))
    assert "--surface pr-body" in msg


def test_format_adds_word_cap_recipe_only_for_rule_7():
    body = " ".join("word" for _ in range(81)) + "."
    wordcap_msg = style.format_violations(style.check(body, surface="mail"))
    assert "Cut articles, filler, pleasantries, hedges" in wordcap_msg
    assert "Status:" in wordcap_msg
    other_msg = style.format_violations(style.check("you should run it.", surface="pr-body"))
    assert "Cut articles" not in other_msg
    assert "Status:" not in other_msg


def test_format_excerpt_and_details_survive_quotes():
    text = 'you should run "the check" now.'
    msg = style.format_violations(style.check(text, surface="pr-body"))
    assert style.check(msg, surface="pr-body") == [], msg
    text = 'you should run and would" stop.'
    msg = style.format_violations(style.check(text, surface="pr-body"))
    assert style.check(msg, surface="pr-body") == [], msg


def test_format_names_the_mention_escape_for_word_rules():
    msg = style.format_violations(style.check("you should run it.", surface="pr-body"))
    assert "A quoted word is a mention, not a use." in msg
    assert 'Wrap "should" in double quotes or backticks to name it.' in msg
    rule4_msg = style.format_violations(style.check("don't run it.", surface="pr-body"))
    assert 'Wrap "don\'t" in double quotes or backticks' in rule4_msg
    body = " ".join("word" for _ in range(81)) + "."
    length_msg = style.format_violations(style.check(body, surface="pr-body"))
    assert "A quoted word is a mention" not in length_msg


def test_the_refusal_message_passes_its_own_rules():
    # The gate must not violate its own rule; every banned word it names is
    # quoted, so masking exempts it.
    msg = style.format_violations(style.check("you should don't; run if x.", surface="pr-body"))
    assert style.check(msg, surface="pr-body") == [], msg
    msg = style.format_violations(style.check("a paragraph broken\nacross two lines.", surface="pr-body"))
    assert "rule 6" in msg
    assert style.check(msg, surface="pr-body") == [], msg


def test_rule_7_refusal_with_recipe_passes_rules_1_to_6():
    sentences = [
        " ".join("word" for _ in range(count)) + "."
        for count in (20, 20, 20, 21)
    ]
    body = " ".join(sentences)
    msg = style.format_violations(style.check(body, surface="mail"))
    assert style.check(msg, surface="pr-body") == [], msg
    assert len(style._mask(msg).split()) <= style.MESSAGE_WORD_CAP


def test_the_refusal_message_passes_at_the_mail_surface():
    msg = style.format_violations(style.check("you should do this; now.", surface="mail"))
    assert style.check(msg, surface="mail") == [], msg


def test_markdown_refusal_offers_no_marker_escape():
    violations = style.check("One line; two clauses.", surface="markdown")
    markdown = style.format_violations(violations, surface="markdown")
    assert "style-exception" not in markdown
    assert "--surface markdown --diff-base" in markdown
    assert "style-exception" in style.format_violations(violations, surface="pr-body")


def test_enforce_style_refuses_81_words_with_positive_marker(capsys, monkeypatch):
    from fno.mail.cli import _enforce_style

    monkeypatch.setenv("FNO_STYLE_ENFORCE", "1")
    body = " ".join("word" for _ in range(81)) + "."
    with pytest.raises(typer.Exit):
        _enforce_style(body)
    error = capsys.readouterr().err
    assert "rule 7" in error
    assert "81" in error and "80" in error


def test_enforce_style_accepts_cap_and_exception(monkeypatch):
    from fno.mail.cli import _enforce_style

    monkeypatch.setenv("FNO_STYLE_ENFORCE", "1")
    sentence = " ".join("word" for _ in range(20)) + "."
    _enforce_style(" ".join(sentence for _ in range(4)))
    _enforce_style(" ".join("word" for _ in range(81)) + ".\nstyle-exception: log payload")


# --- has_exception and boundaries ----------------------------------------------

def test_has_exception_rows():
    rows = [
        ("body\nstyle-exception: legacy inbox\n", "legacy inbox"),
        ("body\n<!-- style-exception: historical doc -->\n", "historical doc"),
        ("style-exception:   \n", None),
        ("plain body", None),
    ]
    for body, want in rows:
        assert style.has_exception(body) == want, body


def test_boundaries_are_clean():
    assert style.check("") == []
    assert style.check("```\nstuff\n```") == []


# --- fix(): the mechanical rewrite set (rules 2 and 6) --------------------------

def test_fix_splits_and_joins_then_round_trips():
    rows = [
        ("a body with a semicolon; and more", "a body with a semicolon. And more"),
        ("line one ends here\nand the wrapped half continues.", "line one ends here and the wrapped half continues."),
        ("first half here\nsecond half; then it ends.", "first half here second half. Then it ends."),
        ("it ends here;", "it ends here."),
    ]
    for text, want in rows:
        fixed, residue = style.fix(text, surface="pr-body")
        assert fixed == want, text
        assert residue == []
        assert style.check(fixed, surface="pr-body") == []


def test_fix_applies_what_it_can_and_names_the_residue():
    fixed, residue = style.fix("the runner should retry; then stop", surface="pr-body")
    assert fixed == "the runner should retry. Then stop"
    assert [v.rule for v in residue] == [3]


def test_fix_skips_lines_that_also_carry_code():
    text = "use `fmt` here; it is faster"
    fixed, residue = style.fix(text, surface="pr-body")
    assert fixed == text
    assert [v.rule for v in residue] == [2]


def test_fix_never_touches_a_fenced_block():
    text = "```bash\nmake; make install\n```\n"
    fixed, residue = style.fix(text, surface="pr-body")
    assert fixed == text
    assert residue == []


# --- fix(): the negation invariant ----------------------------------------------
# fix() is the only code that rewrites a mail body. A rewrite that dropped a
# negation would invert a sent instruction, so every fixable rewrite must keep
# the whole-word negation counts exactly.

_NEGATION_WORDS = ("not", "no", "never", "none", "nothing", "cannot", "without", "nor")


def _negation_counts(text: str) -> dict[str, int]:
    return {w: len(re.findall(rf"\b{w}\b", text.lower())) for w in _NEGATION_WORDS}


@pytest.mark.parametrize(
    "body",
    [
        # Positive control: the semicolon split rewrites the body, so the
        # assertion proves a rewrite happened and negations survived it.
        "do not merge; CI is not green",
        "never merge; there is no green build, nor a rerun without review",
        "cannot ship this\nnone of the checks pass; nothing else blocks",
        # No fixable violation: fix returns the input unchanged.
        "do not merge because CI is not green",
    ],
)
def test_fix_preserves_negation_words(body):
    fixed, _residue = style.fix(body, surface="pr-body")
    assert _negation_counts(fixed) == _negation_counts(body)


def test_fix_negation_control_rewrites_and_keeps_not():
    body = "do not merge; CI is not green"
    fixed, _residue = style.fix(body, surface="pr-body")
    assert fixed != body
    assert _negation_counts(fixed)["not"] == 2


# --- Multiple hits in one sentence ----------------------------------------------

def test_multiple_hits_in_one_sentence_each_report():
    text = "you should run it and you would try it."
    assert sum(1 for v in style.check(text, surface="pr-body") if v.rule == 3) == 2
    text = "don't stop and don't wait."
    assert sum(1 for v in style.check(text, surface="pr-body") if v.rule == 4) == 2
