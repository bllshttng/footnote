"""Every env var source reads must be classified as ambient or environment.

This is the guard that stops ``hermetic.py`` becoming the next allowlist that
loses to the fifth thing nobody thought of.

The three specimens on 2026-08-11 all had the same root shape: an author
sandboxed the channels they could think of, and the channel they could not
think of stayed open until a human noticed a red suite. A list is only as good
as the next person's memory, so this test replaces the memory. Add an env read
to source, and CI asks you - by name - whether a test may see the developer's
value for it.

Writing the decision down is the whole job; both answers are fine.
"""
from __future__ import annotations

import collections
import re
from pathlib import Path

import pytest

from fno.hermetic import classify

REPO_ROOT = Path(__file__).resolve().parents[3]

# The read shapes that reach the process environment, in both languages.
_PATTERNS = (
    r'environ\.get\(\s*["\']([A-Za-z_][A-Za-z0-9_]*)["\']',
    r'environ\[\s*["\']([A-Za-z_][A-Za-z0-9_]*)["\']',
    r'getenv\(\s*["\']([A-Za-z_][A-Za-z0-9_]*)["\']',
    r'env::var(?:_os)?\(\s*"([A-Za-z_][A-Za-z0-9_]*)"',
)

# Source trees only. A test file setting its own marker is not a leak; it is a
# test doing its job.
_SOURCE_ROOTS = (("cli/src", "*.py"), ("crates", "*.rs"))


def _env_reads() -> dict[str, set[str]]:
    """{env name -> {files reading it}} across the source trees."""
    found: dict[str, set[str]] = collections.defaultdict(set)
    for rel, glob in _SOURCE_ROOTS:
        root = REPO_ROOT / rel
        if not root.exists():  # pragma: no cover - a partial checkout
            continue
        for path in root.rglob(glob):
            parts = path.parts
            if "tests" in parts or "target" in parts or path.name.startswith("test_"):
                continue
            text = path.read_text(errors="replace")
            for pattern in _PATTERNS:
                for name in re.findall(pattern, text):
                    found[name].add(str(path.relative_to(REPO_ROOT)))
    return found


def test_the_probe_actually_finds_env_reads():
    """Positive control.

    An empty result would make the classification test below pass while
    measuring nothing - the absence-only failure. HOME is read ~35 times in
    source; if the probe cannot see it, the probe is broken, not the code.
    """
    reads = _env_reads()
    assert "HOME" in reads, "env-read probe found nothing; the regexes are stale"
    assert len(reads) > 20, f"probe found only {len(reads)} names; expected dozens"


def test_every_env_read_in_source_is_classified():
    """The guard itself."""
    unclassified = {
        name: sorted(files)[:3]
        for name, files in _env_reads().items()
        if classify(name) == "unclassified"
    }
    if unclassified:
        lines = [
            f"  {name}  (read in {', '.join(files)})"
            for name, files in sorted(unclassified.items())
        ]
        pytest.fail(
            "These env vars are read by source but not classified in "
            "fno/hermetic.py:\n"
            + "\n".join(lines)
            + "\n\nDecide for each one: does a test get to see the developer's "
            "value?\n"
            "  no  -> add it to _AMBIENT_NAMES (it gets scrubbed)\n"
            "  yes -> add it to _ENVIRONMENT with a comment saying why\n"
            "Either answer is fine. Leaving it undecided is the thing that "
            "broke three test suites on 2026-08-11.",
            pytrace=False,
        )


def test_a_name_cannot_be_both_ambient_and_environment():
    """Overlap would make the answer depend on rule order rather than intent."""
    from fno.hermetic import _AMBIENT_NAMES, _ENVIRONMENT, _RUNNER_PASSTHROUGH

    overlap = (set(_AMBIENT_NAMES) & set(_ENVIRONMENT)) | (
        set(_AMBIENT_NAMES) & set(_RUNNER_PASSTHROUGH)
    )
    assert not overlap, f"classified both ways: {sorted(overlap)}"


def test_classify_agrees_with_the_scrub():
    """The reporting view and the acting view must not drift.

    A classification that says "ambient" while neutralise keeps the value would
    make the guard decorative: green, and wrong.
    """
    import tempfile

    from fno.hermetic import neutralise

    sandbox = Path(tempfile.mkdtemp())
    probes = {name: "ambient-value" for name in _env_reads()}
    out = neutralise(probes, sandbox)
    for name in probes:
        if classify(name) == "ambient":
            assert out.get(name) != "ambient-value", (
                f"{name} classifies as ambient but survived neutralise()"
            )
