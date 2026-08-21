"""Style checker for agent-authored text.

Seven rules, checked at the tool boundary. ``docs/style-rules.md`` is the
normative statement; this module is the mechanism. Pure: no filesystem, no
state, no network. ``fno mail send`` and the hand-run ``fno doctor lint style``
surfaces route through :func:`check`.

Code does not count against a sentence. A masking pass runs before any rule
and replaces each code construct with one placeholder token, so a 60-character
identifier costs one word. See :func:`_mask` for what is removed and what is
replaced.
"""
from __future__ import annotations

import re
from dataclasses import dataclass


# Rule numbers are the identity every caller and event carries. Do not renumber.
RULE_NAMES = {
    1: "length",
    2: "semicolon",
    3: "modal",
    4: "contraction",
    5: "condition",
    6: "wrap",
    7: "wordcap",
}

LIST_ITEM_CAP = 20
PARAGRAPH_CAP = 25
MESSAGE_WORD_CAP = 80

# Rule 3. Matched lowercase, whole-word. "may" is lowercase-only so the month
# "May" never fires; the other four carry no capitalized homonym in prose.
BANNED_MODALS = frozenset({"should", "would", "might", "could"})
BANNED_MODAL_MAY = "may"

# Rule 4. A closed list, matched case-insensitively after normalising the curly
# apostrophe U+2019 to U+0027. A regex pattern was rejected because it flags
# "the agent's body", a correct possessive. A missed contraction is a cheaper
# error than a refused correct sentence.
CONTRACTIONS = frozenset(
    {
        "ain't", "aren't", "can't", "could've", "couldn't", "daren't",
        "didn't", "doesn't", "don't", "hadn't", "hasn't", "haven't",
        "he'd", "he'll", "he's", "here's", "how'd", "how'll", "how's",
        "i'd", "i'll", "i'm", "i've", "isn't", "it'd", "it'll", "it's",
        "let's", "ma'am", "might've", "mightn't", "must've", "mustn't",
    "needn't", "o'clock", "oughtn't", "shan't", "she'd", "she'll", "she's",
        "should've", "shouldn't", "that'd", "that'll", "that's", "there's",
        "they'd", "they'll", "they're", "they've", "'tis", "'twas",
        "we'd", "we'll", "we're", "we've", "weren't", "what'll",
        "what're", "what's", "what've", "when's", "where's", "who'd",
        "who'll", "who're", "who's", "who've", "why's", "won't", "would've",
        "wouldn't", "y'all", "you'd", "you'll", "you're", "you've",
    }
)

# Rule 5. Matches "if"/"when" NOT followed by a word char or hyphen, so
# "if-branch" and "when-clause" skip (the hyphen), and "iffy"/"whenever" skip
# (the word char). Punctuation and end-of-sentence still count, so a trailing
# conditional is caught.
_CONDITION_RE = re.compile(r"\b(if|when)(?![\w-])", re.IGNORECASE)

# One neutral token. It is one word for counting and matches no rule itself.
_PLACEHOLDER = "x"

# Sentence-split guard: these abbreviations keep their period without splitting.
_ABBREVIATIONS = ("e.g.", "i.e.", "vs.", "etc.")

# Exception markers. The line form starts a physical line (mail/PR body); the
# comment form is the markdown escape. Both require a non-empty reason. A
# mid-sentence mention of the marker (in prose or a code span) does NOT count,
# so a doc that describes the escape is not exempted by the description.
_LINE_EXCEPTION_RE = re.compile(r"^\s*style-exception:\s*(.+?)\s*$", re.MULTILINE)
_COMMENT_EXCEPTION_RE = re.compile(r"<!--\s*style-exception:\s*(.+?)\s*-->")

# Rule 6. A paragraph is one physical line, so the only legal newline is one
# that starts the next block. Two kinds of block start matter, and the split is
# what keeps rule 6 from refusing correct markdown.
#
# OWN-LINE blocks take a whole line and cannot be continued, so the line after
# one is always legal: a heading, a setext underline, a thematic break, a raw
# HTML line, and a link reference definition. Every one of these was a live
# false positive before it was listed here.
#
# CONTINUABLE blocks start a block AND accept a lazy continuation, so a bare
# prose line under one IS a break inside a paragraph: a list item, a blockquote.
_HEADING_RE = re.compile(r"^\s{0,3}#{1,6}\s")
# Setext underlines and thematic breaks. The two need separate alternatives:
# a setext underline is legal at ONE character (`-` alone closes an h2), while a
# thematic break needs three. Folding them into the shared repeat group put the
# 3-char floor on both and read `Heading\n-\nbody.` as a wrapped paragraph.
_OWN_LINE_BREAK_RE = re.compile(
    r"^\s{0,3}(?:=+[ \t]*|-+[ \t]*|([-*_])[ \t]*(?:\1[ \t]*){2,})$"
)
# A block-level HTML open or close tag. A `<details>` block was the specimen.
_HTML_BLOCK_RE = re.compile(r"^\s{0,3}</?[A-Za-z][\w-]*")
# A link reference definition. Several in a row are normal and are not a wrap.
_REF_DEF_RE = re.compile(r"^\s{0,3}\[[^\]]+\]:\s")
# A `key: value` field line. Mail carries receipts and the worker return
# grammar AGENTS.md mandates, and `RESULT: SUCCESS` over `TASK: 2.1` is two
# fields rather than one wrapped sentence. Rule 6 refused that grammar outright,
# which left a codex worker no legal way to report. The key is ONE word with no
# space, so a wrapped sentence is not exempted by a colon later in the line.
# A prose line opening "Note: ..." is exempted too, and that missed break is the
# cheaper error, the same trade the rule 4 closed list makes.
_FIELD_LINE_RE = re.compile(r"^\s{0,3}[A-Za-z][\w.-]*:[ \t]")
_BLOCKQUOTE_RE = re.compile(r"^\s{0,3}>")

_FENCE_OPEN_RE = re.compile(r"^[ \t]*(`{3,}|~{3,})")
_LOG_RE = re.compile(
    r"^(\[[A-Z]{2,}\]|ERROR\b|WARN(?:ING)?\b|INFO\b|DEBUG\b|TRACE\b"
    r"|\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2})"
)
# `1)` is an ordered-list delimiter in CommonMark exactly as `1.` is. Accepting
# only the dot charged rule 6 against a correct `1)` list with no legal fix, and
# quietly gave those items the 25-word paragraph cap instead of the 20-word list
# cap. That second miss predates rule 6.
_LIST_MARKER_RE = re.compile(r"^\s{0,3}([-*+]|\d+[.)])\s+")
# A GFM delimiter row written without leading pipes, e.g. `--- | ---`.
_DELIMITER_ROW_RE = re.compile(r"^:?-+:?(?:[ \t]*\|[ \t]*:?-+:?)+[ \t]*$")
_FILENAME_RE = re.compile(r"\b[\w./-]+\.(?:py|sh|rs|ts|js|toml|ya?ml|json|md|txt|lock)\b")
_PATH_RE = re.compile(r"\b[A-Za-z][\w-]*(?:::|/)[\w./:-]*")
_URL_RE = re.compile(r"[A-Za-z][\w.+-]*://\S*")
_FLAG_RE = re.compile(r"(?<![\w-])(?:--[A-Za-z][\w-]*|-[A-Za-z])")
_QUOTE_RE = re.compile(r'"[^"]*"')
_CODE_DOUBLE_RE = re.compile(r"``([^`]+)``")
_CODE_SINGLE_RE = re.compile(r"`([^`]+)`")
_LINK_RE = re.compile(r"\[([^\]]+)\]\([^)]*\)")
_IDENT_RE = re.compile(r"\b[A-Za-z]\w*_\w+")
_COMMENT_SPAN_RE = re.compile(r"<!--.*?-->")


@dataclass
class Violation:
    """One rule breach on one sentence.

    ``sentence`` carries the masked text (code shown as the placeholder), so the
    word count reported in ``detail`` matches what the reader sees. ``detail`` is
    the actionable half: it names the sentence and the fix.

    Rule 6 is the exception to both field names. Its unit is a LINE, not a
    sentence, so it carries the 0-based line index and the whole masked line.
    Read ``detail`` rather than these two fields: it always names the right unit.
    """

    rule: int
    sentence_index: int
    sentence: str
    detail: str


def has_exception(text: str) -> str | None:
    """Return the bypass reason if the text carries a style-exception marker.

    Accepts both the line form (``style-exception: why``) and the HTML-comment
    form (``<!-- style-exception: why -->``), since both contain the marker. An
    empty reason does not count. Returns ``None`` when no marker is present.
    """
    match = _LINE_EXCEPTION_RE.search(text)
    if match is None:
        match = _COMMENT_EXCEPTION_RE.search(text)
    if match is None:
        return None
    reason = match.group(1).strip()
    return reason or None


def check(text: str, *, surface: str = "mail") -> list[Violation]:
    """Return every violation found in ``text``.

    Rule 7 is the first surface-scoped rule. Mail carries the 80-word prose cap;
    PR bodies, comments, and changed markdown do not, because they are not read
    mid-turn. The text is masked whole, then each line is split into its
    sentences: a paragraph is one physical line and carries as many sentences
    as it needs, and rule 6 is what holds that shape.

    The message cap stays here rather than in ``_run``. ``check_lines`` uses
    ``_run`` for added-lines markdown checks, where a whole-body count would
    charge a file's total against one added line. Masking also means a pasted log
    can count near zero words; the cap covers prose, not a log dump.
    """
    violations = _run(text, None)
    if surface == "mail":
        violations.extend(_check_message_length(text))
    return violations


def check_lines(text: str, line_numbers: set[int]) -> list[Violation]:
    """Check only the given 1-based lines, and report only on those lines.

    Rule 6 reads the line ABOVE a given line to decide whether it continues a
    paragraph, so context comes from the whole text. The violation is still
    charged to the given line, so this never annotates a line the caller did
    not pass in.

    The whole text is masked first, so an added line inside an existing fenced
    block is masked as code (blanked) and skipped rather than read as prose.
    Block type for each kept line is read off the matching raw line. Used by the
    added-lines markdown gate, where the diff supplies only `+` lines and the
    fence delimiters often live on unchanged lines.
    """
    return _run(text, set(line_numbers))


def _run(text: str, only: set[int] | None) -> list[Violation]:
    # A CRLF file broke every anchored block test at once: `lead == "---"` never
    # matched `---\r`, so frontmatter stayed unblanked and rule 6 fired down the
    # whole block. Normalising here keeps the line COUNT identical, so the
    # caller's 1-based line numbers still line up.
    text = text.replace("\r\n", "\n")
    masked = _mask(text)
    violations: list[Violation] = []
    sentence_index = 0
    raw_lines = text.split("\n")
    masked_lines = masked.split("\n")
    # Rule 6 is the one rule that reads a pair of lines rather than a sentence,
    # so its state lives here. It advances on EVERY line, including a line
    # `only` excludes: an added line sitting under unchanged prose must still
    # see that prose, or the added-lines gate reads it as paragraph-initial.
    prev_continuable = False
    # A table written without leading pipes. Tracked HERE and not in `_mask`,
    # because the rows need an exemption from rule 6 and from nothing else.
    # Blanking them in the mask bought that exemption by dropping them out of
    # rules 1 to 5 as well, which let a long sentence carrying a pipe sit under
    # a table and pass everything. Scoped this way the worst case is one missed
    # wrap, never a missed semicolon or modal.
    in_table = False
    for index, (raw_line, masked_line) in enumerate(zip(raw_lines, masked_lines), 1):
        blank = not masked_line.strip()
        is_list = bool(_LIST_MARKER_RE.match(raw_line))
        if blank or "|" not in raw_line:
            in_table = False
        table_row = _starts_table_run(raw_line, raw_lines, index)
        if table_row:
            in_table = True
        own_line = (
            table_row
            or (in_table and "|" in raw_line and not blank)
            or bool(_HEADING_RE.match(raw_line))
            or bool(_OWN_LINE_BREAK_RE.match(raw_line))
            or bool(_HTML_BLOCK_RE.match(raw_line))
            or bool(_REF_DEF_RE.match(raw_line))
            or bool(_FIELD_LINE_RE.match(raw_line))
        )
        starts_block = is_list or own_line or bool(_BLOCKQUOTE_RE.match(raw_line))
        # Only the CONTINUING line is charged, never the line above it.
        #
        # A break belongs to a pair, so scoping it to either half reads as the
        # more complete rule. Measured on this tree, it is the unlandable one:
        # 6519 rule-6 breaks sit in 185 legacy files, so editing one line of one
        # paragraph charged the untouched line below it too, and a one-line typo
        # fix could not pass without reflowing prose the author never opened.
        # That is the flag-day rewrite the added-lines ratchet exists to avoid.
        #
        # The cost is a real blind spot, kept deliberately and documented in
        # docs/style-rules.md: a new line inserted directly ABOVE untouched
        # prose splits that paragraph and goes unreported until someone touches
        # the line below. This module already trades the same way twice, at the
        # rule 4 closed list and the rule 1 block-type cap. A missed break is
        # cheaper than refusing a line the author never wrote.
        in_scope = only is None or index in only
        if not blank and not starts_block and prev_continuable and in_scope:
            violations.append(
                Violation(
                    6, index - 1, masked_line.strip(),
                    f"line {index} continues the paragraph above. "
                    "A paragraph is one physical line. "
                    "Join the two lines, or put a blank line between them.",
                )
            )
        prev_continuable = not blank and not own_line
        if blank or only is not None and index not in only:
            continue
        work = _LIST_MARKER_RE.sub("", masked_line, count=1) if is_list else masked_line
        for sentence in _split_sentences(work):
            violations.extend(_check_sentence(sentence, sentence_index, is_list))
            sentence_index += 1
    return violations


_EXCERPT_CAP = 12


def format_violations(violations: list[Violation]) -> str:
    """Render violations as a self-teaching, rule-compliant refusal message.

    The message itself passes rules 1 to 6: every banned word it names is
    double-quoted, and the masking pass replaces quoted spans with one token
    before any rule runs, so a gate that violates its own rule never ships.
    Rule 6 is why the lines are joined by a BLANK line rather than a newline.
    Each line here is its own paragraph, so single newlines would make the
    refusal an example of the break it refuses.

    Violations are grouped by rule number so a sender fixing one rule reads its
    every occurrence together. Every violation carries a quoted, capped excerpt
    of the offending text (rule 6's ``sentence`` field already holds its line,
    so no special case is needed there) - a sender no longer counts sentences
    by hand to find the one named in the detail.
    """
    if not violations:
        return ""
    lines = ["message blocked by the style rules."]
    by_rule: dict[int, list[Violation]] = {}
    for violation in violations:
        by_rule.setdefault(violation.rule, []).append(violation)
    for rule in sorted(by_rule):
        name = RULE_NAMES.get(rule, "?")
        for violation in by_rule[rule]:
            excerpt = _excerpt(violation.sentence)
            lines.append(f'rule {rule} ({name}): {violation.detail} "{excerpt}"')
    if 7 in by_rule:
        lines.extend(
            [
                "Cut articles, filler, pleasantries, hedges. "
                "Fragments work. Keep technical terms exact.",
                "Status: X. Why Y. Done at Z.",
                "Approval: Problem X. Options Y or Z. I recommend Z because A. Your call?",
                "Put findings on the node and link it.",
            ]
        )
    lines.append("add a style-exception line with a reason, or pass --style-exception.")
    lines.append('run "fno doctor lint style --stdin" to check a rewrite before you send it.')
    return "\n\n".join(lines)


def _quote_safe(text: str) -> str:
    """Replace an embedded double quote so it cannot close a wrapping quote early.

    Applies to any offending token or excerpt this module embeds inside its own
    double-quoted spans - an unmatched inner ``"`` shifts every quote pairing
    after it, which can leave a later word unmasked and fail the refusal's own
    self-check.
    """
    return text.replace('"', "'")


def _excerpt(sentence: str) -> str:
    """Cap the offending text at 12 words, quoted-safe."""
    words = _quote_safe(sentence).split()
    if len(words) > _EXCERPT_CAP:
        words = words[:_EXCERPT_CAP] + ["..."]
    return " ".join(words)


def _split_sentences(line: str) -> list[str]:
    """Split a masked line on sentence enders, protecting abbreviations."""
    protected = line
    for abbreviation in _ABBREVIATIONS:
        protected = protected.replace(abbreviation, abbreviation.replace(".", "\x00"))
    parts = re.split(r"(?<=[.!?])\s+", protected.strip())
    return [part.replace("\x00", ".") for part in parts if part.strip()]


def _check_sentence(sentence: str, index: int, is_list: bool) -> list[Violation]:
    out: list[Violation] = []
    shown = index + 1
    cap = LIST_ITEM_CAP if is_list else PARAGRAPH_CAP
    words = sentence.split()
    word_count = len(words)
    if word_count > cap:
        out.append(
            Violation(
                1, index, sentence,
                f"sentence {shown} is {word_count} words. The cap is {cap}.",
            )
        )
    if ";" in sentence:
        out.append(
            Violation(
                2, index, sentence,
                f"sentence {shown} has a semicolon. Split it into two sentences.",
            )
        )
    for word in words:
        stripped = word.strip(".,;:!?\"'()")
        lowered = stripped.lower()
        if lowered in BANNED_MODALS or stripped == BANNED_MODAL_MAY:
            out.append(
                Violation(
                    3, index, sentence,
                    f'sentence {shown} uses "{_quote_safe(word)}". '
                    'Write "can", "will", or "must" instead.',
                )
            )
    for word in words:
        token = word.replace("’", "'").lower().strip(".,;:!?\"()")
        if token in CONTRACTIONS:
            out.append(
                Violation(
                    4, index, sentence,
                    f'sentence {shown} has the contraction "{_quote_safe(word)}". '
                    "Write the words out.",
                )
            )
    keyword = _condition_keyword(sentence)
    if keyword is not None:
        out.append(
            Violation(
                5, index, sentence,
                f'sentence {shown} puts "{keyword}" after the command. '
                "The condition must start the sentence.",
            )
        )
    return out


def _check_message_length(text: str) -> list[Violation]:
    masked = _mask(text)
    words = masked.split()
    count = len(words)
    if count <= MESSAGE_WORD_CAP:
        return []
    first_line = masked.splitlines()[0].strip() if masked.splitlines() else ""
    return [
        Violation(
            7,
            0,
            first_line,
            f"this message runs {count} words. The cap is {MESSAGE_WORD_CAP} words.",
        )
    ]


def _condition_keyword(sentence: str) -> str | None:
    """Return the first non-initial if/when, or None if it leads the sentence."""
    body = _LIST_MARKER_RE.sub("", sentence).lstrip()
    matches = list(_CONDITION_RE.finditer(body))
    if not matches:
        return None
    if matches[0].start() == 0 and len(matches) == 1:
        return None
    for match in matches:
        if match.start() != 0:
            return match.group(1).lower()
    return None


def _mask(text: str) -> str:
    """Replace code constructs with placeholders and blank non-prose lines.

    Removed entirely (zero words): frontmatter, fenced and indented code blocks,
    HTML comments, table rows, log lines. Replaced by one placeholder token:
    inline code spans, link targets, double-quoted spans, flags, paths, URLs,
    dotted filenames, and underscore identifiers.

    Every input line maps to one output line (blanked lines become ""), so the
    caller can zip the raw and masked texts line-for-line to read block type off
    the raw line. Frontmatter is blanked rather than stripped for the same reason.
    """
    out: list[str] = []
    in_fence = False
    fence_char = ""
    in_comment = False
    in_frontmatter = False
    lines = text.split("\n")
    for index, raw_line in enumerate(lines):
        lead = raw_line.lstrip()


        # Frontmatter: a leading `--- ... ---` block. Blank it line-for-line so
        # the masked text keeps the same line count as the raw text.
        if index == 0 and lead == "---":
            in_frontmatter = True
            out.append("")
            continue
        if in_frontmatter:
            if lead == "---":
                in_frontmatter = False
            out.append("")
            continue

        if in_comment:
            if "-->" in raw_line:
                in_comment = False
            out.append("")
            continue

        fence = _FENCE_OPEN_RE.match(lead)
        if fence is not None:
            char = fence.group(1)[0]
            if not in_fence:
                in_fence, fence_char = True, char
            elif fence_char == char:
                in_fence, fence_char = False, ""
            out.append("")
            continue
        if in_fence:
            out.append("")
            continue
        if raw_line.startswith("    ") or raw_line.startswith("\t"):
            out.append("")
            continue
        if lead.startswith("<!--"):
            if "-->" not in raw_line:
                in_comment = True
                out.append("")
                continue
            # A same-line comment falls through: _mask_inline strips just the
            # span, so prose trailing the comment is still checked.
        # A leading-pipe table row is unambiguous, so it is removed outright as
        # it always has been. A PIPELESS row is deliberately NOT handled here.
        # Blanking removes a line from rules 1 to 6, and the only thing a
        # pipeless row ever needed was an exemption from rule 6. Two earlier
        # attempts blanked it and each opened a rule-evasion hole, because a
        # pipeless row is shaped exactly like a sentence carrying a pipe. That
        # exemption now lives in `_run`, scoped to rule 6, so such a line is
        # still checked for length, semicolons, modals, and contractions.
        if lead.startswith("|") or _DELIMITER_ROW_RE.match(lead):
            out.append("")
            continue
        if _LOG_RE.match(lead):
            out.append("")
            continue
        out.append(_mask_inline(raw_line))
    return "\n".join(out)


def _starts_table_run(raw_line: str, lines: "list[str]", index: int) -> bool:
    """True when this 1-based line opens a table run, in either spelling.

    Two openers count: a delimiter row, and the HEADER directly above one. The
    header needs a lookahead, because it is indistinguishable from prose until
    the delimiter row beneath it is read, and without it the header was charged
    while every body row was exempt.

    A paragraph above a setext underline is untouched, since a delimiter row
    needs pipes and a bare dash run never matches one.
    """
    lead = raw_line.lstrip()
    if _DELIMITER_ROW_RE.match(lead) or (
        lead.startswith("|") and _DELIMITER_ROW_RE.match(lead.strip("| \t"))
    ):
        return True
    if "|" not in raw_line:
        return False
    nxt = lines[index].lstrip() if index < len(lines) else ""
    return bool(_DELIMITER_ROW_RE.match(nxt)) or (
        nxt.startswith("|") and bool(_DELIMITER_ROW_RE.match(nxt.strip("| \t")))
    )


def _mask_inline(line: str) -> str:
    line = _COMMENT_SPAN_RE.sub("", line)
    line = _CODE_DOUBLE_RE.sub(_PLACEHOLDER, line)
    line = _CODE_SINGLE_RE.sub(_PLACEHOLDER, line)
    line = _LINK_RE.sub(r"\1", line)
    line = _QUOTE_RE.sub(_PLACEHOLDER, line)
    line = _FLAG_RE.sub(_PLACEHOLDER, line)
    line = _URL_RE.sub(_PLACEHOLDER, line)
    line = _PATH_RE.sub(_PLACEHOLDER, line)
    line = _FILENAME_RE.sub(_PLACEHOLDER, line)
    line = _IDENT_RE.sub(_PLACEHOLDER, line)
    return line
