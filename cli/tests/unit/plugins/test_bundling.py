from __future__ import annotations

import os
import subprocess
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[4]


def test_generator_refuses_to_clobber_non_pack_destination(tmp_path: Path) -> None:
    # AC10-ERR: an existing destination whose frontmatter has no `pack:` key is
    # not overwritten. The generator names both paths and exits non-zero.
    target_root = tmp_path / "repo"
    (target_root / "agents").mkdir(parents=True)
    hand_authored = target_root / "agents" / "growth-marketer.md"
    hand_authored.write_text(
        "---\n"
        "name: growth-marketer\n"
        "description: a hand-authored file, not a bundled pack output.\n"
        "---\n\nbody\n",
        encoding="utf-8",
    )

    env = {**os.environ, "REPO_ROOT": str(target_root)}
    result = subprocess.run(
        ["bash", str(REPO_ROOT / "scripts" / "generate-skill-bundles.sh")],
        capture_output=True,
        text=True,
        env=env,
    )

    assert result.returncode != 0
    assert "refusing to overwrite" in result.stderr
    assert "agents/growth-marketer.md" in result.stderr
    # The hand-authored file is unmodified.
    assert "hand-authored" in hand_authored.read_text(encoding="utf-8")
