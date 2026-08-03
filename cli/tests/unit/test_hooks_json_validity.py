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
CODEX_HOOKS_JSON = REPO_ROOT / "hooks" / "codex-hooks.json"


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


def test_codex_plugin_manifest_points_to_session_start_hook() -> None:
    manifest = json.loads(
        (REPO_ROOT / ".codex-plugin" / "plugin.json").read_text(encoding="utf-8")
    )
    assert manifest["hooks"] == "./hooks/codex-hooks.json"

    data = json.loads(CODEX_HOOKS_JSON.read_text(encoding="utf-8"))
    hooks = data["hooks"]["SessionStart"]
    assert hooks == [
        {
            "matcher": "",
            "hooks": [
                {
                    "type": "command",
                    "command": "env FNO_PLATFORM=codex ${PLUGIN_ROOT}/hooks/session-start.sh",
                }
            ],
        }
    ]
    resolved = hooks[0]["hooks"][0]["command"].replace("${PLUGIN_ROOT}", str(REPO_ROOT))
    assert str(REPO_ROOT / "hooks" / "session-start.sh") in resolved


def test_codex_hooks_use_supported_event_names_and_existing_commands() -> None:
    data = json.loads(CODEX_HOOKS_JSON.read_text(encoding="utf-8"))
    supported = {
        "PreToolUse",
        "PostToolUse",
        "PreCompact",
        "PostCompact",
        "SessionStart",
        "UserPromptSubmit",
        "SubagentStart",
        "SubagentStop",
        "Stop",
    }
    assert set(data["hooks"]).issubset(supported)

    copied_claude_only = {
        "WorktreeCreate",
        "CwdChanged",
        "FileChanged",
        "SessionEnd",
        "StopFailure",
    }
    assert set(data["hooks"]).isdisjoint(copied_claude_only)

    assert "${CODEX_PLUGIN_ROOT}" not in CODEX_HOOKS_JSON.read_text(encoding="utf-8")
    placeholder = re.compile(r"\$\{PLUGIN_ROOT(:-[^}]*)?\}")
    failures: list[str] = []
    for event, registrations in data["hooks"].items():
        for reg in registrations:
            for hook in reg.get("hooks", []):
                substituted = placeholder.sub(str(REPO_ROOT), hook.get("command", ""))
                m = re.search(rf"{re.escape(str(REPO_ROOT))}\S+", substituted)
                if m is None:
                    continue
                path = Path(m.group(0).rstrip(';"'))
                if not path.exists():
                    failures.append(f"{event}: missing path {path}")
    if failures:
        pytest.fail("codex-hooks.json references missing files:\n  " + "\n  ".join(failures))


def test_codex_hooks_block_canonical_edit_tools() -> None:
    data = json.loads(CODEX_HOOKS_JSON.read_text(encoding="utf-8"))
    registrations = data["hooks"]["PreToolUse"]
    worktree_guard = [
        registration
        for registration in registrations
        if registration.get("matcher") == "Edit|Write"
        and any(
            hook.get("command")
            == "bash ${PLUGIN_ROOT}/hooks/worktree-write-protect.sh"
            for hook in registration.get("hooks", [])
        )
    ]
    assert len(worktree_guard) == 1, (
        "Codex apply_patch exposes Edit and Write matcher aliases; exactly one "
        "PreToolUse registration must route them through the worktree guard"
    )


def test_hook_configs_do_not_register_harness_ownership_guard() -> None:
    for config_path in (HOOKS_JSON, CODEX_HOOKS_JSON):
        data = json.loads(config_path.read_text(encoding="utf-8"))
        commands = [
            hook.get("command", "")
            for registration in data["hooks"].get("PreToolUse", [])
            for hook in registration.get("hooks", [])
        ]
        assert not any("worktree-harness-guard" in command for command in commands), (
            f"{config_path.name} still blocks sessions by harness ownership"
        )


def test_release_codex_marketplace_points_at_repo_plugin_root() -> None:
    marketplace = json.loads(
        (REPO_ROOT / ".agents" / "plugins" / "marketplace.json").read_text(
            encoding="utf-8"
        )
    )
    assert marketplace["name"] == "footnote"
    entry = marketplace["plugins"][0]
    assert entry["name"] == "fno"
    assert entry["source"] == {"source": "local", "path": "."}
    plugin_root = (REPO_ROOT / entry["source"]["path"]).resolve()
    assert plugin_root == REPO_ROOT
    assert (plugin_root / ".codex-plugin" / "plugin.json").is_file()


def test_codex_marketplace_has_no_legacy_dev_alias() -> None:
    assert not (
        REPO_ROOT
        / ".agents"
        / "marketplaces"
        / "footnote-dev"
        / ".agents"
        / "plugins"
        / "marketplace.json"
    ).exists()


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


def test_drain_hook_wired_into_claude_and_codex_sessionstart() -> None:
    """The cross-harness mail drain must fire for a CLAUDE recipient too.

    Claude's hooks.json invokes individual SessionStart hooks and does NOT call
    the session-start.sh wrapper (that is codex-hooks.json's entry), so the drain
    block inside the wrapper never runs for claude. The receive side is only
    symmetric if inject-mail-drain-session-start.sh is ALSO in claude's array.
    """
    claude = HOOKS_JSON.read_text(encoding="utf-8")
    assert "inject-mail-drain-session-start.sh" in claude, (
        "claude SessionStart is missing the mail-drain hook: a hand-started "
        "claude session addressed claude-<id> would never drain its mail"
    )
    ss = json.loads(claude)["hooks"]["SessionStart"]
    cmds = [h["command"] for entry in ss for h in entry.get("hooks", [])]
    assert any("inject-mail-drain-session-start.sh" in c for c in cmds)


def test_plan_location_guard_wired_into_claude_and_codex_pretooluse() -> None:
    """The plan-save-location gate must fire on BOTH harnesses.

    A guard registered on one of N reachable paths is decorative: a codex
    session writing a plan never loads claude's hooks.json, and vice versa.
    Nothing else asserts these registrations, so dropping either one would
    leave the guard's own harness green while silently disabling it for half
    the fleet.
    """
    guard = "hooks/plan-location-guard.sh"
    assert (REPO_ROOT / guard).is_file(), f"guard missing at {guard}"

    for path, root_var, matcher in (
        (HOOKS_JSON, "CLAUDE_PLUGIN_ROOT", "Write"),
        (CODEX_HOOKS_JSON, "PLUGIN_ROOT", "Edit|Write"),
    ):
        registrations = json.loads(path.read_text(encoding="utf-8"))["hooks"][
            "PreToolUse"
        ]
        wired = [
            registration
            for registration in registrations
            if registration.get("matcher") == matcher
            and any(
                hook.get("command") == f"bash ${{{root_var}}}/{guard}"
                for hook in registration.get("hooks", [])
            )
        ]
        assert len(wired) == 1, (
            f"{path.name} must carry exactly one PreToolUse registration "
            f"routing matcher {matcher!r} through {guard}"
        )


def test_every_manifest_hook_is_wired_and_pretooluse_launches() -> None:
    """Every manifest command must be wired (script exists), and the read-only
    PreToolUse gates must start under the real ``$SHELL -lc`` launch path.

    A hook whose command cannot resolve (PLUGIN_ROOT empty/unset, or a script
    removed) fails OPEN in Codex: it exits 127/2 and the guard was silently
    absent. Verifying the hand-expanded absolute path always passes, because that
    path cannot reproduce a placeholder-expansion failure; only the configured
    string through the shell Codex actually invokes (codex-rs/hooks engine
    command_runner.rs) can.

    Only PreToolUse gates are LAUNCHED: SessionStart/Stop and other stateful
    hooks mutate ~/.fno when executed (inject-mail-drain advances the real mail
    cursor), so running them from a test would damage live state. Stateful
    events still get an existence check. This is the CI-catchable half; the
    live-install half is `fno doctor`. Reuses the doctor launcher so the
    isolated-temp-cwd and event-scope invariants live in one place.
    """
    shell = os.environ.get("SHELL")
    if not shell:
        pytest.skip("$SHELL unset; cannot reproduce the real launch path")

    from fno.doctor import (
        _PROBE_LAUNCH_EVENTS,
        _PROBE_MANIFESTS,
        _is_hook_launch_failure,
        _launch_plugin_hook,
        _manifest_event_commands,
        _referenced_hook_scripts,
    )

    # The probe must only execute read-only gate events. Pin the scope here so a
    # future change that adds SessionStart (stateful, mutates ~/.fno) to the
    # launch set fails this test instead of silently running it.
    assert _PROBE_LAUNCH_EVENTS == ("PreToolUse",), (
        "probe may only launch read-only PreToolUse gates; adding a stateful "
        "event here would execute it against the probe env"
    )

    failures: list[str] = []
    samples: dict[str, tuple[str, str]] = {}  # manifest name -> (command, root_var)
    for name, rel, root_var in _PROBE_MANIFESTS:
        data = json.loads((REPO_ROOT / rel).read_text(encoding="utf-8"))
        for event, command in _manifest_event_commands(data, source=name):
            for script in _referenced_hook_scripts(command, REPO_ROOT):
                if not script.is_file():
                    failures.append(f"{name}/{event}: missing script {script}")
            if event in _PROBE_LAUNCH_EVENTS:
                samples.setdefault(name, (command, root_var))
                result = _launch_plugin_hook(
                    command, root_value=str(REPO_ROOT), shell=shell, root_var=root_var
                )
                if _is_hook_launch_failure(result):
                    failures.append(
                        f"{name}/{event}: rc={result['rc']} cmd={result['resolved'][:70]}"
                    )
    assert not failures, (
        "manifest hooks missing or failing to launch through $SHELL -lc:\n  "
        + "\n  ".join(failures)
    )

    # The advertised fail-open itself: an UNRESOLVED root must make every
    # manifest's PreToolUse launch fail. Asserted per manifest so a broken root
    # on one harness cannot be masked by another; requiring every manifest to
    # contribute a sample means dropping PreToolUse from one manifest fails here
    # instead of being hidden by the other.
    expected = {name for name, _rel, _root in _PROBE_MANIFESTS}
    assert set(samples) == expected, (
        f"every manifest must carry a PreToolUse gate to probe; got {sorted(samples)}"
    )
    for name, (command, root_var) in samples.items():
        empty_root = _launch_plugin_hook(
            command, root_value="", shell=shell, root_var=root_var
        )
        assert _is_hook_launch_failure(empty_root), (
            f"{name}: empty plugin root must fail the launch (the fail-open), "
            f"got rc={empty_root['rc']}"
        )

    # Placeholder fidelity: a command using the OTHER harness's root variable
    # must fail when only this manifest's var is set, because the real harness
    # does not set the other var. Pins that the probe is not masking a
    # wrong-placeholder manifest into a false green.
    wrong = _launch_plugin_hook(
        "bash ${CLAUDE_PLUGIN_ROOT}/hooks/graph-write-protect.sh",
        root_value=str(REPO_ROOT),
        shell=shell,
        root_var="PLUGIN_ROOT",
    )
    assert _is_hook_launch_failure(wrong), (
        "a command using ${CLAUDE_PLUGIN_ROOT} must fail when only PLUGIN_ROOT "
        f"is set (real Codex sets no CLAUDE_PLUGIN_ROOT); got rc={wrong['rc']}"
    )

