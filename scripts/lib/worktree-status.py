#!/usr/bin/env python3
"""Repository entry point for the packaged worktree-status implementation."""

import sys
from pathlib import Path


repo_src = Path(__file__).resolve().parents[2] / "cli" / "src"
if repo_src.is_dir():
    sys.path.insert(0, str(repo_src))

from fno.worktree_status import main  # noqa: E402


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
