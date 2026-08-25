#!/usr/bin/env python3
"""Hermetic keyless dispatch-to-terminal smoke for CI and contributors."""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path


# The canonical scrub lists live in cli/src/fno/hermetic.py and
# cli/src/fno/agents/mux_spawn.py; keep this tuple aligned with their
# credential set. AUTH_TOKEN and BASE_URL are here for the same reason they
# are there: a token or a redirected base URL makes a green PASS a lie.
KEYS = (
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "AGY_API_KEY",
    "CODEX_API_KEY",
)


def _write_executable(path: Path, body: str) -> None:
    path.write_text(body, encoding="utf-8")
    path.chmod(0o755)


def _rows(path: Path) -> list[dict]:
    if not path.exists():
        return []
    rows: list[dict] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            # A malformed line (partial write, stray log) must end in a FAIL
            # receipt naming the missing marker, never a traceback.
            continue
        if isinstance(value, dict):
            rows.append(value)
    return rows


def _event_type(row: dict) -> str:
    return str(row.get("type") or row.get("kind") or "")


def _fno_agents_bin() -> str:
    repo_root = Path(__file__).resolve().parents[2]
    for profile in ("debug", "release"):
        path = repo_root / "crates" / "fno-agents" / "target" / profile / "fno-agents"
        if path.is_file() and os.access(path, os.X_OK):
            return str(path)
    raise RuntimeError(
        "fno-agents binary not present; build it with "
        "`cargo build -p fno-agents --bin fno-agents`. CI runs this smoke in "
        "the post-build shard only; the pytest shard deletes the binary on "
        "purpose so the @requires_rust wrapper skips there."
    )


def _fail(effect: str, detail: str) -> int:
    print(f"keyless smoke: FAIL: {effect}: {detail}")
    return 1


def main() -> int:
    repo_root = Path(__file__).resolve().parents[2]
    # Resolve before the dispatch runs: on a box without the binary the run
    # must fail here with the build guidance, not after dispatch has spent
    # its whole budget.
    agents_bin = _fno_agents_bin()
    with tempfile.TemporaryDirectory(prefix="fno-keyless-smoke-") as raw_root:
        root = Path(raw_root)
        project = root / "project"
        project.joinpath(".fno").mkdir(parents=True)
        provider_bin = root / "provider-bin"
        provider_bin.mkdir()
        agents_home = root / "agents"
        claims_root = root / "claims"
        fake_argv = root / "provider-argv.txt"
        session_id = "11111111-2222-3333-4444-" + "555555555555"
        head_sha = "deadbeef" * 5
        settings_text = "[review]\n" + "required_bots = []\n"

        _write_executable(
            provider_bin / "codex",
            """#!/bin/sh
set -eu
printf '%s\\n' "$@" > "$KEYLESS_ARGV_FILE"
printf '{"type":"thread.started","thread_id":"%s"}\\n' "$KEYLESS_SESSION_ID"
printf '%s\\n' '{"type":"item.completed","item":{"type":"agent_message","text":"provider output is ignored"}}'
printf '%s\\n' '{"type":"turn.completed"}'
""",
        )
        _write_executable(
            root / "gh",
            """#!/bin/sh
set -eu
case "$*" in
  *--version*) echo 'gh version 2.x' ;;
  *headRefName*) printf '{"state":"OPEN","number":1,"headRefName":"main","headRefOid":"%s"}\\n' "$KEYLESS_HEAD_SHA" ;;
  *checks*) echo '[{"name":"keyless-smoke","state":"SUCCESS","bucket":"pass"}]' ;;
  *reviews*) echo '{"reviews":[],"comments":[]}' ;;
  *pulls*comments*) echo '[]' ;;
  *) printf '%s\\n' "$KEYLESS_HEAD_SHA" ;;
esac
""",
        )
        _write_executable(
            root / "git",
            """#!/bin/sh
case "$*" in
  *--raw*) exit 1 ;;
  *) printf '%s\\n' "$KEYLESS_HEAD_SHA" ;;
esac
""",
        )

        # Strip both provider keys and FNO_* here so the hermetic claim holds
        # for a direct `python3 scripts/evals/keyless_smoke.py` run, not only
        # under the pytest wrapper.
        env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("FNO_") and key not in KEYS
        }
        env.update(
            {
                "HOME": str(root / "home"),
                "FNO_AGENTS_HOME": str(agents_home),
                "FNO_CLAIMS_ROOT": str(claims_root),
                "FNO_EVENTS_PATH": str(project / ".fno" / "events.jsonl"),
                "FNO_AGENTS_RUNTIME": "python",
                "KEYLESS_ARGV_FILE": str(fake_argv),
                "KEYLESS_SESSION_ID": session_id,
                "KEYLESS_HEAD_SHA": head_sha,
                "PATH": f"{provider_bin}{os.pathsep}{env.get('PATH', '')}",
            }
        )

        dispatch = subprocess.run(
            [
                "uv", "run", "--project", str(repo_root / "cli"), "fno-py",
                "agents", "spawn", "--name", "keyless-smoke",
                "--harness", "codex", "--substrate", "headless",
                "--cwd", str(project), "--timeout", "10", "--", "keyless seed",
            ],
            cwd=repo_root,
            env=env,
            capture_output=True,
            text=True,
            timeout=45,
        )
        if dispatch.returncode != 0:
            return _fail("dispatch", dispatch.stderr or dispatch.stdout)
        if not dispatch.stdout.strip() or "torn down" not in dispatch.stderr:
            return _fail("receipts", f"stdout={dispatch.stdout!r} stderr={dispatch.stderr!r}")
        argv = fake_argv.read_text(encoding="utf-8").splitlines() if fake_argv.exists() else []
        if not {"exec", "--json", "--"}.issubset(argv):
            return _fail("dispatch argv", repr(argv))

        project_events = project / ".fno" / "events.jsonl"
        lifecycle_events = agents_home / "events.jsonl"
        project_types = [_event_type(row) for row in _rows(project_events)]
        lifecycle_types = [_event_type(row) for row in _rows(lifecycle_events)]
        all_types = project_types + lifecycle_types
        for required in ("claim_acquired", "claim_released"):
            if required not in all_types:
                return _fail("claims", f"missing {required}: {all_types!r}")
        if all_types.index("claim_acquired") > all_types.index("claim_released"):
            return _fail("claims", f"release preceded acquire: {all_types!r}")
        if not any(kind in all_types for kind in ("agent_spawned", "agent_ask_done")):
            return _fail("registry", f"missing row-minted marker: {all_types!r}")

        loop = root / "loop"
        loop.mkdir()
        (loop / "target-state.md").write_text(
            "---\nsession_id: keyless-smoke\ncreated_at: 2026-08-23T00:00:00Z\n"
            "attended: false\nauto_merge_approved: false\n---\n",
            encoding="utf-8",
        )
        (loop / "transcript.jsonl").write_text(
            '{"message":{"role":"assistant","content":"<promise>MISSION COMPLETE</promise>"}}\n',
            encoding="utf-8",
        )
        (loop / "settings.toml").write_text(settings_text, encoding="utf-8")
        loop_events = project / ".fno" / "events.jsonl"
        terminal = subprocess.run(
            [
                agents_bin, "loop-check", "--state", str(loop / "target-state.md"),
                "--transcript", str(loop / "transcript.jsonl"), "--cwd", str(project),
                "--events", str(loop_events), "--global-events", str(root / "global-events.jsonl"),
                "--settings", str(loop / "settings.toml"), "--global-settings", str(root / "none.toml"),
                "--now", "2026-08-23T00:30:00Z", "--gh-bin", str(root / "gh"),
                "--git-bin", str(root / "git"), "--author-harness", "none",
            ],
            cwd=repo_root,
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
        )
        if terminal.returncode != 0:
            return _fail("terminal", terminal.stderr or terminal.stdout)
        try:
            decision = json.loads(terminal.stdout.strip().splitlines()[-1])
        except (IndexError, json.JSONDecodeError):
            return _fail("terminal", f"non-JSON output: {terminal.stdout!r}")
        if decision.get("decision") != "allow":
            return _fail("terminal", repr(decision))
        reason = decision.get("termination_reason")
        if not reason:
            return _fail("terminal", f"missing termination reason: {decision!r}")
        if "termination" not in [_event_type(row) for row in _rows(loop_events)]:
            return _fail("terminal events", "termination marker absent")

        print("keyless smoke: dispatch -> claims -> registry -> terminal -> receipts")
        print(f"keyless smoke: PASS (terminal reason={reason})")
        return 0


if __name__ == "__main__":
    sys.exit(main())
