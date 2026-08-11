"""Hook output strings must prescribe only fno verbs that resolve.

`hooks/context-nudge.sh` fires `REASON` / `ORPHAN_REASON` / `FLUSH_REASON`
strings into a live session under context pressure, so an agent reads them and
acts without a chance to check. Every `fno` command those strings name must
resolve against the checked-in verb surface (`scripts/ci/verb-baseline.txt`).
A hook string is the highest-cost instance of the prescribed-verb class: a dead
verb or a wrong-layer transport here is acted on blind.

Backstory: the hook once prescribed `fno agents crown --succeed` (a deleted
verb) and `fno-agents mail-inject` (the daemon transport, not the agent front
door). Both were caught by a human noticing a worker echo them back, not by any
gate. The live forms are `fno agents spawn --crown <scope>` and
`fno mail send '<verb>' --to-self --raw`.

Scope is the `fno` surface that `verb-baseline.txt` covers. `bash <script>` and
harness builtins like `/compact` are separate surfaces (a foreign-verb allowlist
is wave 2) and are not asserted here.
"""
from __future__ import annotations

import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
HOOK = REPO_ROOT / "hooks" / "context-nudge.sh"
BASELINE = REPO_ROOT / "scripts" / "ci" / "verb-baseline.txt"

# The three variables the hook assigns its emitted (user-facing) strings to.
_OUTPUT_VAR = re.compile(r"^\s*(REASON|ORPHAN_REASON|FLUSH_REASON)=", re.MULTILINE)
_FNO = re.compile(r"\bfno\b\s+")
_WORD = re.compile(r"[A-Za-z][\w-]*")
# `fno-agents` binary verbs are a separate surface, hidden from the fno ratchet.
# The front door `fno mail send --raw` routes to them; prescribing the binary
# directly is the transport-not-front-door defect, so none should appear.
_FNO_AGENTS = re.compile(r"\bfno-agents\b\s+(\S+)")


def _baseline_leaves() -> set[str]:
    leaves: set[str] = set()
    for line in BASELINE.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        toks = [t for t in line.split() if not t.startswith("!")]
        if toks:
            leaves.add(" ".join(toks))
    return leaves


def _output_lines(hook_text: str) -> list[str]:
    return [ln for ln in hook_text.splitlines() if _OUTPUT_VAR.match(ln)]


def _extract_fno_verbs(line: str, leaves: set[str]) -> list[str]:
    """Resolve each `fno ...` in a line to its longest baseline-leaf prefix.

    Captures word tokens after `fno` until a non-verb token (flag, quote,
    placeholder, path) and resolves to the longest prefix that is a real leaf,
    so prose like 'fno mail send is the door' resolves to 'mail send' rather
    than over-capturing the trailing words.
    """
    resolved: list[str] = []
    for m in _FNO.finditer(line):
        pos = m.end()
        tokens: list[str] = []
        while pos < len(line) and len(tokens) < 3:
            wm = _WORD.match(line, pos)
            if not wm:
                break
            tokens.append(wm.group())
            nxt = wm.end()
            if nxt < len(line) and line[nxt] in " \t":
                pos = nxt + 1
            else:
                break
        hit: str | None = None
        for k in range(len(tokens), 0, -1):
            cand = " ".join(tokens[:k])
            if cand in leaves:
                hit = cand
                break
        resolved.append(hit if hit is not None else " ".join(tokens))
    return resolved


def test_hook_output_commands_resolve() -> None:
    leaves = _baseline_leaves()
    lines = _output_lines(HOOK.read_text(encoding="utf-8"))
    assert lines, "no REASON/ORPHAN_REASON/FLUSH_REASON assignments found"

    prescribed: list[str] = []
    for ln in lines:
        prescribed.extend(_extract_fno_verbs(ln, leaves))

    # Positive control: the scan reached real content, not an empty parse.
    # An absence here has two explanations and only the positive marker tells
    # them apart (assert-a-positive-marker).
    assert "mail send" in prescribed, "expected the /compact front door in output"
    assert "agents spawn" in prescribed, "expected the spawn --crown path in output"

    bad = [v for v in prescribed if v not in leaves]
    assert not bad, f"hook output prescribes non-resolving fno verbs: {bad}"


def test_hook_output_prescribes_no_dead_verbs_or_transport() -> None:
    emitted = "\n".join(_output_lines(HOOK.read_text(encoding="utf-8")))
    # crown was deleted; the live succession is `fno agents spawn --crown`.
    assert "agents crown " not in emitted, "dead verb `fno agents crown` in output"
    assert "--succeed" not in emitted, "dead `--succeed` flag in output"
    # mail-inject is the daemon transport, not the agent front door.
    assert not _FNO_AGENTS.search(emitted), (
        "hook output prescribes the fno-agents transport binary directly; "
        "the front door is `fno mail send --raw`"
    )
