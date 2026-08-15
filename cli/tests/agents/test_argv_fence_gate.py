"""Mechanical argv-fence gate: a positional seed push must be fenced.

A spawn seed whose first token is flag-shaped ("--model x ...") dies in the
launcher's flag parser unless it rides behind a ``--`` end-of-options fence.
Unit tests pin the argv of each known builder, but they only fence the sites
they name: a NEW seam ships unfenced and green. This gate is shape-based
instead. It scans every argv builder in cli/src and crates/fno-agents/src for
a push of a message expression and fails unless that push is one of:

- fenced: the argv element immediately before the message is ``"--"``;
- value-form: the element immediately before is a single flag push
  (``-p`` / ``-i`` / ``--prompt``), so the message is that flag's value;
- exempt: the site carries an ``argv-fence: exempt`` comment, for harnesses
  whose ``--`` support is unverified (see the agy pane arm) or internal
  ``fno`` CLI namespaces (the x-04ce seam).

A seed wears a name (``message`` / ``prompt`` / ``full_prompt`` / ``seed`` /
``effective``), not a fixed identifier; string literals are stripped before
the name test so a flag like ``--append-system-prompt`` is never mistaken
for a seed. Bare-name elements of multi-line list literals are scanned, but
call arguments are not: the innermost unclosed bracket must be ``[``.

A new unfenced seam fails this test with no edit to this file. The floor
assertions below are the positive control: a scanner whose patterns rot and
match nothing fails loudly instead of certifying an empty sweep.
"""

import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
PY_DIRS = [REPO_ROOT / "cli/src/fno"]
RS_DIR = REPO_ROOT / "crates/fno-agents/src"

STR = r'"([^"]*)"'
PY_LIST = re.compile(r"^\s*([\w.]+)\s*\+=\s*\[(.*)\]")
PY_ASSIGN = re.compile(r"^\s*([\w.]+)\s*=\s*\[(.*)\]")
PY_CONCAT = re.compile(r"^\s*([\w.]+)\s*=\s*[\w.]+\s*\+\s*\[(.*)\]")
PY_EXTEND = re.compile(r"^\s*([\w.]+)\.extend\(\[(.*)\]\)\s*$")
PY_APPEND = re.compile(r"^\s*([\w.]+)\.append\((.*)\)\s*$")
RS_PUSH = re.compile(r"^\s*([\w.]+)\.push\((.*)\);?\s*$")
RS_ELEMENT = re.compile(
    r"^\s*(?:(?:ctx\.)?(?:message|effective)\.(?:clone|to_string)\(\)|normalize_codex_command\([^)]*\))\s*,\s*$"
)
PY_COMMENT = re.compile(r"^\s*#")
RS_COMMENT = re.compile(r"^\s*//")
STRING_IN = re.compile(STR)
# A pushed seed wears one of these names. Literal strings are stripped before
# the test so a flag literal like "--append-system-prompt" (which contains the
# word "prompt" behind a hyphen boundary) is never mistaken for a seed push.
HAS_MESSAGE = re.compile(r"\b(?:message|full_prompt|prompt|seed|effective|msg)\b")

# The only flags whose following token is legitimately the seed: flags that
# take the seed as their VALUE. Any other "-"-prefixed literal (a boolean
# flag) leaves a following seed as a bare positional, which is the exact
# unfenced shape this gate exists to catch, so it falls through to VIOLATION.
VALUE_FORM_FLAGS = frozenset(
    {"-p", "-i", "-q", "--prompt", "--search", "--append-system-prompt"}
)

# The container must be argv-shaped by name. Without this, any list named
# e.g. ``dated`` or a list comprehension over ``message`` is flagged, and the
# gate cries wolf on non-argv code.
ARGV_VAR = re.compile(r"^(?:[A-Za-z_]*argv|cmd|command|args|base_cmd|full_cmd)s?$")


def _literals(line: str) -> list:
    return STRING_IN.findall(line)


def _prev_code_line(lines: list, idx: int, is_rust: bool) -> str:
    skip = RS_COMMENT if is_rust else PY_COMMENT
    j = idx - 1
    while j >= 0:
        stripped = lines[j].strip()
        if not stripped or skip.match(lines[j]) or stripped in ("}", "]", ");"):
            j -= 1
            continue
        return lines[j]
    return ""


def _list_opener_line(lines: list, idx: int) -> str:
    """The opener line of the list literal enclosing line idx, or "".

    Scans upward from idx-1, chars right-to-left, tracking depth. When depth
    goes negative at a "[", that line opens the innermost list literal
    enclosing line idx; a "(" (a call argument) or "{" ends the search empty.
    Heuristic: brackets inside strings/comments can mislead, which the floor
    assertions below would surface as census drift.
    """
    depth = 0
    for j in range(idx - 1, -1, -1):
        for ch in reversed(lines[j]):
            if ch in ")]}":
                depth += 1
            elif ch in "([{":
                if depth == 0:
                    return lines[j] if ch == "[" else ""
                depth -= 1
        # A def/return line with no open bracket context ends the search: the
        # bare name is a statement, not an element.
        stripped = lines[j].strip()
        if stripped.startswith(("return ", "def ", "if ", "for ", "while ", "raise ")):
            return ""
    return ""


def _argv_named(container: str) -> bool:
    """True when the list/append target is argv-shaped by name."""
    return bool(ARGV_VAR.match(container.split(".")[0]))


def _exempt(lines: list, idx: int, is_rust: bool) -> bool:
    """True when an ``argv-fence: exempt`` marker rides directly above the
    push, or above the multi-line list opener that contains it."""
    skip = RS_COMMENT if is_rust else PY_COMMENT
    opener = re.compile(r"^\s*[\w.]+\s*(?:\+=|=[^=]|\+)\s*\[\s*$")
    j = idx - 1
    while j >= 0 and (skip.match(lines[j]) or opener.match(lines[j])):
        if "argv-fence: exempt" in lines[j]:
            return True
        j -= 1
    return False


def _classify_inline(text: str) -> str:
    """Classify a seed token inside a bracketed argv expression.

    The token before the seed decides: ``"--"`` fences it, a VALUE_FORM_FLAGS
    literal takes it as the flag's value, anything else is a VIOLATION.
    """
    tokens = [t.strip() for t in text.split(",")]
    for t_i, tok in enumerate(tokens):
        if HAS_MESSAGE.search(re.sub(STR, "", tok)):
            prev_tok = tokens[t_i - 1] if t_i else ""
            lits = _literals(prev_tok)
            if lits == ["--"]:
                return "fenced"
            if lits and lits[-1] in VALUE_FORM_FLAGS:
                return "value-form"
            return "VIOLATION"
    return ""


def _classify_prev(prev: str) -> str:
    """Classify from the immediately preceding argv line.

    The message is legal only when the argv TAIL is awaiting a value: a lone
    ``"--"`` (fence) or a lone flag literal. A tail like
    ``["--output-format", output_format]`` already bound its flag to a value
    (the last element is not a literal), so a message after it is a bare
    positional: violation. ``["claude", "-p"]`` ends on the flag itself, so
    an appended message becomes ``-p``'s value: value-form.
    """
    body = re.sub(STR, '"X"', prev.strip().rstrip(";"))
    if "[" in body:
        body = body[body.rindex("[") + 1:]
        if "]" in body:
            body = body[: body.rindex("]")]
    chunks = [c.strip() for c in body.split(",") if c.strip()]
    tail = chunks[-1] if chunks else ""
    while True:
        m = re.match(r"^[\w.]+\.(?:push|append)\((.*)\)$", tail)
        if not m:
            break
        tail = m.group(1).strip()
    if re.fullmatch(r'"X"(?:\.(?:into|to_string)\(\))?', tail):
        lit = _literals(prev)[-1]
        if lit == "--":
            return "fenced"
        if lit in VALUE_FORM_FLAGS:
            return "value-form"
    return "VIOLATION"


def scan_file(path: Path, is_rust: bool) -> list:
    """Return (file, line_no, pushed_text, classification) for each message push."""
    lines = path.read_text().splitlines()
    found = []
    for idx, line in enumerate(lines, start=1):
        if PY_COMMENT.match(line) or RS_COMMENT.match(line):
            continue
        kind = None
        pushed = None
        if not is_rust:
            m = (
                PY_LIST.match(line)
                or PY_ASSIGN.match(line)
                or PY_CONCAT.match(line)
                or PY_EXTEND.match(line)
            )
            if m and HAS_MESSAGE.search(re.sub(STR, "", m.group(2))):
                if _argv_named(m.group(1)):
                    kind = _classify_inline(m.group(2))
                    pushed = line.strip()
            if kind is None:
                m = PY_APPEND.match(line)
                if (
                    m
                    and _argv_named(m.group(1))
                    and HAS_MESSAGE.search(re.sub(STR, "", m.group(2)))
                ):
                    pushed = line.strip()
                    kind = _classify_prev(_prev_code_line(lines, idx - 1, is_rust))
            if kind is None:
                # Element of a multi-line argv LIST literal, bare or mixed with
                # literals on one line (not a call argument: the innermost
                # unclosed opener must be "[" and its assignment argv-named).
                # A bare leading element takes its classification from the line
                # above; a mid-line seed classifies against the in-line token
                # before it.
                opener = _list_opener_line(lines, idx - 1)
                if opener and HAS_MESSAGE.search(re.sub(STR, "", line)):
                    om = re.match(r"^\s*([\w.]+)\s*(?:\+=|=[^=]|\+)\s*\[", opener)
                    if om and _argv_named(om.group(1)):
                        seed_index = None
                        for t_i, tok in enumerate(t.strip() for t in line.split(",")):
                            if HAS_MESSAGE.search(re.sub(STR, "", tok)):
                                seed_index = t_i
                                break
                        if seed_index == 0:
                            pushed = line.strip()
                            kind = _classify_prev(_prev_code_line(lines, idx - 1, is_rust))
                        elif seed_index is not None:
                            inline = _classify_inline(line)
                            if inline:
                                pushed = line.strip()
                                kind = inline
        else:
            is_push = RS_PUSH.match(line)
            arg = is_push.group(2) if is_push else ""
            if (
                is_push
                and _argv_named(is_push.group(1))
                and HAS_MESSAGE.search(re.sub(STR, "", arg))
            ) or (
                arg == "" and RS_ELEMENT.match(line) and HAS_MESSAGE.search(line)
            ):
                pushed = line.strip()
                kind = _classify_prev(_prev_code_line(lines, idx - 1, is_rust))
        if kind == "VIOLATION" and _exempt(lines, idx - 1, is_rust):
            kind = "exempt"
        if kind is not None:
            found.append((path, idx, pushed, kind))
    return found


def scan_all() -> list:
    results = []
    py_files = [p for d in PY_DIRS for p in sorted(d.rglob("*.py"))]
    for p in py_files:
        results.extend(scan_file(p, is_rust=False))
    if RS_DIR.is_dir():
        for p in sorted(RS_DIR.rglob("*.rs")):
            results.extend(scan_file(p, is_rust=True))
    return results


def test_every_positional_seed_push_is_fenced_value_form_or_exempt() -> None:
    results = scan_all()
    violations = [r for r in results if r[3] == "VIOLATION"]
    assert not violations, (
        "Unfenced argv seed seam(s) - a leading-flag seed would die in the "
        "launcher parser. Fence with a `--` element immediately before the "
        "message (or mark `argv-fence: exempt` with a why-comment):\n"
        + "\n".join(f"  {p}:{n}: {text}" for p, n, text, _ in violations)
    )


def test_scanner_still_sees_the_known_seams() -> None:
    # Positive control: patterns that match nothing certify an empty sweep.
    results = scan_all()
    counts = {}
    for _, _, _, kind in results:
        counts[kind] = counts.get(kind, 0) + 1
    assert counts.get("fenced", 0) >= 15, counts
    assert counts.get("value-form", 0) >= 8, counts
    assert counts.get("exempt", 0) >= 3, counts
    assert sum(counts.values()) == len(results)
