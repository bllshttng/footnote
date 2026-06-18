"""Validate hooks/hooks.json structure after the consolidation pass.

The cuts in Phase 02 removed hooks/distill-task-signal.sh plus its
TaskCreated/TaskCompleted registrations. The merges in Phase 04 updated
the postmortem script path in target-stop-hook.sh. If any of these touched
hooks.json incorrectly we want a hard test failure rather than a runtime
no-op hook discovered the next time a BLOCKED transition fires.
"""
from __future__ import annotations

import json
import os
import re
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[3]
HOOKS_JSON = REPO_ROOT / "hooks" / "hooks.json"


def test_hooks_json_is_valid_json() -> None:
    """A malformed hooks.json means every hook silently no-ops at runtime."""
    data = json.loads(HOOKS_JSON.read_text(encoding="utf-8"))
    assert isinstance(data, dict)
    assert "hooks" in data, "expected top-level 'hooks' key"


def test_hooks_json_no_distill_references() -> None:
    """Phase 02 removed skills/distill/ and hooks/distill-task-signal.sh."""
    text = HOOKS_JSON.read_text(encoding="utf-8")
    assert "distill-task-signal" not in text, (
        "hooks.json still references the removed distill-task-signal.sh hook"
    )
    # The TaskCreated and TaskCompleted hook arrays existed only to fire
    # distill-task-signal. They should be gone too; if they reappear later
    # for an unrelated hook, that's fine - this assertion catches the
    # specific stale-registration class.
    data = json.loads(text)
    hooks = data.get("hooks", {})
    for key in ("TaskCreated", "TaskCompleted"):
        if key in hooks:
            # Permit re-registration of these events for non-distill hooks
            # in the future; only fail if a distill artifact survives.
            for entry in hooks[key]:
                for hook in entry.get("hooks", []):
                    cmd = hook.get("command", "")
                    assert "distill" not in cmd, (
                        f"{key} still fires a distill-named command: {cmd}"
                    )


def test_hooks_json_command_paths_resolve() -> None:
    """Every command: path under ${CLAUDE_PLUGIN_ROOT}/... must exist on disk."""
    data = json.loads(HOOKS_JSON.read_text(encoding="utf-8"))
    placeholder = re.compile(r"\$\{CLAUDE_PLUGIN_ROOT(:-[^}]*)?\}")
    failures: list[str] = []
    for event, registrations in data.get("hooks", {}).items():
        for reg in registrations:
            for hook in reg.get("hooks", []):
                cmd_raw = hook.get("command", "")
                # The command may be a shell snippet (test -f && ... ; pwd, etc).
                # Pull the first ${CLAUDE_PLUGIN_ROOT}/... path token and verify
                # that file exists. Skip purely command-only entries
                # (e.g. `command -v foo`) where no plugin path is referenced.
                substituted = placeholder.sub(str(REPO_ROOT), cmd_raw)
                # Find the first plugin-rooted path in the command string.
                m = re.search(rf"{re.escape(str(REPO_ROOT))}\S+", substituted)
                if m is None:
                    continue
                path = Path(m.group(0).rstrip(';"'))
                if not path.exists():
                    failures.append(f"{event}: missing path {path}")
    if failures:
        pytest.fail("hooks.json references missing files:\n  " + "\n  ".join(failures))


def test_postmortem_script_is_executable() -> None:
    """Phase 04 moved generate-postmortem.sh; the stop hook must still find it.

    Even with the path updated in target-stop-hook.sh, a permission regression
    would silently disable postmortem capture on every BLOCKED transition.
    """
    pm = REPO_ROOT / "skills" / "target" / "scripts" / "postmortem" / "generate-postmortem.sh"
    assert pm.is_file(), f"postmortem generator missing at {pm}"
    assert os.access(pm, os.X_OK), f"postmortem generator not executable: {pm}"


def test_preflight_runner_is_executable() -> None:
    """Phase 04 also moved run-checks.sh; target invokes it."""
    pf = REPO_ROOT / "skills" / "target" / "scripts" / "preflight" / "run-checks.sh"
    assert pf.is_file(), f"preflight runner missing at {pf}"
    assert os.access(pf, os.X_OK), f"preflight runner not executable: {pf}"
