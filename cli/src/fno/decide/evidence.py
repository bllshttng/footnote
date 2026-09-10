"""The read that produced a ruling's code fact, carried on the ruling.

Three rulings in one afternoon named a code fact that was false, and each was
caught downstream by a worker re-measuring - a review round the two-round cap
could not afford. The shape: a measured claim and an assumed one are written
identically. So a ruling body asserting a code fact must carry `--read`: the
command that produced it. The verb RUNS the command at record time and stores
the exit code and output head on the row. A string nobody ran is prose; a row
with an exit code is evidence.
"""

from __future__ import annotations

import re
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Callable

# The claim vocabulary, listed in ONE place so a reviewer can audit it in one
# read. A false positive on ordinary prose is the failure that gets this gate
# disabled, so it fires on two shapes and nothing else: a `path:line` citation
# over a source extension, and a count bound to a fixed noun list (or its
# negative - "zero callers" is a code fact too, and the expensive kind).
SOURCE_EXTS = (
    "py", "rs", "ts", "tsx", "js", "jsx", "sh", "go", "rb", "java",
    "c", "h", "cpp", "md", "toml", "yaml", "yml", "json",
)
_NOUNS = r"(?:lines?|call sites?|callers?|consumers?|usages?|occurrences?|matches|files?)"
_CITATION_RE = re.compile(
    r"\b[\w./\\-]+\.(?:" + "|".join(SOURCE_EXTS) + r"):\d+(?:-\d+)?"
)
_COUNTED_RE = re.compile(r"\b\+?\d+\s+" + _NOUNS)
_NEGATIVE_RE = re.compile(r"\b(?:no|zero)\s+" + _NOUNS)
MAX_READS = 5
OUT_HEAD_LINES = 5
OUT_HEAD_CHARS = 400


class UnmeasuredClaimError(ValueError):
    """A code fact stated with no read attached."""


class UnresolvableCitationError(ValueError):
    """A citation the repo contradicts."""


def _unique(matches: "list[str]") -> "list[str]":
    seen: "set[str]" = set()
    return [m for m in matches if not (m in seen or seen.add(m))]


def find_code_claims(text: str) -> "list[str]":
    """Claim spans in a ruling body, empty when it asserts none."""
    return _unique(_CITATION_RE.findall(text) + _NEGATIVE_RE.findall(text)
                   + _COUNTED_RE.findall(text))


def _tracked_files(root: Path) -> "list[str]":
    try:
        done = subprocess.run(
            ["git", "ls-files"], cwd=root, capture_output=True, text=True,
            timeout=10, check=False,
        )
        if done.returncode == 0:
            return [line for line in done.stdout.splitlines() if line]
    except (OSError, subprocess.TimeoutExpired):
        pass
    return sorted(  # not a repo (a hermetic test tmp dir): every file counts
        str(p.relative_to(root)) for p in root.rglob("*")
        if p.is_file() and ".git" not in p.parts
    )


def check_citations(text: str, *, root: Path | None = None) -> "list[str]":
    """One failure line per citation the repo contradicts, empty when clean."""
    from fno.paths import resolve_repo_root

    root = root or resolve_repo_root()
    tracked = _tracked_files(root)
    failures: "list[str]" = []
    for cite in _unique(_CITATION_RE.findall(text)):
        path_text, _, line_text = cite.rpartition(":")
        line = int(line_text.partition("-")[0])
        exact = [p for p in tracked if p.replace("\\", "/") == path_text]
        if not exact:
            named = [p for p in tracked if p.rsplit("/", 1)[-1] == path_text]
            if len(named) == 1:
                exact = named
            elif len(named) > 1:
                failures.append(
                    f"{cite}: '{path_text}' resolves to {len(named)} tracked files; "
                    "write the repo-relative path"
                )
                continue
            else:
                failures.append(f"{cite}: names no tracked file")
                continue
        try:
            length = len((root / exact[0]).read_text(errors="replace").splitlines())
        except OSError:
            failures.append(f"{cite}: the file exists but is unreadable")
            continue
        if line > length:
            failures.append(f"{cite}: the file has {length} lines")
    return failures


def _default_run(cmd: str, *, cwd: Path, timeout: int):
    return subprocess.run(
        cmd, shell=True, cwd=cwd, timeout=timeout, capture_output=True,
        text=True, check=False,
    )


def _head_sha(root: Path) -> str:
    try:
        done = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=root, capture_output=True,
            text=True, timeout=10, check=False,
        )
        return done.stdout.strip() if done.returncode == 0 else ""
    except (OSError, subprocess.TimeoutExpired):
        return ""


def _zeroish(row: dict) -> bool:
    out = str(row["out_head"]).strip()
    return out == "" or out == "0" or (row["exit"] == 1 and out == "")


def run_reads(
    commands: "list[str]",
    *,
    root: Path | None = None,
    timeout: int = 20,
    run: Callable[..., Any] = _default_run,
) -> "list[dict]":
    """Run each read, bounded, and return one evidence row per command.

    Refusals, both earned: a read that cannot run stores no row (a ruling
    whose own read does not run is not evidence), and an all-zero result set
    refuses under the standing pitfall - assert a positive marker, never an
    absence - so a zero must arrive beside a control aimed at something known
    to be present. A non-zero exit WITH output is a measurement, not a
    failure: `grep -c` exiting 1 on a real zero is the read working.
    """
    from fno.paths import resolve_repo_root

    root = root or resolve_repo_root()
    if len(commands) > MAX_READS:
        raise UnmeasuredClaimError(
            f"cap is {MAX_READS} reads per ruling, got {len(commands)}"
        )
    rows: "list[dict]" = []
    for cmd in commands:
        try:
            done = run(cmd, cwd=root, timeout=timeout)
        except subprocess.TimeoutExpired as exc:
            raise UnmeasuredClaimError(
                f"read '{cmd}' timed out after {timeout}s and stored no row. "
                "A ruling whose own read does not run is not evidence."
            ) from exc
        except OSError as exc:
            raise UnmeasuredClaimError(
                f"read '{cmd}' could not run: {exc}. A ruling whose own read "
                "does not run is not evidence."
            ) from exc
        head = "\n".join((done.stdout or "").splitlines()[:OUT_HEAD_LINES])
        rows.append({
            "cmd": cmd,
            "exit": done.returncode,
            "out_head": head[:OUT_HEAD_CHARS],
            "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "head_sha": _head_sha(root),
        })
    if rows and all(_zeroish(row) for row in rows):
        raise UnmeasuredClaimError(
            f"read '{rows[0]['cmd']}' produced a zero. A zero needs a control: "
            "a second --read of the same shape aimed at something known to be "
            "present. Check the axes that could move the reading - symbol, "
            "age, directory - before trusting the zero."
        )
    return rows


def unmeasured_note_warning(claims: "list[str]") -> str:
    return (
        f"note appended with an unmeasured code fact ('{claims[0]}'): a reader "
        "cannot tell measured from assumed. Attach --read <command> - it runs "
        "at record time and its output is stored; pair a zero with a control."
    )


def _body(decision: str, rationale: str | None) -> str:
    return f"{decision}\n{rationale or ''}"


def check_ruling_evidence(
    decision: str,
    rationale: str | None,
    reads: "list[str] | None",
    *,
    root: Path | None = None,
) -> "list[dict] | None":
    """Gate for the ruling lane; returns the rows to store, or None.

    Order matters: a citation the repo contradicts is refused whatever is
    attached to it, then a claim with no read is refused. No claim, no
    change from today's behavior.
    """
    text = _body(decision, rationale)
    failures = check_citations(text, root=root)
    if failures:
        raise UnresolvableCitationError(
            "; ".join(failures)
            + ". Fix or drop the citation; an attached read does not save it."
        )
    claims = find_code_claims(text)
    if not claims:
        return None
    if not reads:
        raise UnmeasuredClaimError(
            f"the ruling asserts a code fact ('{claims[0]}') and carries no "
            "read. Attach --read with the command that produced it; the "
            "command runs at record time and its output is stored on the "
            "row. Example: --read \"rg -c 'def record' cli/src/fno/law.py\". "
            "Pair a zero with a control."
        )
    return run_reads(list(reads), root=root)


def note_evidence(
    text: str, reads: "list[str] | None", *, root: Path | None = None
) -> "tuple[list[dict] | None, list[str] | None]":
    """Gate for the note lane: (rows, claims), each None.

    Split disposition on purpose: a contradicted citation RAISES (a note
    naming a file:line the repo contradicts is a demonstrably false fact),
    while a claim with no read only reports - the note verb advises, never
    refuses a body.
    """
    failures = check_citations(text, root=root)
    if failures:
        raise UnresolvableCitationError(
            "; ".join(failures)
            + ". Fix or drop the citation; a note is a fact on the node even "
            "when --quiet."
        )
    claims = find_code_claims(text)
    if not claims:
        return None, None
    if not reads:
        return None, claims
    return run_reads(list(reads), root=root), None


def warn_if_note_is_long(text: str, *, stream: Any = sys.stderr) -> None:
    """Advise on a long note, never refuse one.

    Why uncapped, and why the blunt multiplier:
    docs/architecture/backlog-graph-verb-contracts.md.
    """
    from fno import style

    try:
        from fno.config import load_settings

        cap = load_settings().style.word_cap.encounter
    except Exception:  # noqa: BLE001 - an advisory must never break a write
        cap = style.MESSAGE_WORD_CAP
    count = style.word_count(text)
    if count <= cap * 4:
        return
    print(
        f"note appended ({count} words). Long evidence belongs in a plan doc; "
        "a note carrying a path is cheaper for every later reader.",
        file=stream,
    )


__all__ = [
    "MAX_READS",
    "UnmeasuredClaimError",
    "UnresolvableCitationError",
    "check_citations",
    "check_ruling_evidence",
    "find_code_claims",
    "note_evidence",
    "run_reads",
    "unmeasured_note_warning",
    "warn_if_note_is_long",
]
