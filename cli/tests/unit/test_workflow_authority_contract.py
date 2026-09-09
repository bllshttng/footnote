"""Behavioral contract: workflow skills follow the authority their helpers enforce.

Each skill in this file once restated a policy its owning implementation had
replaced, so an LLM following the prose could re-run a mutation the helper
refuses or gate a launch the helper frees. These tests pin the agreement from
both sides: run the helper and assert its receipt, then assert the skill names
that receipt's fields as the thing it obeys. Positive markers first - a stale
prose absence alone proves nothing about what replaced it.
"""
from __future__ import annotations

import os
import re
import subprocess
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
HELPER = REPO_ROOT / "skills" / "agent" / "scripts" / "confirm-decision.sh"


def _skill(rel: str) -> str:
    return (REPO_ROOT / rel).read_text(encoding="utf-8")


def _run_confirm_helper(*args: str, reader: str = "auto") -> dict[str, str]:
    env = dict(os.environ, DISPATCH_CONFIRM_READER=f"echo {reader}")
    proc = subprocess.run(
        ["bash", str(HELPER), *args],
        capture_output=True,
        text=True,
        env=env,
        timeout=30,
    )
    assert proc.returncode == 0, proc.stderr
    fields = {}
    for line in proc.stdout.splitlines():
        key, _, value = line.partition("=")
        fields[key] = value
    return fields


def _fences(text: str) -> list[tuple[int, str]]:
    """(char_offset, body) of every fenced code block."""
    out = []
    for match in re.finditer(r"^[ \t]*```[a-z]*\n(.*?)^[ \t]*```", text, re.S | re.M):
        out.append((match.start(), match.group(1)))
    return out


# ---- agent: obey confirm-decision.sh (ab-8bed07a0) --------------------------


def test_helper_free_lane_auto_skips_the_confirm():
    fields = _run_confirm_helper("--node", "ab-1234", "--provider", "claude")
    assert fields["posture"] == "auto"
    assert fields["confirm_required"] == "0"


def test_helper_cautious_always_optin_confirms():
    fields = _run_confirm_helper("--node", "ab-1234", reader="always")
    assert fields["confirm_required"] == "1"


def test_helper_degraded_read_skips_toward_no_confirm():
    """A failed config read degrades to `auto` (skip + warn), never to `always`."""
    env = dict(os.environ)
    env["DISPATCH_CONFIRM_READER"] = "false"
    proc = subprocess.run(
        ["bash", str(HELPER), "--node", "ab-1234"],
        capture_output=True,
        text=True,
        env=env,
        timeout=30,
    )
    fields = dict(
        (k, v)
        for k, _, v in (line.partition("=") for line in proc.stdout.splitlines())
    )
    assert fields["confirm_required"] == "0"
    assert "unreadable" in fields["warn"]


def test_helper_caveat_warns_without_confirming():
    fields = _run_confirm_helper("--node", "ab-1234", "--yolo", "1")
    assert fields["confirm_required"] == "0"
    assert fields["caveat"] == "1"
    assert fields["warn"]


def test_agent_skill_obeying_helper_is_stated_and_stale_rule_gone():
    text = _skill("skills/agent/SKILL.md")
    hard_rules = text[text.index("Hard rules") :]
    assert "confirm-decision.sh" in hard_rules
    assert "confirm_required" in hard_rules
    assert "always confirms under" not in text
    assert "degrades to `always`" not in text
    assert "degrades to `auto`" in text


def test_agent_skill_preserves_explicit_consent_gates():
    text = _skill("skills/agent/SKILL.md")
    assert "confirm even the free lane" in text
    assert "always confirms (destructive)" in text


# ---- triage: rank mutations only after the approval boundary (AC1-EDGE) ------


def test_triage_mutations_sit_after_the_approval_boundary():
    text = _skill("skills/triage/SKILL.md")
    boundary = text.index("Present to user")
    mutations = ("fno backlog update", "triage apply")
    before = [body for off, body in _fences(text) if off < boundary]
    after = [body for off, body in _fences(text) if off > boundary]
    for fence in before:
        for command in mutations:
            assert command not in fence, f"{command} runs before approval"
    assert any("triage apply" in fence for fence in after), (
        "positive control: the approved apply path must still exist"
    )


def test_triage_tournament_order_becomes_proposal_not_rank_write():
    text = _skill("skills/triage/SKILL.md")
    section = text[
        text.index("Tournament ordering") : text.index("**Validate.**")
    ]
    assert "priority_changes" in section
    assert "fno backlog update <id>" not in section  # no invocation, prose may name it
    assert "fno backlog rank" in section  # the operator-only pin stays named


# ---- mail: reply form matches the shipped parser; full session ids ----------


def test_mail_reply_contract_is_the_parser_contract():
    text = _skill("skills/mail/SKILL.md")
    assert "Do not pass a body as a positional to `reply`" not in text
    assert text.count("exactly one of the three") >= 1
    reply = text[text.index("## `reply") : text.index("## `unread")]
    assert "--body-file" in reply


def test_mail_addresses_peers_by_full_session_id():
    text = _skill("skills/mail/SKILL.md")
    assert "normally the same bare 8-hex short-id" not in text
    assert "full session id" in text
    assert "from_session" in text


# ---- setup: schema-owned questions; ask only missing consequential choices ---


def test_setup_promise_is_scoped_to_named_exceptions():
    text = _skill("skills/setup/SKILL.md")
    assert "Do not invent extra questions" not in text
    assert "review gate" in text
    assert "workspace topology" in text


def test_setup_skips_reconfirmed_update_and_reads_existing_values():
    text = _skill("skills/setup/SKILL.md")
    assert "already requested" in text
    assert "current value" in text


# ---- law: supersession prose states the CLI rule once -----------------------


def test_law_supersession_rule_matches_the_cli_guard():
    text = _skill("skills/law/SKILL.md")
    assert "can supersede another `chat_attested` row" in text
    assert "cannot supersede an `operator` row" in text
    assert "Every live-state reader then stops seeing the operator's law" not in text
