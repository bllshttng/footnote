"""fno.plan.criteria - canonical acceptance-criteria compiler.

One deterministic parser for the ``## Acceptance Criteria`` section body.
Multiple author source shapes enter; one ordered, strict semantic
representation leaves: criterion records carrying a stable code, the original
statement text, a behavior kind, and a source position for diagnostics.

Source shapes accepted (the six families locked in design x-f905):

  - legacy bold labels:   ``**AC1-HP:** Given ... when ... then ...``
  - headings:             ``### AC1-HP: Title``  or  ``### Title``
  - numbered items:       ``1. Given ... when ... then ...``
  - bullets:              ``-`` / ``*`` / ``+`` behavior
  - table rows:           ``| label | behavior |``  (cells join in column order)
  - label + Given/When/Then block

Outcomes:

  - truly empty section      -> ``[]``
  - non-empty, zero matches  -> :class:`CriteriaParseError` (malformed; AC6-EDGE)
  - duplicate explicit code   -> :class:`CriteriaParseError` naming both
                                 source locations (AC4-ERR)
  - otherwise                 -> ordered ``list[Criterion]``

Pure and structural: it recognizes non-empty statements and stable boundaries.
It does NOT judge whether arbitrary prose is observably useful - that quality
judgment belongs to the full-context Think and Blueprint layers. Repeated
compilation of an unchanged body yields identical codes and ordering (AC2-HP).
"""
from __future__ import annotations

import re
from dataclasses import dataclass


class CriteriaParseError(ValueError):
    """A non-empty Acceptance Criteria body could not be compiled.

    Two triggers, both hard errors at finalization (AC4-ERR / AC6-EDGE):

      - the body has content but no recognized criterion shape, or
      - two criteria carry the same explicit identifier.

    The message names source locations so an author can fix the format without
    translating the whole document into one Markdown house style.
    """


@dataclass(frozen=True)
class Criterion:
    """One compiled acceptance criterion.

    code:       stable task-addressable identifier. An author-supplied token
                (``AC1-HP`` or ``AC7``) is preserved verbatim; an unlabeled
                criterion gets a synthesized ``AC{n}`` in document order.
    text:       the original observable statement, preserved verbatim.
    kind:       behavior classification retained when explicit (the ``-TYPE``
                suffix), else ``GENERAL``.
    source_ref: human-readable source position for diagnostics (``line N``).
    """

    code: str
    text: str
    kind: str
    source_ref: str


# ---------------------------------------------------------------------------
# AC identifier tokens
# ---------------------------------------------------------------------------

# An optional lowercase letter suffix on the number allows a sub-criterion
# spelling like AC3b / AC3b-HP (x-cdc5): without it, a letter-suffixed bold
# label matched NEITHER regex and compiled to nothing - silently dropped from
# the contract, no error. The suffix is lowercase to stay distinct from the
# uppercase -TYPE kind suffix. AC3 and AC3-HP still match ([a-z]* is empty).
_AC_TYPED_RE = r"AC\d+[a-z]*-[A-Z]+"  # AC1-HP, AC12-EDGE, AC3b-HP
_AC_BARE_RE = r"AC\d+[a-z]*"  # AC1, AC7, AC3b
_AC_ANY_RE = rf"(?:{_AC_TYPED_RE}|{_AC_BARE_RE})"

# A leading AC token on a label / heading / list item / table cell. The
# trailing delimiter class lets "AC1-HP:", "AC1.", "AC1 -", and "AC1" all parse.
_LEADING_CODE_RE = re.compile(rf"^({_AC_ANY_RE})\b[\s:.\-]*")
_CODE_NUMBER_RE = re.compile(r"AC(\d+)")

# A bold AC marker opening a line: ``**AC1-HP:** text``, ``**AC1:** text``, or
# the documented tagged legacy form ``**AC1-HP (api):** text`` (the ``(api)``
# suffix is metadata, not part of the code or kind).
_BOLD_AC_LINE_RE = re.compile(
    rf"^\s*\*\*({_AC_ANY_RE})(?:\s*\([^)\n]*\))?\s*:?\s*\*\*\s*(.*)$"
)

# A Given/When/Then scenario line. Case-insensitive on the keyword so a hand-
# written spec's lowercase "when" still attaches; the word boundary keeps
# "Thenarbitrary" from matching.
_GWT_PREFIX_RE = re.compile(r"^\s*(?:Given|When|Then|And)\b\s+", re.IGNORECASE)

_LIST_MARKER_RE = re.compile(r"^\s*(?:[-*+]|\d+\.)\s+")
_TABLE_SEP_CELL_RE = re.compile(r"^:?-+:?$")


def _is_heading(line: str) -> bool:
    return line.lstrip().startswith("#")


def _is_bold_ac(line: str) -> bool:
    return _BOLD_AC_LINE_RE.match(line) is not None


def _is_table(line: str) -> bool:
    return line.lstrip().startswith("|")


def _is_list(line: str) -> bool:
    return _LIST_MARKER_RE.match(line) is not None


def _is_gwt(line: str) -> bool:
    return _GWT_PREFIX_RE.match(line) is not None


_FENCE_OPEN_RE = re.compile(r"^[ \t]*(`{3,}|~{3,})")


def _fence_delim(line: str) -> str | None:
    """The opening fence delimiter string (``` or ~~~) if the line is one."""
    m = _FENCE_OPEN_RE.match(line)
    return m.group(1) if m else None


def _is_fence(line: str) -> bool:
    """A fenced-code-block delimiter line (``` or ~~~)."""
    return _fence_delim(line) is not None


def _is_anchor(line: str) -> bool:
    """A line that opens a new criterion (so it ends the previous one's block)."""
    return _is_heading(line) or _is_bold_ac(line) or _is_table(line) or _is_list(line)


def _split_leading_code(text: str) -> tuple[str | None, str, str]:
    """If ``text`` opens with an explicit AC token, return (code, kind, rest).

    kind is the ``-TYPE`` suffix when present, else ``GENERAL``. When no token
    is present, returns (None, "", text) so the caller can synthesize a code.
    """
    m = _LEADING_CODE_RE.match(text)
    if not m:
        return None, "", text
    code = m.group(1)
    kind = code.split("-", 1)[1] if "-" in code else "GENERAL"
    return code, kind, text[m.end():].strip()


def _code_number(code: str) -> int | None:
    m = _CODE_NUMBER_RE.match(code)
    return int(m.group(1)) if m else None


def _gather_until_break(lines: list[str], start: int) -> tuple[list[str], int]:
    """Collect continuation lines (non-blank, non-anchor) from ``start``.

    Used by the marker families (bold / heading / list): the criterion's text
    is the marker content plus any following non-blank line that is not itself a
    new anchor (a contiguous Given/When/Then block, or wrapped prose).
    Returns (stripped_parts, index_of_the_break).
    """
    out: list[str] = []
    j = start
    while j < len(lines):
        s = lines[j]
        if not s.strip() or _is_anchor(s) or _is_fence(s):
            break
        out.append(s.strip())
        j += 1
    return out, j


def _gather_gwt(lines: list[str], start: int) -> tuple[list[str], int]:
    """Collect a contiguous Given/When/Then block from ``start``."""
    out: list[str] = []
    j = start
    while j < len(lines):
        s = lines[j]
        if not s.strip() or not _is_gwt(s) or _is_fence(s):
            break
        out.append(s.strip())
        j += 1
    return out, j


def _split_table_row(line: str) -> list[str]:
    s = line.strip()
    if s.startswith("|"):
        s = s[1:]
    if s.endswith("|"):
        s = s[:-1]
    return [c.strip() for c in s.split("|")]


def _is_separator_row(cells: list[str]) -> bool:
    non_empty = [c for c in cells if c.strip()]
    return bool(non_empty) and all(_TABLE_SEP_CELL_RE.match(c.strip()) for c in non_empty)


def _table_data_rows(block: list[str]) -> list[tuple[str | None, str, str, int]]:
    """Compile a contiguous table block into criterion rows.

    Each data row becomes one criterion; its cells join in column order (so
    Given/When/Then columns concatenate into one scenario). The header row (the
    one a separator follows) and the separator row itself are excluded. Returns
    (code, kind, text, line_offset) per data row.
    """
    rows = [_split_table_row(line) for line in block]
    sep_idx: int | None = None
    for idx, cells in enumerate(rows):
        if _is_separator_row(cells):
            sep_idx = idx
            break
    out: list[tuple[str | None, str, str, int]] = []
    for idx, cells in enumerate(rows):
        if not cells or _is_separator_row(cells):
            continue
        if sep_idx is not None and idx == sep_idx - 1:
            continue  # header row
        code, kind, first_rest = _split_leading_code(cells[0])
        text_cells = ([first_rest] if first_rest else []) + [
            c for c in cells[1:] if c.strip()
        ]
        out.append((code, kind or "GENERAL", " ".join(text_cells).strip(), idx))
    return out


def _next_synth(counter: int, used: set[int]) -> int:
    """Next document-order synthesized number, skipping explicit-claimed ones."""
    counter += 1
    while counter in used:
        counter += 1
    return counter


def _finalize(
    raw: list[tuple[int, str | None, str, str]],
    *,
    source: str,
) -> list[Criterion]:
    """Assign synthesized codes, detect duplicate explicit identifiers.

    A marker that matched but yielded no statement (a bare ``**AC1-HP:**`` or
    ``### AC1-HP`` with nothing after it) is not a valid criterion: the design
    contract is that a candidate is valid when it yields a non-empty statement.
    Dropping it here means a task that references that code fails resolution
    (caught at compiled-v1 validation) rather than stamping ready against a
    criterion that carries no behavior at all.
    """
    raw = [record for record in raw if record[3].strip()]
    explicit_numbers: set[int] = set()
    for _line, code, _kind, _text in raw:
        if code is not None:
            num = _code_number(code)
            if num is not None:
                explicit_numbers.add(num)

    synth = 0
    seen: dict[str, int] = {}
    out: list[Criterion] = []
    for line_no, code, kind, text in raw:
        if code is None:
            synth = _next_synth(synth, explicit_numbers)
            code = f"AC{synth}"
            kind = "GENERAL"
        elif code in seen:
            raise CriteriaParseError(
                f"duplicate acceptance identifier {code!r} in {source}: "
                f"first at line {seen[code]}, again at line {line_no}. "
                f"Rename one so task ownership stays unambiguous."
            )
        seen[code] = line_no
        out.append(Criterion(code=code, text=text, kind=kind, source_ref=f"line {line_no}"))
    return out


def compile_criteria(
    section_body: str,
    *,
    source: str = "Acceptance Criteria",
) -> list[Criterion]:
    """Compile an Acceptance Criteria section body into ordered criteria.

    Returns ``[]`` for a truly empty section. Raises :class:`CriteriaParseError`
    for a non-empty body that yields no recognized criterion (AC6-EDGE) or for a
    duplicate explicit identifier (AC4-ERR).
    """
    body = section_body or ""
    if not body.strip():
        return []

    lines = body.splitlines()
    n = len(lines)
    raw: list[tuple[int, str | None, str, str]] = []
    i = 0
    in_fence = False
    fence_close = ""
    while i < n:
        line = lines[i]
        # Skip fenced code blocks so a list-like example line ("- --verbose")
        # inside a criterion's code block is not compiled as a criterion. Track
        # the opening delimiter: a closing fence must match its char and be at
        # least as long, so a triple-backtick line inside a four-backtick block
        # stays content (CommonMark).
        delim = _fence_delim(line)
        if delim is not None:
            if not in_fence:
                in_fence = True
                fence_close = delim
            elif delim[0] == fence_close[0] and len(delim) >= len(fence_close):
                in_fence = False
                fence_close = ""
            i += 1
            continue
        if in_fence or not line.strip():
            i += 1
            continue

        # 1. Legacy bold AC marker.
        m = _BOLD_AC_LINE_RE.match(line)
        if m:
            code = m.group(1)
            kind = code.split("-", 1)[1] if "-" in code else "GENERAL"
            parts = [p for p in [m.group(2).strip()] if p]
            cont, j = _gather_until_break(lines, i + 1)
            parts.extend(cont)
            raw.append((i + 1, code, kind, " ".join(parts).strip()))
            i = j
            continue

        # 2. Heading (with or without an explicit AC id in the title).
        if _is_heading(line):
            title = re.sub(r"^#+\s*", "", line.strip())
            code, kind, rest = _split_leading_code(title)
            parts = [p for p in [rest] if p]
            cont, j = _gather_until_break(lines, i + 1)
            parts.extend(cont)
            raw.append((i + 1, code, kind or "GENERAL", " ".join(parts).strip()))
            i = j
            continue

        # 3. Markdown table block.
        if _is_table(line):
            j = i
            while j < n and lines[j].lstrip().startswith("|"):
                j += 1
            for rcode, rkind, rtext, offset in _table_data_rows(lines[i:j]):
                if rtext:
                    raw.append((i + 1 + offset, rcode, rkind, rtext))
            i = j
            continue

        # 4. List item (numbered or bulleted).
        if _is_list(line):
            item = _LIST_MARKER_RE.sub("", line, count=1)
            code, kind, rest = _split_leading_code(item)
            parts = [p for p in [rest] if p]
            cont, j = _gather_until_break(lines, i + 1)
            parts.extend(cont)
            raw.append((i + 1, code, kind or "GENERAL", " ".join(parts).strip()))
            i = j
            continue

        # 5. Bare Given/When/Then block (no marker or label in this paragraph).
        if _is_gwt(line):
            gwt, j = _gather_gwt(lines, i)
            if gwt:
                raw.append((i + 1, None, "GENERAL", " ".join(gwt).strip()))
            i = j
            continue

        # 6. Descriptive label + contiguous Given/When/Then block. A plain line
        #    is a criterion only when a Given/When/Then block follows it; prose
        #    alone is not (so a non-empty prose-only section refuses, AC6-EDGE).
        k = i + 1
        while k < n and not lines[k].strip():
            k += 1
        if k < n and _is_gwt(lines[k]):
            code, kind, rest = _split_leading_code(line.strip())
            gwt, j = _gather_gwt(lines, k)
            parts = ([rest] if rest else []) + gwt
            raw.append((i + 1, code, kind or "GENERAL", " ".join(parts).strip()))
            i = j
            continue

        i += 1

    criteria = _finalize(raw, source=source)
    if not criteria:
        raise CriteriaParseError(
            f"{source} has content but no recognized criterion shape. "
            "Rewrite each criterion in one of the accepted forms:\n"
            "  - **AC1-HP:** Given ... when ... then ...   (legacy bold id)\n"
            "  - ### AC1-HP: Title  /  ### Title           (heading)\n"
            "  - 1. Given ... when ... then ...            (numbered item)\n"
            "  - - Given ... when ... then ...             (bullet)\n"
            "  - | label | Given | When | Then |           (table row)\n"
            "  - A descriptive label followed by contiguous Given/When/Then lines\n"
            "Or leave the section truly empty if the work is not yet decomposed."
        )
    return criteria


__all__ = ["Criterion", "CriteriaParseError", "compile_criteria"]
