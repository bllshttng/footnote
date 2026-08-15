"""Tests for the six-rule style checker (``cli/src/fno/style.py``).

Each rule is covered positive and negative, plus the two deliberate sharp
edges: rule 4 must not flag the possessive "agent's", and rule 5 must skip the
hyphenated compounds "if-branch" and "when-clause". Every masking construct is
exercised, since "code does not count" is the load-bearing exemption.

Rule 6 gets the widest negative coverage of the six. It is the only rule that
reads a PAIR of lines, so every block that legally owns a newline (blank line,
list marker, heading, table row, fence, frontmatter, thematic break) needs its
own case. A false positive there refuses correct markdown.
"""
from __future__ import annotations

from fno import style


def rule_set(text: str) -> set[int]:
    return {v.rule for v in style.check(text)}


def rule_one(text: str) -> int | None:
    hits = [v.rule for v in style.check(text)]
    return hits[0] if hits else None


# --- Rule 1: length, split on block type -------------------------------------

def test_paragraph_over_25_words_fails():
    words = " ".join("w" for _ in range(26))
    assert 1 in rule_set(words + ".")


def test_paragraph_at_25_words_passes():
    words = " ".join("w" for _ in range(25))
    assert not rule_set(words + ".")


def test_list_item_uses_the_20_cap():
    body = " ".join("w" for _ in range(21))
    assert 1 in rule_set("- " + body + ".")
    body = " ".join("w" for _ in range(20))
    assert not rule_set("- " + body + ".")


def test_numbered_list_item_uses_the_20_cap():
    body = " ".join("w" for _ in range(21))
    assert 1 in rule_set("1. " + body + ".")


def test_block_type_not_sentence_mood():
    # Same 24-word body: a paragraph passes, but as a list item it is under the
    # 20 cap only if shorter. Here 24 words passes as a paragraph.
    body = " ".join("w" for _ in range(24))
    assert not rule_set(body + ".")
    assert 1 in rule_set("- " + body + ".")


# --- Rule 2: semicolon --------------------------------------------------------

def test_semicolon_fails():
    assert 2 in rule_set("do one thing; do another.")


def test_no_semicolon_passes_rule_2():
    assert 2 not in rule_set("do one thing. do another.")


# --- Rule 3: modals -----------------------------------------------------------

def test_banned_modal_fails():
    for word in ("should", "would", "might", "could"):
        assert 3 in rule_set(f"you {word} run it."), word


def test_lowercase_may_fails():
    assert 3 in rule_set("that may break.")


def test_capital_may_the_month_passes():
    assert 3 not in rule_set("the release shipped in May.")


def test_approved_modals_pass():
    for word in ("can", "will", "must"):
        assert 3 not in rule_set(f"you {word} run it."), word


def test_modal_inside_code_span_passes():
    assert 3 not in rule_set("run `should` as a literal token.")


# --- Rule 4: contractions (closed list) --------------------------------------

def test_contraction_fails():
    assert 4 in rule_set("do not leave it open." .replace("do not", "don't"))


def test_possessive_is_not_flagged():
    # The false positive a regex pattern would produce. Closed list is clean.
    assert 4 not in rule_set("the agent's body is the surface.")
    assert 4 not in rule_set("parse the agents' rows.")


def test_curly_apostrophe_contraction_fails():
    assert 4 in rule_set("it is ready".replace("it is", "it’s"))


# --- Rule 5: condition before command ----------------------------------------

def test_condition_after_command_fails():
    assert 5 in rule_set("run the check if the build is green.")


def test_condition_first_passes():
    assert 5 not in rule_set("if the build is green, run the check.")


def test_capital_condition_first_passes():
    assert 5 not in rule_set("If the build is green, run the check.")


def test_when_after_command_fails():
    assert 5 in rule_set("stop when the queue is empty.")


def test_hyphenated_compounds_skip():
    assert 5 not in rule_set("an if-branch is conditional.")
    assert 5 not in rule_set("a when-clause gates the run.")


# --- Rule 6: a paragraph is one physical line --------------------------------

def test_newline_inside_a_paragraph_fails():
    assert 6 in rule_set("the gate refuses a break\ninside this paragraph.")


def test_one_line_paragraph_passes():
    assert 6 not in rule_set("the gate refuses a break inside this paragraph.")


def test_many_sentences_on_one_line_pass():
    assert not rule_set("first sentence here. second sentence here. third one here.")


def test_blank_line_starts_the_next_paragraph():
    assert 6 not in rule_set("first paragraph here.\n\nsecond paragraph here.")


def test_list_items_are_legal_breaks():
    assert 6 not in rule_set("- first item here.\n- second item here.\n- third item here.")


def test_lazy_list_continuation_fails():
    # A bare prose line under a list item is a break inside that item.
    assert 6 in rule_set("- the item starts here\nand runs onto this line.")


def test_prose_after_a_heading_passes():
    assert 6 not in rule_set("# The heading\nthe paragraph under it.")


def test_prose_after_a_table_passes():
    body = "intro line here.\n| a | b |\n|---|---|\nclosing line here."
    assert 6 not in rule_set(body)


def test_prose_after_a_fence_passes():
    assert 6 not in rule_set("intro here.\n```\ncode()\n```\nclosing here.")


def test_prose_after_frontmatter_passes():
    assert 6 not in rule_set("---\nkey: value\n---\nthe body line.")


def test_prose_after_a_thematic_break_passes():
    assert 6 not in rule_set("first paragraph.\n---\nsecond paragraph.")


def test_blockquote_lines_are_legal_breaks():
    assert 6 not in rule_set("> quoted line one.\n> quoted line two.")


def test_raw_html_block_lines_are_legal_breaks():
    # A <details> block was the live false positive: three consecutive lines of
    # valid markdown that rule 6 refused with no correct fix available.
    body = "<details>\n<summary>the summary</summary>\nthe body line.\n</details>"
    assert 6 not in rule_set(body)


def test_setext_underline_is_a_legal_break():
    assert 6 not in rule_set("The heading\n===\nthe paragraph under it.")


def test_a_short_setext_underline_is_a_legal_break():
    # A setext h2 closes on one dash. Sharing the thematic-break repeat group
    # put a three-character floor on it and read the heading as a wrap.
    assert 6 not in rule_set("The heading\n-\nthe paragraph under it.")
    assert 6 not in rule_set("The heading\n--\nthe paragraph under it.")


def test_paren_ordered_lists_are_legal_breaks():
    # CommonMark reads `1)` exactly as `1.`.
    assert 6 not in rule_set("1) first item.\n2) second item.\n3) third item.")


def test_paren_ordered_item_gets_the_list_cap():
    # The same miss quietly gave these items the 25-word paragraph cap.
    body = " ".join("w" for _ in range(21))
    assert 1 in rule_set("1) " + body + ".")


def test_a_pipeless_table_is_not_charged():
    # THREE body rows on purpose. A one-row table passes even when only the
    # delimiter row is blanked, so a single-row fixture pinned nothing and let
    # a real multi-row table keep failing from its second row on.
    body = "intro.\n\na | b\n--- | ---\n1 | 2\n3 | 4\n5 | 6\n\nafter."
    assert 6 not in rule_set(body)


def test_a_pipeless_table_header_is_not_charged_rule_6():
    # The header is proven a table row by the delimiter row BELOW it, so it
    # needs a lookahead. Without one it was charged rule 6 while every body row
    # was waived, which applies one rule to one row of a table and not the rest.
    body = "Intro paragraph.\n\nflag | what it does\n--- | ---\nx | y\n"
    assert 6 not in rule_set(body)


def test_prose_after_a_pipeless_table_keeps_every_other_rule():
    # The hole that survived two fixes. A pipeless row is shaped like a sentence
    # carrying a pipe, so blanking it in the mask waived ALL six rules. The
    # waiver is rule 6 only now, and the proof is that the sentence reports the
    # same rules under a table as it does standing alone.
    bad = (
        "You should use a | b here and it is a very long sentence with lots and "
        "lots and lots of extra words beyond the cap; really."
    )
    assert rule_set("a | b\n--- | ---\n1 | 2\n" + bad + "\n") == rule_set(bad + "\n")
    assert {1, 2, 3} <= rule_set(bad + "\n")


def test_a_mixed_pipe_table_body_row_is_not_charged_rule_6():
    # A delimiter row written WITH pipes above body rows written without them is
    # valid GFM: leading pipes are per-row optional.
    assert 6 not in rule_set("| a | b |\n| --- | --- |\n1 | 2\n3 | 4\n")


def test_table_state_does_not_leak_past_a_fence():
    # The severe one. The table flag was cleared only on the fall-through path,
    # so a fence straight after a table carried it onward and blanked the next
    # prose line holding a pipe. That line escaped ALL six rules, not just 6.
    body = "a | b\n--- | ---\nc | d\n```\ncode\n```\nUse a | b; you should stop.\n"
    assert {2, 3} <= rule_set(body)


def test_table_state_does_not_leak_past_indented_code():
    body = "a | b\n--- | ---\nc | d\n    indented\nUse a | b; you should stop.\n"
    assert {2, 3} <= rule_set(body)


def test_a_paragraph_above_a_setext_underline_is_not_a_table_header():
    # The lookahead must not swallow this: a delimiter row needs pipes, and
    # `---` alone is a setext underline.
    assert 6 not in rule_set("The heading\n---\nthe paragraph under it.")


def test_a_table_stops_exempting_once_it_ends():
    # Position is what exempts a body row, so the exemption must end with the
    # table. Otherwise every pipe-free wrap after one would go unchecked.
    body = "a | b\n--- | ---\n1 | 2\n\nprose that wraps\nonto a second line."
    assert 6 in rule_set(body)


def test_prose_carrying_a_pipe_is_still_checked():
    # The delimiter row alone is blanked. Blanking a body-row shape would let
    # any sentence carrying a pipe escape all six rules.
    assert 4 in rule_set("run a | b and don't stop.")


def test_link_reference_definitions_are_legal_breaks():
    assert 6 not in rule_set("[one]: https://a.example\n[two]: https://b.example")


def test_crlf_frontmatter_does_not_read_as_prose():
    # Every anchored block test failed at once on CRLF, so frontmatter stayed
    # unblanked and rule 6 fired down the whole block.
    assert 6 not in rule_set("---\r\nkey: value\r\n---\r\nthe body line.\r\n")


def test_field_lines_are_legal_breaks():
    # The worker return grammar AGENTS.md mandates. Rule 6 refused it outright,
    # which left a codex worker no legal way to report.
    assert 6 not in rule_set("RESULT: SUCCESS\nTASK: 2.1")
    assert 6 not in rule_set("status: green\npr: 123\nbranch: feature/x")


def test_a_colon_later_in_the_line_does_not_exempt_a_wrap():
    # The field key is one word with no space, so a wrapped sentence carrying a
    # colon partway through is still a wrap.
    assert 6 in rule_set("the gate refuses this: a paragraph\nbroken across lines.")


def test_check_lines_never_charges_a_line_it_was_not_given():
    # The deliberate blind spot. A new line inserted directly ABOVE untouched
    # prose splits that paragraph and goes unreported, because charging the
    # untouched line below would fail a one-line edit in any legacy doc.
    # Documented in docs/style-rules.md beside the rule 1 and rule 4 trades.
    text = "this added line starts it\nan untouched continuation.\n"
    assert style.check_lines(text, {1}) == []
    reported = {v.sentence_index + 1 for v in style.check_lines(text, {2}) if v.rule == 6}
    assert reported == {2}


def test_check_lines_sees_the_unchanged_line_above():
    # The added line continues prose that the diff never touched, so rule 6
    # must still fire. State advances on every line, not only checked ones.
    text = "an unchanged paragraph line\nthis added line continues it.\n"
    assert 6 in {v.rule for v in style.check_lines(text, {2})}


def test_check_lines_first_added_line_after_a_blank_passes():
    text = "unchanged paragraph.\n\nthis added line starts its own.\n"
    assert 6 not in {v.rule for v in style.check_lines(text, {3})}


# --- Masking: code does not count --------------------------------------------

def test_inline_code_span_is_one_word():
    # The code span holds ten words; masking collapses it to one. The sentence
    # is 24 words masked (would be 33 unmasked), so it passes the 25-word cap.
    body = "the " * 20 + "span `one two three four five six seven eight nine ten` ends."
    assert 1 not in rule_set(body)


def test_path_does_not_split_the_sentence():
    body = "edit cli/src/fno/style.py to add the rule."
    assert not rule_set(body)


def test_flag_is_one_word():
    body = "pass --style-exception with a reason to bypass."
    assert not rule_set(body)


def test_fenced_block_removed_entirely():
    body = "intro line.\n\n```\n" + ("word " * 60) + "\n```\n\nclosing line."
    assert not rule_set(body)


def test_indented_code_block_removed():
    body = "intro.\n    code_line_with_semicolon; and_modal should\noutro."
    assert not rule_set(body)


def test_frontmatter_removed():
    body = "---\nkey: don't do this\n---\nbody line."
    assert not rule_set(body)


def test_frontmatter_does_not_misalign_block_type():
    # Frontmatter is blanked line-for-line (not stripped), so the raw/masked line
    # zip stays aligned and a long list item after frontmatter gets the 20-word
    # list cap, not the 25-word paragraph cap.
    body = "---\nkey: value\n---\n- " + " ".join("w" for _ in range(22)) + "."
    assert 1 in rule_set(body)


def test_markdown_link_line_is_not_a_log_line():
    # [See](x.md) is a markdown link, not a log level. It must not be blanked, or
    # a contraction in the line would slip past rule 4.
    assert 4 in rule_set("[See](docs/x.md) the docs have don't in them.")


def test_same_line_html_comment_keeps_trailing_prose():
    # A comment that opens and closes on one line strips only the span, so prose
    # trailing it is still checked.
    assert 4 in rule_set("<!-- note --> trailing prose has don't in it.")


def test_check_lines_skips_an_added_line_inside_an_existing_fence():
    # The fence delimiters are unchanged lines; only the code line is "added".
    # check_lines masks the whole file, so the added code line is blanked (code)
    # and not flagged for its semicolon.
    text = (
        "intro prose here.\n\n"
        "```\nif ready; then echo hi; fi\n```\n\n"
        "close prose.\n"
    )
    assert style.check_lines(text, {4}) == []


def test_check_lines_still_checks_added_prose():
    # An added prose line outside any fence is checked normally.
    text = "intro.\nthis added line uses should trip the modal rule.\noutro.\n"
    assert 3 in {v.rule for v in style.check_lines(text, {2})}


def test_html_comment_removed():
    body = "intro. <!-- don't should; x --> outro."
    assert not rule_set(body)


def test_table_row_removed():
    body = "intro.\n| don't | should |\n|---|---|\noutro."
    assert not rule_set(body)


def test_log_line_removed():
    body = "intro.\n[ERROR] don't should; failed\noutro."
    assert not rule_set(body)


def test_link_text_kept_target_dropped():
    body = "see the [style rules](docs/style-rules.md) page."
    assert not rule_set(body)


def test_multiple_modals_in_one_sentence_each_report():
    text = "you should run it and you would try it."
    violations = style.check(text)
    assert sum(1 for v in violations if v.rule == 3) == 2


def test_multiple_contractions_in_one_sentence_each_report():
    text = "don't stop and don't wait."
    violations = style.check(text)
    assert sum(1 for v in violations if v.rule == 4) == 2


# --- format_violations --------------------------------------------------------

def test_format_returns_empty_when_clean():
    assert style.format_violations([]) == ""


def test_format_names_each_rule():
    text = "you should run it."
    msg = style.format_violations(style.check(text))
    assert "rule 3" in msg
    assert "modal" in msg


def test_format_reports_every_violation_class_in_one_pass():
    # The measured failure: four distinct rule classes across a body, all
    # named in one refusal instead of costing four round trips.
    body = (
        " ".join("w" for _ in range(26)) + ".\n\n"
        "do one thing; do another.\n\n"
        "you should run it.\n\n"
        "stop when the queue is empty.\n"
    )
    msg = style.format_violations(style.check(body))
    for rule in (1, 2, 3, 5):
        assert f"rule {rule}" in msg


def test_format_groups_by_rule_number():
    text = "you should try it and you would run it."
    msg = style.format_violations(style.check(text))
    first = msg.index("rule 3")
    second = msg.index("rule 3", first + 1)
    assert first < second


def test_format_quotes_an_excerpt_of_the_offending_text():
    text = "you should run it."
    msg = style.format_violations(style.check(text))
    assert '"you should run it."' in msg


def test_format_caps_the_excerpt_at_twelve_words():
    words = " ".join("w" for _ in range(26))
    msg = style.format_violations(style.check(words + "."))
    assert "..." in msg
    assert " ".join(["w"] * 13) not in msg


def test_format_names_the_local_dry_run():
    msg = style.format_violations(style.check("you should run it."))
    assert "fno lint style --stdin" in msg


def test_format_excerpt_survives_an_inner_double_quote():
    # A sentence carrying a literal double quote must not close the wrapping
    # quote early and expose the rest of the excerpt to the word count.
    text = 'you should run "the check" now.'
    msg = style.format_violations(style.check(text))
    assert style.check(msg) == [], msg


def test_the_refusal_message_passes_its_own_rules():
    # The gate must not violate its own rule. The refusal message is itself
    # style-checked; every banned word it names is quoted, so masking exempts it.
    text = "you should don't; run if x."
    msg = style.format_violations(style.check(text))
    assert style.check(msg) == [], msg


def test_the_refusal_message_survives_a_rule_6_violation():
    # Rule 6 is the one rule whose refusal is multi-line by nature, so it is the
    # one most able to break the self-consistency invariant above.
    msg = style.format_violations(style.check("a paragraph broken\nacross two lines."))
    assert "rule 6" in msg
    assert style.check(msg) == [], msg


# --- has_exception ------------------------------------------------------------

def test_exception_line_returns_reason():
    assert style.has_exception("body\nstyle-exception: legacy inbox\n") == "legacy inbox"


def test_exception_html_comment_returns_reason():
    body = "body\n<!-- style-exception: historical doc -->\n"
    assert style.has_exception(body) == "historical doc"


def test_exception_empty_reason_is_none():
    assert style.has_exception("style-exception:   \n") is None


def test_no_exception_is_none():
    assert style.has_exception("plain body") is None


# --- Boundaries ---------------------------------------------------------------

def test_empty_body_is_clean():
    assert style.check("") == []


def test_only_a_code_fence_is_clean():
    assert style.check("```\nstuff\n```") == []


def test_no_ending_punctuation_is_still_counted():
    # One run-on sentence with no terminal punctuation still gets capped.
    body = " ".join("w" for _ in range(30))
    assert 1 in rule_set(body)
