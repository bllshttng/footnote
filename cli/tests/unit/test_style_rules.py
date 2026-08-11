"""Tests for the five-rule style checker (``cli/src/fno/style.py``).

Each rule is covered positive and negative, plus the two deliberate sharp
edges: rule 4 must not flag the possessive "agent's", and rule 5 must skip the
hyphenated compounds "if-branch" and "when-clause". Every masking construct is
exercised, since "code does not count" is the load-bearing exemption.
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


# --- format_violations --------------------------------------------------------

def test_format_returns_empty_when_clean():
    assert style.format_violations([]) == ""


def test_format_names_each_rule():
    text = "you should run it."
    msg = style.format_violations(style.check(text))
    assert "rule 3" in msg
    assert "modal" in msg


def test_the_refusal_message_passes_its_own_rules():
    # The gate must not violate its own rule. The refusal message is itself
    # style-checked; every banned word it names is quoted, so masking exempts it.
    text = "you should don't; run if x."
    msg = style.format_violations(style.check(text))
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
