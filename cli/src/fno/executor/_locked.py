"""Extract /think's executor lock from a design doc.

Ported byte-for-byte from the retired ``scripts/lib/parse-locked-executor.sh``
(internalized for self-contained packaging, ab-58645f63). This module is now
the one definition of the locked-decision parser.

Reads design-doc text. Emits one of:
    ''            no lock recorded (or unknown value rejected)
    'tdd'         plan-level executor: tdd
    'do'          one-release alias for tdd
    'impeccable'  plan-level executor: impeccable
    'mixed'       plan-level executor: tdd with per-task impeccable overrides

Multiple entries: take the LAST match (per plan failure modes - if a user
edited /think's output and added a second entry, the most-recent intent
wins). Unknown values: skip and emit empty (failure-mode requirement: do
not silently fall through to a default).

The parser scopes its search to the ``## Locked Decisions`` section. A bare
``executor:`` mention elsewhere in the doc is not a lock - it's prose. This
guards against the domain pitfall where the operator's resolver is discussed
in the architecture section without intending to lock anything.

Tolerant of formatting variations:
    **Executor routing**: ...
    **Executor Routing:** ...
    Executor routing: ...
    Mixed casing of the keyword and value
    Extra whitespace around colons and backticks
    Provenance suffix (auto-detected) / (user-confirmed) / (cli-flag) - optional

CLI:
    cat design.md | python3 -m fno.executor._locked   # -> '' | tdd | impeccable | mixed
"""
from __future__ import annotations

import re
import sys

_CANONICAL = ("tdd", "impeccable", "mixed")


def _normalize_executor(value: str) -> str:
    return "tdd" if value == "do" else value

# Mirrors awk's `tolower($0) ~ /^##[[:space:]]+locked[[:space:]]+decisions/`.
_LOCKED_HEADING_RE = re.compile(r"^##[ \t]+locked[ \t]+decisions")
# Mirrors awk's `/^##[[:space:]]/` (a `## ` heading).
_H2_RE = re.compile(r"^##[ \t]")

# grep -oEi 'executor[[:space:]]*:[[:space:]]*[a-zA-Z_-]+', plus a ``\**`` after
# the colon so a bold-wrapped key (``**Executor:** do``) reaches its value -
# the same tolerance ``_MODEL_KV_RE`` already grants ``**Model:** fable``.
_EXECUTOR_KV_RE = re.compile(
    r"executor[ \t]*:[ \t]*\**[ \t]*[a-zA-Z_-]+", re.IGNORECASE
)
# sed -E 's/^[Ee][Xx]...[Rr][[:space:]]*:[[:space:]]*//' (strip the leading key).
_EXECUTOR_PREFIX_RE = re.compile(r"^executor[ \t]*:[ \t]*\**[ \t]*", re.IGNORECASE)

# is_routing_header: strip leading list markers ("- ", "1. ", "* ") then a
# leading "**", then require ^\*?\*?executor[[:space:]]+routing\*?\*?[[:space:]]*: .
# The wording the documented mixed shape always carries ("per-task overrides").
# This is what separates it from rationale prose that merely names both values.
_OVERRIDE_SHAPE_RE = re.compile(r"overrid(?:e|es|ing)\b|per-task\b", re.IGNORECASE)

_LIST_MARKER_RE = re.compile(r"^[ \t]*([0-9]+\.|[-*])[ \t]*")
_LEADING_BOLD_RE = re.compile(r"^\*\*")
_ROUTING_HEADER_RE = re.compile(
    r"^\*?\*?executor[ \t]+routing\*?\*?[ \t]*:", re.IGNORECASE
)
# New-list-item detection inside the buffering loop:
# grep -qE '^[[:space:]]*([0-9]+\.|[-*])[[:space:]]+\*?\*?'
_NEW_LIST_ITEM_RE = re.compile(r"^[ \t]*([0-9]+\.|[-*])[ \t]+\*?\*?")

# A Locked Decision that names the executor directly rather than through the
# two-word "Executor routing" head, e.g. ``5. **Executor: `do` (archer/TDD).**``.
# Anchored at line start (after an optional list marker and bold run) so a prose
# ``the executor: impeccable resolver`` mention inside the section is still not a
# lock; ``parse_locked_model`` already accepts the same shape for ``Model:``.
_BARE_EXECUTOR_ENTRY_RE = re.compile(
    r"^[ \t]*(?:[0-9]+\.|[-*])?[ \t]*\**executor\**[ \t]*:", re.IGNORECASE
)


def _extract_section(text: str) -> str:
    """Return the body of the ``## Locked Decisions`` section, or ''.

    Reproduces the awk pass: scan lines; on the first ``## `` heading whose
    lowercased form matches the locked-decisions heading, start including
    subsequent lines (the heading line itself is consumed via ``next``); stop
    at the next ``## `` heading or EOF. If no such heading exists, return ''.
    """
    inside = False
    collected: list[str] = []
    for line in text.split("\n"):
        if _H2_RE.match(line):
            if inside:
                break
            if _LOCKED_HEADING_RE.match(line.lower()):
                inside = True
                continue
        if inside:
            collected.append(line)
    if not collected:
        return ""
    # awk prints each `inside` line followed by ORS (\n); the bash captures it
    # in $(...), which strips trailing newlines. Join with \n; the trailing
    # newline is irrelevant because the caller splits on \n again.
    return "\n".join(collected)


def _extract_value(block: str) -> str:
    """Resolve one entry's ``executor:<value>`` to a canonical value, or ''.

    ``block`` is a SINGLE buffered entry. The documented mixed shape - "plan-level
    ``executor: tdd`` with per-task overrides ``executor: impeccable``" - resolves
    to ``mixed``, because taking the last value there would return ``impeccable``
    and route the whole plan through the frontend pipeline, the more expensive of
    the two mistakes.

    Two canonical values alone do NOT prove that shape. The whole buffered entry
    is scanned, rationale included, so a decision that names its rejected option
    ("``executor: do``, not the rejected ``executor: impeccable``") or its history
    ("changed from ``executor: impeccable`` to ``executor: do``") also carries two
    values while having plainly chosen one. Require the override wording that the
    real shape always states; otherwise fall through to last-wins, which preserves
    the stated choice. An explicit ``executor: mixed`` is canonical and needs no
    inference.

    Otherwise this mirrors the bash ``extract_value``: drop backticks, take the
    LAST ``executor: <value>`` (case-insensitive), strip the key prefix,
    lowercase, and filter to the canonical three. Last-wins still decides
    ACROSS entries, which the caller's per-entry buffering keeps separate.
    """
    block = block.replace("`", "")
    matches = _EXECUTOR_KV_RE.findall(block)
    if not matches:
        return ""
    values = [
        _normalize_executor(_EXECUTOR_PREFIX_RE.sub("", m).lower())
        for m in matches
    ]
    if (
        len({v for v in values if v in _CANONICAL}) > 1
        and _OVERRIDE_SHAPE_RE.search(block) is not None
    ):
        return "mixed"
    return values[-1] if values[-1] in _CANONICAL else ""


def _is_entry_head(line: str) -> bool:
    """Return True if ``line`` opens a locked executor entry.

    Two shapes qualify: the "Executor routing" head /think is prompted to emit,
    and a bare ``Executor:`` decision naming the value directly.
    """
    if _BARE_EXECUTOR_ENTRY_RE.match(line):
        return True
    stripped = _LIST_MARKER_RE.sub("", line)
    stripped = _LEADING_BOLD_RE.sub("", stripped)
    return _ROUTING_HEADER_RE.search(stripped) is not None


def parse_locked_executor(text: str) -> str:
    """Parse the locked executor decision from design-doc ``text``.

    Returns '' | 'tdd' | 'impeccable' | 'mixed'. ``do`` input is normalized
    to ``tdd`` for one compatibility release.
    """
    if not text:
        return ""

    section = _extract_section(text)
    if not section:
        return ""

    last_value = ""
    buffer = ""

    def _flush() -> None:
        nonlocal last_value
        if buffer:
            v = _extract_value(buffer)
            if v:
                last_value = v

    # `read <<< "$SECTION"` iterates the section line by line. A here-string
    # adds a trailing newline, so the loop never sees a phantom final line;
    # splitting on \n and dropping a single trailing empty reproduces that.
    lines = section.split("\n")
    if lines and lines[-1] == "":
        lines = lines[:-1]

    for line in lines:
        if _is_entry_head(line):
            # Flush any prior buffered entry first.
            _flush()
            buffer = line
            continue
        if buffer:
            # Blank line (whitespace-only counts) closes the entry. Bash:
            # `[[ -z "${line//[[:space:]]/}" ]]`.
            if line.strip() == "":
                _flush()
                buffer = ""
                continue
            # New list item begins; close out the previous entry.
            if _NEW_LIST_ITEM_RE.match(line):
                _flush()
                buffer = ""
                # Re-evaluate this line in case it's another routing header.
                if _is_entry_head(line):
                    buffer = line
                continue
            buffer = buffer + "\n" + line

    # Flush trailing buffer.
    _flush()

    return last_value


# Capture the WHOLE value after ``model:`` (to end of line), scoped to Locked
# Decisions. Capturing only the first token would silently truncate a malformed
# spaced value like ``opus 4.8`` to ``opus`` and transcribe a wrong-but-valid
# pin (codex review PR #150); the whole value is validated below so a multi-token
# value is REJECTED instead, matching ``fno backlog update --model``. The ``\**``
# around the key tolerate a bold ``**Model**:`` head WITHOUT eating a ``*`` in the
# value (so a metacharacter value stays intact and is rejected, not sanitized).
# Anchored to line start (after an optional list marker) with re.MULTILINE so a
# prose ``the model:`` mention inside the section is not a false lock. The ``\**``
# groups around the key AND after the colon consume bold markers of either style
# (``**Model**:`` and ``**Model:** fable``) WITHOUT eating a ``*`` in the value
# itself, so a metacharacter value stays intact to be rejected (gemini review
# PR #150).
_MODEL_KV_RE = re.compile(
    r"^\s*(?:\d+\.[ \t]+|[-*][ \t]+)?\**model\**[ \t]*:[ \t]*\**[ \t]*(.+)",
    re.IGNORECASE | re.MULTILINE,
)
# Same shell-safe single-token charset the update verb enforces.
_MODEL_TOKEN_RE = re.compile(r"[A-Za-z0-9._:/-]{1,64}")
# An optional trailing provenance suffix, e.g. ``fable (user-confirmed)`` ->
# ``fable`` (mirrors the executor lock's provenance tolerance).
_MODEL_PROVENANCE_RE = re.compile(r"[ \t]*\([^)]*\)[ \t]*$")


def parse_locked_model(text: str) -> str:
    """Parse a locked ``Model:`` decision from design-doc ``text`` (x-571f).

    Scans the ``## Locked Decisions`` section (a bare ``model:`` mention in prose
    elsewhere is not a lock) for the LAST ``Model: <value>`` entry and returns
    the value when it is a single shell-safe token of <=64 chars, else '' (a
    multi-token / whitespaced / metacharacter value is REJECTED, not truncated).
    No allowlist: aliases (fable|opus|sonnet) and full provider-model ids pass
    through verbatim, matching the update verb.
    """
    if not text:
        return ""
    # Strip CR before anything else: on a CRLF checkout every line ends with \r,
    # which `.` in the KV regex captures into the value (``fable\r``) and then the
    # provenance/token checks reject a valid pin (gemini review PR #150).
    section = _extract_section(text.replace("\r", ""))
    if not section:
        return ""
    # Strip backticks so ``Model: `fable``` normalizes before the KV scan (a model
    # token never contains a backtick); bold ``*`` around the key are consumed by
    # the regex, not stripped, so a value's own ``*`` survives to be rejected.
    matches = _MODEL_KV_RE.findall(section.replace("`", ""))
    if not matches:
        return ""
    # Drop a trailing provenance suffix, then require the remainder be exactly one
    # shell-safe token (rejects ``opus 4.8`` and any glob/shell metacharacter).
    val = _MODEL_PROVENANCE_RE.sub("", matches[-1]).strip()
    return val if _MODEL_TOKEN_RE.fullmatch(val) else ""


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    text = sys.stdin.read()
    # `--key model` selects the model-pin parser; default is the executor lock
    # (byte-for-byte backward compatible with `python3 -m fno.executor._locked`).
    value = parse_locked_model(text) if argv[:2] == ["--key", "model"] else parse_locked_executor(text)
    if value:
        print(value)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
