"""The graph status vocabulary is shared across a language boundary.

`crates/fno/src/backlog_view.rs` reads graph.json directly to render the mux
sideline. It is tolerant by design -- a display surface must not refuse to draw
because a status is unfamiliar -- but that tolerance is exactly what makes drift
silent: adding a status on the Python side and forgetting the Rust reader means
those nodes vanish from the sideline with no error anywhere.

This is the check the epic's census originally proposed solving with a
schema-version stamp on graph.json. A stamp needs a writer change plus a version
integer a human must remember to bump, which is the same discipline problem one
level removed. Reading the vocabulary out of both sources and comparing them
catches the same drift mechanically, on every `fno doctor test`, with no runtime cost
and no new field on disk.

Lives on the Python side deliberately: the edit that causes the drift is a
Python edit, so the failure should land in the suite that edit already runs.
"""
from __future__ import annotations

import re
from pathlib import Path

import pytest

from fno.graph.statuses import STATUS_MIGRATION, VALID_STATUSES

_RUST_READER = (
    Path(__file__).resolve().parents[3] / "crates" / "fno" / "src" / "backlog_view.rs"
)


def _classify_body() -> str:
    """The source text of `fn classify`, from its signature to the closing brace.

    Bounded by the next top-level `fn ` rather than by brace counting: the body
    is a flat match with no nested items, so the simpler cut is also the one
    that cannot silently mis-parse.
    """
    src = _RUST_READER.read_text(encoding="utf-8")
    start = src.index("fn classify(")
    rest = src[start + 1 :]
    end = rest.find("\nfn ")
    return rest[: end if end != -1 else len(rest)]


def _named_statuses() -> set[str]:
    """Every string literal that is actually a classify match arm.

    Anchored on the `=>` rather than matching any quoted token in the body: a
    status name mentioned in an in-body comment would otherwise count as
    "named", certifying a gap that is not closed. Today's comments use backticks
    so nothing is mis-counted, but that is a style convention nothing enforces.
    """
    arms = re.findall(r'((?:"[a-z_-]+"\s*\|\s*)*"[a-z_-]+")\s*=>', _classify_body())
    return {lit for arm in arms for lit in re.findall(r'"([a-z_-]+)"', arm)}


@pytest.mark.parametrize(
    "status", sorted(VALID_STATUSES | set(STATUS_MIGRATION)), ids=str
)
def test_rust_reader_names_every_python_status(status: str) -> None:
    """A status Python knows must have an explicit arm in the Rust reader.

    Explicit means named, not merely swallowed by `_ => None`: dropping a status
    from the sideline is a legitimate choice (`done`, `idea`), but it has to be a
    choice someone made rather than the catch-all absorbing it. Extra arms in
    Rust are fine -- they carry legacy spellings Python has already dropped.
    """
    assert status in _named_statuses(), (
        f"graph status {status!r} is in the Python vocabulary but has no arm in "
        f"{_RUST_READER.name}::classify, so nodes with that status are silently "
        f"dropped from the mux sideline. Add an explicit arm -- Some(..) if it is "
        f"queue work, None if it deliberately is not."
    )


def test_reader_source_is_parseable() -> None:
    """Guard the guard: a rename or refactor upstream must not quietly no-op it.

    Without this, moving `classify` or renaming the file turns every parametrized
    case above into a vacuous pass against an empty set.
    """
    assert _RUST_READER.exists(), f"{_RUST_READER} moved; update this test's path"
    named = _named_statuses()
    assert {"ready", "blocked"} <= named, (
        f"parsed only {sorted(named)} out of classify -- the extraction broke rather "
        f"than the vocabulary drifting"
    )
