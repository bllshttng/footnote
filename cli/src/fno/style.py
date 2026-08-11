"""Style checker for agent-authored text.

Five rules, checked at the tool boundary. ``docs/style-rules.md`` is the
normative statement; this module is the mechanism. Pure: no filesystem, no
state, no network. Every caller (``fno mail send``, the PR-body CI gate, the
changed-markdown CI gate) routes through :func:`check`.

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
}

LIST_ITEM_CAP = 20
PARAGRAPH_CAP = 25

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

_FENCE_OPEN_RE = re.compile(r"^[ \t]*(`{3,}|~{3,})")
_LOG_RE = re.compile(
    r"^(\[[A-Za-z]+\]|ERROR\b|WARN(?:ING)?\b|INFO\b|DEBUG\b|TRACE\b"
    r"|\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2})"
)
_LIST_MARKER_RE = re.compile(r"^\s{0,3}([-*+]|\d+\.)\s+")
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

    ``surface`` is carried for forward compatibility (a future rule may scope by
    surface); the five rules apply identically everywhere today. The text is
    masked whole, then split into sentences per line, so a sentence and a line
    are the same unit (the repo's one-sentence-per-line convention).
    """
    del surface  # reserved; no rule scopes by surface yet
    masked = _mask(text)
    violations: list[Violation] = []
    sentence_index = 0
    raw_lines = text.split("\n")
    masked_lines = masked.split("\n")
    for raw_line, masked_line in zip(raw_lines, masked_lines):
        if not masked_line.strip():
            continue
        is_list = bool(_LIST_MARKER_RE.match(raw_line))
        work = _LIST_MARKER_RE.sub("", masked_line, count=1) if is_list else masked_line
        for sentence in _split_sentences(work):
            violations.extend(_check_sentence(sentence, sentence_index, is_list))
            sentence_index += 1
    return violations


def format_violations(violations: list[Violation]) -> str:
    """Render violations as a self-teaching, rule-compliant refusal message.

    The message itself passes the five rules: every banned word it names is
    double-quoted, and the masking pass replaces quoted spans with one token
    before any rule runs, so a gate that violates its own rule never ships.
    """
    if not violations:
        return ""
    lines = ["message blocked by the style rules."]
    for violation in violations:
        name = RULE_NAMES.get(violation.rule, "?")
        lines.append(f"rule {violation.rule} ({name}): {violation.detail}")
    lines.append("add a style-exception line with a reason, or pass --style-exception.")
    return "\n".join(lines)


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
                    f'sentence {shown} uses "{word}". Write "can", "will", or "must" instead.',
                )
            )
            break
    for word in words:
        token = word.replace("’", "'").lower().strip(".,;:!?\"()")
        if token in CONTRACTIONS:
            out.append(
                Violation(
                    4, index, sentence,
                    f'sentence {shown} has the contraction "{word}". Write the words out.',
                )
            )
            break
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
    """
    text = _strip_frontmatter(text)
    out: list[str] = []
    in_fence = False
    fence_char = ""
    in_comment = False
    for raw_line in text.split("\n"):
        lead = raw_line.lstrip()

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
        if lead.startswith("|"):
            out.append("")
            continue
        if _LOG_RE.match(lead):
            out.append("")
            continue
        out.append(_mask_inline(raw_line))
    return "\n".join(out)


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


def _strip_frontmatter(text: str) -> str:
    if not text.startswith("---\n") and not text.startswith("---\r\n"):
        return text
    lines = text.split("\n")
    for index in range(1, len(lines)):
        if lines[index].strip() == "---":
            return "\n".join(lines[index + 1:])
    return text
