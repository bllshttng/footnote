#!/usr/bin/env python3
"""Regenerate the Python golden payloads for the session_truth parity test.

Run from the repo root (or anywhere; paths resolve from this file):

    python3 crates/fno-agents/tests/fixtures/session_truth/make_goldens.py

The `fno-agents` binary must be built: the peer-role classifier shells to it,
exactly as production does. Fixtures and goldens are committed together.
"""

from __future__ import annotations

import calendar
import json
import os
import sqlite3
import sys
import time
from pathlib import Path
from types import SimpleNamespace

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[4]
sys.path.insert(0, str(REPO / "cli" / "src"))

import fno.agents.session_truth as st  # noqa: E402

NOW = calendar.timegm(time.strptime("2026-10-08T12:00:00Z", "%Y-%m-%dT%H:%M:%SZ"))


def iso(delta_s: int) -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(NOW + delta_s))


def claude_line(role: str, text: str, delta_s: int, model: str | None = None) -> str:
    msg: dict = {"role": role, "content": text}
    if model:
        msg["model"] = model
    return json.dumps({"type": role, "timestamp": iso(delta_s), "message": msg})


def write(name: str, lines: list[str]) -> Path:
    path = HERE / name
    path.write_text("".join(line + "\n" for line in lines), encoding="utf-8")
    return path


def session(agent: str, sid: str, path: Path) -> SimpleNamespace:
    return SimpleNamespace(
        agent=agent, session_id=sid, cwd="/fixture/project", transcript_path=str(path)
    )


CASES: list[dict] = []


def claude_case(name: str, sid: str, lines: list[str]) -> None:
    path = write(f"{name}.jsonl", lines)
    CASES.append({"name": name, "agent": "claude", "sid": sid, "path": path})


def codex_case(name: str, sid: str, lines: list[str]) -> None:
    path = write(f"{name}.jsonl", lines)
    CASES.append({"name": name, "agent": "codex", "sid": sid, "path": path})


claude_case(
    "claude_done",
    "sid-done-0001",
    [
        claude_line("user", "ship the gate", -120, model=None),
        claude_line("assistant", "<promise>MISSION COMPLETE: gate shipped</promise>", -60, model="claude-sonnet-5-5"),
    ],
)
claude_case(
    "claude_watching",
    "sid-watch-0001",
    [
        claude_line("assistant", '<watching reason="ci" pr="12">waiting on CI</watching>', -120, model="claude-sonnet-5-5"),
    ],
)
claude_case(
    "claude_your_move",
    "sid-move-0001",
    [
        claude_line("assistant", "The schema has two candidate keys. Should I proceed with the migration?", -300, model="claude-sonnet-5-5"),
    ],
)
claude_case(
    "claude_your_move_option",
    "sid-opt-0001",
    [
        claude_line("assistant", "Apply this to all four files? [Y/n]", -300, model="claude-sonnet-5-5"),
    ],
)
claude_case(
    "claude_api_error",
    "sid-apierr-0001",
    [
        claude_line("assistant", "API Error: 429 usage limit reached for this account", -30, model=None),
    ],
)
claude_case(
    "claude_stalled",
    "sid-stall-0001",
    [
        claude_line("assistant", "Reading the ledger now.", -10800, model="claude-sonnet-5-5"),
    ],
)
claude_case(
    "claude_working",
    "sid-work-0001",
    [
        claude_line("user", "summarize the failures", -60),
        claude_line("assistant", "Reading the ledger now.", -30, model="claude-sonnet-5-5"),
    ],
)
claude_case(
    "claude_user_clears_promise",
    "sid-clear-0001",
    [
        claude_line("assistant", "<promise>MISSION COMPLETE: earlier task</promise>", -3600, model="claude-sonnet-5-5"),
        claude_line("user", "new task: sweep the backlog", -60),
    ],
)
claude_case(
    "claude_peer_skips",
    "sid-peer-0001",
    [
        claude_line("assistant", "<promise>MISSION COMPLETE: earlier task</promise>", -60, model="claude-sonnet-5-5"),
        claude_line("user", '<cross-session-message from="lead-session-1">ack, continuing</cross-session-message>', -30),
    ],
)
claude_case(
    "claude_title_and_synthetic_model",
    "sid-title-0001",
    [
        claude_line("assistant", "first real answer", -300, model="claude-sonnet-5-5"),
        json.dumps({"type": "agent-name", "timestamp": iso(-120), "agentName": "tgt-x-aaaa-liveness"}),
        json.dumps({"type": "assistant", "timestamp": iso(-30), "message": {"role": "assistant", "content": "Interrupted by the user", "model": "<synthetic>"}}),
    ],
)
codex_case(
    "codex_working",
    "sid-codex-0001",
    [
        json.dumps({"type": "turn_context", "timestamp": iso(-90), "payload": {"model": "gpt-6.1-sol"}}),
        json.dumps({"type": "response_item", "timestamp": iso(-45), "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Running the sweep."}]}}),
    ],
)
CASES.append({"name": "unknown_handle", "agent": None, "sid": None, "path": None})

# opencode: a fixture store the reader opens through rusqlite.
DB_PATH = HERE / "opencode.db"
STORAGE_DIR = HERE / "storage"
if DB_PATH.exists():
    DB_PATH.unlink()
conn = sqlite3.connect(DB_PATH)
conn.execute("CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT)")
conn.execute("CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT)")
conn.execute("INSERT INTO message VALUES ('m1', 'ses_goldenfix01', 1000, '{\"role\": \"user\"}')")
conn.execute("INSERT INTO part VALUES ('p1', 'm1', 'ses_goldenfix01', 1000, '{\"type\": \"text\", \"text\": \"run the board sweep\"}')")
conn.execute("INSERT INTO message VALUES ('m2', 'ses_goldenfix01', 2000, '{\"role\": \"assistant\"}')")
conn.execute("INSERT INTO part VALUES ('p2', 'm2', 'ses_goldenfix01', 2000, '{\"type\": \"tool\", \"tool\": \"bash\"}')")
conn.execute("INSERT INTO part VALUES ('p3', 'm2', 'ses_goldenfix01', 2100, '{\"type\": \"text\", \"text\": \"Sweep done.\"}')")
conn.commit()
conn.close()

CASES.append({"name": "opencode_fixture", "agent": "opencode", "sid": "ses_goldenfix01", "path": None})

goldens: dict = {"now_s": NOW, "cases": {}}
for case in CASES:
    name = case["name"]
    if name == "unknown_handle":
        resolver = lambda handle: (None, [])  # noqa: E731
        result = st.resolve_session_truth(name, resolve=resolver, now_s=NOW)
    elif case["agent"] == "opencode":
        resolved = SimpleNamespace(
            agent="opencode",
            session_id=case["sid"],
            cwd="/fixture/project",
            transcript_path=None,
        )
        result = st.resolve_session_truth(
            name,
            resolve=lambda handle: (resolved, []),
            opencode_storage_dir=STORAGE_DIR,
            now_s=NOW,
        )
    else:
        resolved = session(case["agent"], case["sid"], case["path"])
        result = st.resolve_session_truth(
            name, resolve=lambda handle: (resolved, []), now_s=NOW
        )
    payload = {}
    from fno.agents.reachability import classify_reachability

    reach = classify_reachability(
        truth_state=result.get("state"),
        age_s=result.get("last_activity_age_s"),
        falsifier=None,
        observed_model=result.get("observed_model"),
    )
    for key in (
        "handle", "state", "reason", "last_activity_age_s", "last_event_at",
        "last_activity_basis", "last_message", "provider_refusal", "session_id",
        "observed_model", "harness_title", "suggestions",
    ):
        payload[key] = result.get(key)
    payload["reachability"] = reach.verdict
    payload["basis"] = reach.basis
    payload["falsifier_error"] = None
    goldens["cases"][name] = payload

(HERE / "goldens.json").write_text(json.dumps(goldens, indent=1, sort_keys=True) + "\n", encoding="utf-8")
print("goldens written:", len(goldens["cases"]), "cases")
for name, payload in goldens["cases"].items():
    print(f"  {name}: state={payload['state']} reason={payload['reason']} basis={payload['last_activity_basis']} reach={payload['reachability']}/{payload['basis']}")
