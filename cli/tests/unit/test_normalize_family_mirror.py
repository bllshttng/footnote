"""x-8151: normalize.sh's family mirror may not drift from the source table.

skills/agent/scripts/normalize.sh classifies every spawn input, so its
/target-family membership test is a hot regex mirror of
harness_map._TARGET_FAMILY (the same generated-checked shape as
resolve_command_surface's static fallback). This test parses the mirror out
of the shell and asserts it against the source table by behavior - a
spelling the source knows must match, and near-misses must not - so the
hand-copied-list drift that dropped opencode refusals cannot come back.
Slow paths ask the source directly via `fno agents dispatch family`; this
mirror serves the hot classifier that cannot afford a subprocess.
"""

import re
from pathlib import Path

from fno.agents.harness_map import _TARGET_FAMILY

NORM_PATH = (
    Path(__file__).resolve().parents[3] / "skills" / "agent" / "scripts" / "normalize.sh"
)


def _mirror_regex() -> str:
    src = NORM_PATH.read_text()
    match = re.search(r"_normalize_family_regex='([^']+)'", src)
    assert match, (
        "the _normalize_family_regex mirror is missing from normalize.sh; "
        "the hot classifier would silently stop recognizing family messages"
    )
    # POSIX character class -> Python equivalent; the rest is ERE-compatible.
    return match.group(1).replace("[[:space:]]", r"\s")


def test_mirror_matches_every_source_spelling():
    regex = re.compile(_mirror_regex())
    for spelling in _TARGET_FAMILY:
        assert regex.search(spelling), f"mirror misses source spelling {spelling!r}"
        assert regex.search(f"{spelling} x-1"), (
            f"mirror misses {spelling!r} with an argument"
        )


def test_mirror_rejects_non_family_shapes():
    regex = re.compile(_mirror_regex())
    for near_miss in ("/targetx x-1", "/fno:review x-1", "$fno:think x-1", "prose note"):
        assert not regex.search(near_miss), f"mirror wrongly matches {near_miss!r}"
