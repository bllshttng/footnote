"""Preflight probe: check prerequisites for fno runtime."""
from __future__ import annotations

import shutil
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class Check:
    """Result of a single preflight check."""

    name: str
    passed: bool
    note: str = ""

    def to_dict(self) -> dict:
        d: dict = {"name": self.name, "pass": self.passed}
        if self.note:
            d["note"] = self.note
        return d


def _check_git() -> Check:
    path = shutil.which("git")
    if not path:
        return Check("git", False, note="git not found on PATH; install from https://git-scm.com/")
    return Check("git", True)


def _check_gh_auth() -> Check:
    path = shutil.which("gh")
    if not path:
        return Check(
            "gh-auth",
            False,
            note="install with `brew install gh` or see https://cli.github.com/",
        )
    # Try gh auth status - exit 0 means authenticated
    result = subprocess.run(
        ["gh", "auth", "status"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return Check("gh-auth", False, note="run `gh auth login` to authenticate")
    return Check("gh-auth", True)


def _check_python() -> Check:
    major = sys.version_info.major
    minor = sys.version_info.minor
    if major < 3 or (major == 3 and minor < 11):
        return Check(
            "python",
            False,
            note=f"python>=3.11 required; found {major}.{minor}",
        )
    return Check("python", True)


def _check_plugin_json(plugin_path: Path) -> Check:
    if not plugin_path.exists():
        return Check(
            "plugin.json",
            False,
            note=f"plugin.json not found at {plugin_path}; run fno from the plugin directory",
        )
    return Check("plugin.json", True)


def run_probe(plugin_path: Path | None = None) -> tuple[bool, list[Check]]:
    """Run all preflight checks and return (all_passed, checks).

    Args:
        plugin_path: Path to plugin.json. Defaults to .claude-plugin/plugin.json relative to cwd.
    """
    if plugin_path is None:
        plugin_path = Path(".claude-plugin") / "plugin.json"

    checks: list[Check] = [
        _check_git(),
        _check_gh_auth(),
        _check_python(),
        _check_plugin_json(plugin_path),
    ]
    all_passed = all(c.passed for c in checks)
    return all_passed, checks
