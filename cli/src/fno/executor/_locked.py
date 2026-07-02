"""Extract /think's executor lock from a design doc.

Ported byte-for-byte from the retired ``scripts/lib/parse-locked-executor.sh``
(internalized for self-contained packaging, ab-58645f63). This module is now
the one definition of the locked-decision parser.

Reads design-doc text. Emits one of:
    ''            no lock recorded (or unknown value rejected)
    'do'          plan-level executor: do
    'impeccable'  plan-level executor: impeccable
    'mixed'       plan-level executor: do with per-task impeccable overrides

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
    cat design.md | python3 -m fno.executor._locked   # -> '' | do | impeccable | mixed
"""
from __future__ import annotations

import re
import sys

_CANONICAL = ("do", "impeccable", "mixed")

# Mirrors awk's `tolower($0) ~ /^##[[:space:]]+locked[[:space:]]+decisions/`.
_LOCKED_HEADING_RE = re.compile(r"^##[ \t]+locked[ \t]+decisions")
# Mirrors awk's `/^##[[:space:]]/` (a `## ` heading).
_H2_RE = re.compile(r"^##[ \t]")

# grep -oEi 'executor[[:space:]]*:[[:space:]]*[a-zA-Z_-]+'
_EXECUTOR_KV_RE = re.compile(r"executor[ \t]*:[ \t]*[a-zA-Z_-]+", re.IGNORECASE)
# sed -E 's/^[Ee][Xx]...[Rr][[:space:]]*:[[:space:]]*//' (strip the leading key).
_EXECUTOR_PREFIX_RE = re.compile(r"^executor[ \t]*:[ \t]*", re.IGNORECASE)

# is_routing_header: strip leading list markers ("- ", "1. ", "* ") then a
# leading "**", then require ^\*?\*?executor[[:space:]]+routing\*?\*?[[:space:]]*: .
_LIST_MARKER_RE = re.compile(r"^[ \t]*([0-9]+\.|[-*])[ \t]*")
_LEADING_BOLD_RE = re.compile(r"^\*\*")
_ROUTING_HEADER_RE = re.compile(
    r"^\*?\*?executor[ \t]+routing\*?\*?[ \t]*:", re.IGNORECASE
)
# New-list-item detection inside the buffering loop:
# grep -qE '^[[:space:]]*([0-9]+\.|[-*])[[:space:]]+\*?\*?'
_NEW_LIST_ITEM_RE = re.compile(r"^[ \t]*([0-9]+\.|[-*])[ \t]+\*?\*?")


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
    """Find the LAST ``executor:<value>`` in ``block``; return the lowercase
    value if canonical (do|impeccable|mixed), else ''.

    Mirrors the bash ``extract_value``: drop backticks, grep the last
    ``executor: <value>`` (case-insensitive), strip the key prefix, lowercase,
    filter to the canonical three.
    """
    block = block.replace("`", "")
    matches = _EXECUTOR_KV_RE.findall(block)
    if not matches:
        return ""
    val = _EXECUTOR_PREFIX_RE.sub("", matches[-1]).lower()
    return val if val in _CANONICAL else ""


def _is_routing_header(line: str) -> bool:
    """Return True if ``line`` opens an "Executor routing" entry.

    Strip leading list markers and a leading ``**``, then require the
    ``executor routing:`` head (bold variants and plain prefix accepted).
    """
    line = _LIST_MARKER_RE.sub("", line)
    line = _LEADING_BOLD_RE.sub("", line)
    return _ROUTING_HEADER_RE.search(line) is not None


def parse_locked_executor(text: str) -> str:
    """Parse the locked executor decision from design-doc ``text``.

    Returns '' | 'do' | 'impeccable' | 'mixed'.
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
        if _is_routing_header(line):
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
                if _is_routing_header(line):
                    buffer = line
                continue
            buffer = buffer + "\n" + line

    # Flush trailing buffer.
    _flush()

    return last_value


# grep -oEi 'model[[:space:]]*:[[:space:]]*<token>' scoped to Locked Decisions.
# The value is a single non-whitespace token (same shape `fno backlog update
# --model` validates); backticks are stripped first so `Model: `fable`` works.
_MODEL_KV_RE = re.compile(r"model[ \t]*:[ \t]*(\S+)", re.IGNORECASE)


def parse_locked_model(text: str) -> str:
    """Parse a locked ``Model:`` decision from design-doc ``text`` (x-571f).

    Scans the ``## Locked Decisions`` section (a bare ``model:`` mention in prose
    elsewhere is not a lock) for the LAST ``Model: <token>`` entry and returns
    the single-token value, or '' when none is present / the value is not a
    single token of <=64 chars. No allowlist: aliases (fable|opus|sonnet) and
    full provider-model ids pass through verbatim, matching the update verb.
    """
    if not text:
        return ""
    section = _extract_section(text)
    if not section:
        return ""
    # Strip backticks and bold markers so ``**Model**: `fable``` normalizes to
    # ``Model: fable`` before the KV scan (tolerant of the executor-lock's own
    # bold conventions). A model token never contains ``*`` or a backtick.
    matches = _MODEL_KV_RE.findall(section.replace("`", "").replace("*", ""))
    if not matches:
        return ""
    val = matches[-1]
    return val if len(val) <= 64 else ""


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
