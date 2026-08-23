"""Pin the think skill to a small fixed payload.

The byte count IS the mechanism this skill relies on: a large document that
tells the agent to "scale depth to risk" produces a different process every run.
Deleting the reference bodies turns this green; the test fails (naming the
files) the moment a reference creeps back in. The measurement is a positive
byte count, not the absence of a marker.
"""

import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
THINK_DIR = ROOT / "skills" / "think"
BYTE_LIMIT = 4096


def _tracked_files() -> list[Path]:
    """Tracked files under skills/think/, relative to the repo root."""
    out = subprocess.check_output(
        ["git", "ls-files", "--", "skills/think/"],
        cwd=ROOT,
        text=True,
    ).strip()
    if not out:
        return []
    return [ROOT / line for line in out.splitlines()]


def test_think_payload_stays_under_the_byte_pin() -> None:
    files = _tracked_files()
    total = sum(f.stat().st_size for f in files)
    allowed = {"SKILL.md", "LIMITATIONS.md"}
    offenders = sorted(str(f.relative_to(ROOT)) for f in files if f.name not in allowed)
    assert total <= BYTE_LIMIT, (
        f"skills/think/ payload is {total} bytes (limit {BYTE_LIMIT}). "
        f"Offending files beyond SKILL.md: {offenders}"
    )


def test_skill_md_and_limitations_are_the_only_tracked_files() -> None:
    files = _tracked_files()
    names = sorted(f.relative_to(THINK_DIR).as_posix() for f in files)
    assert names == ["LIMITATIONS.md", "SKILL.md"], (
        f"skills/think/ must contain only SKILL.md and LIMITATIONS.md, found: {names}"
    )
