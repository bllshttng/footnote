"""Behavioral contract: workflow skills follow the authority their helpers enforce.

Each skill in this file once restated a policy its owning implementation had
replaced, so an LLM following the prose could re-run a mutation the helper
refuses or gate a launch the helper frees. These tests assert the skill names
the receipt fields and verbs it obeys; skills/agent/tests/test_confirm.sh runs
the confirm helper itself. Positive markers first - a stale prose absence alone
proves nothing about what replaced it.
"""
from __future__ import annotations

import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]


def _skill(rel: str) -> str:
    return (REPO_ROOT / rel).read_text(encoding="utf-8")


def _fences(text: str) -> list[tuple[int, str]]:
    """(char_offset, body) of every fenced code block."""
    out = []
    for match in re.finditer(r"^[ \t]*```[a-z]*\n(.*?)^[ \t]*```", text, re.S | re.M):
        out.append((match.start(), match.group(1)))
    return out


# ---- agent: obey confirm-decision.sh (ab-8bed07a0) --------------------------


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


# ---- mail: reply form matches the shipped parser -------------------------


def test_mail_reply_contract_is_the_parser_contract():
    text = _skill("skills/mail/SKILL.md")
    assert "Do not pass a body as a positional to `reply`" not in text
    assert text.count("exactly one of the three") >= 1
    reply = text[text.index("## `reply") : text.index("## `unread")]
    assert "--body-file" in reply


# ---- dnd: the operator door arms the wall clock ------------------------------


def test_dnd_door_arms_the_wall_clock_and_mail_delegates():
    from typer.testing import CliRunner

    from fno.mail.cli import mail_app

    result = CliRunner().invoke(mail_app, ["hold", "--help"])
    assert result.exit_code == 0, result.output
    output = re.sub(r"\x1b\[[0-9;]*m", "", result.output)
    output = re.sub(r"[│┃╭╮╰╯─━]+", " ", output)
    output = " ".join(output.split())
    assert "--for" in output
    assert "The deadline never moves" in output

    dnd = _skill("skills/dnd/SKILL.md")
    assert "name: dnd" in dnd
    assert "fno agents mail hold --for <N>" in dnd
    assert "fno agents mail hold --for 20" in dnd

    mail = _skill("skills/mail/SKILL.md")
    assert "/fno:dnd" in mail
    assert "hold --minutes <N>" not in mail


# ---- law: supersession prose states the CLI rule once -----------------------


def test_law_supersession_rule_matches_the_cli_guard():
    text = _skill("skills/law/SKILL.md")
    assert "can supersede another `chat_attested` row" in text
    assert "cannot supersede an `operator` row" in text
    assert "Every live-state reader then stops seeing the operator's law" not in text


# ---- wave 2: harness and terminal routes -------------------------------------


def test_term_arms_the_resolved_beat_before_arming():
    text = _skill("skills/lead/SKILL.md")
    arm = text[text.index("## Arm the beat") :]
    assert "fno-agents harness-beat" in arm  # the capability row decides, not prose
    assert "never branch on the harness name" in arm
    assert "lead_settle" in arm  # the daemon settle mail pushes on every harness
    assert "arms no watch" in arm  # the lead arms nothing; the daemon mails
    assert "use provider-backed goal actions" in arm
    assert "positive `provider_goal` receipt" in arm
    assert "separate positive `stop` receipt" in arm
    assert "Every goal wake runs the check-in body" in arm


def test_lead_checkin_binds_worker_age_to_the_top_json_payload():
    # x-bd3f: the check-in required a live-worker count and an oldest-worker
    # stamp while naming no field a machine payload actually carries, so a lead
    # following the skill reported both lines as unmeasurable while live workers
    # ran. The contract is the top view's served payload, and an unreadable
    # instrument prints a refusal, never a default that reads healthy.
    text = _skill("skills/lead/SKILL.md")
    checkin = text[text.index("## The check-in body") :]
    checkin = checkin[: checkin.index("## Recording a ruling")]
    assert "fno agents top --json" in checkin
    assert "status_age_s" in checkin
    assert "predicate" in checkin
    assert "worker activity unmeasured" in checkin
    assert "oldest worker last-seen stamp" not in checkin


def test_lead_checkin_names_the_merge_finish_line():
    # The check-in printed open-PR indicators and a lever list that never
    # merged, so a lead following the skill read a ready PR as report-only and
    # escalated it instead of merging. The merge law makes the team the
    # merger, so the skill must carry the verb, the gate, and the merge rule
    # together: a lever the file cannot name is a law rediscovered by
    # exhaustion.
    text = _skill("skills/lead/SKILL.md")
    checkin = text[text.index("## The check-in body") :]
    checkin = checkin[: checkin.index("## Recording a ruling")]
    assert "fno do pr status" in checkin
    assert "fno do pr merge" in checkin
    assert "the team merges green" in checkin


def test_term_finding_starts_a_new_epic():
    # Law d-08ef1f90: new findings go into a new small epic, not a running
    # one. The old lever told a lead to parent a finding into an active
    # mission scope, which grew running epics without bound.
    text = _skill("skills/lead/SKILL.md")
    assert "### A finding starts a new epic" in text
    assert "--type epic" in text
    assert "--parent null" in text
    assert "fno agents org promote <handle> --scope" in text
    assert "inside an active mission scope" not in text
    once = _skill("skills/lead/references/once.md")
    assert "../SKILL.md#a-finding-starts-a-new-epic" in once


def test_review_empty_diff_guard_resolves_the_named_target():
    text = _skill("skills/review/SKILL.md")
    guard = text[text.index("### 2a. Empty-diff guard") :]
    guard = guard[: guard.index("### 2b")]
    assert "REVIEW_TARGET" in text[text.index("## Step 1") : text.index("## Step 2")]
    assert 'if [ -z "$REVIEW_TARGET" ]' in guard
    assert 'git log "$BASE".."$REVIEW_TARGET"' in guard


def test_execute_repairs_in_scope_failures_within_the_bound():
    text = _skill("skills/execute/references/flat.md")
    assert "gets REPAIRED" in text
    assert "iteration bound" in text
    assert "neither substitutes for the configured review count" in text
    assert "If any verification fails → stop and report what failed" not in text


def test_lead_rule_and_exit_name_the_lead_channel_not_decide():
    text = _skill("skills/lead/SKILL.md")
    rule = text[text.index("## Recording a ruling") :]
    assert "fno backlog note <node> <text>" in rule
    assert "--authority role" in rule
    assert "escalations directory" in rule
    assert "fno inbox decisions <subject> --lane law" in rule
    once = _skill("skills/lead/references/once.md")
    exit_section = once[once.index("Before you step_down") :]
    assert "../SKILL.md#recording-a-ruling" in exit_section
    assert "fno agents org done" in exit_section


def test_lead_mailbox_addresses_full_session_ids():
    text = _skill("skills/lead/references/once.md")
    assert "the bare 8-hex session prefix, the same id" not in text
    assert "FULL session id" in text
    assert "refuses an ambiguous short form" in text


# ---- wave 3: conditional recipes behind triggers, concrete drift --------------


def test_agent_and_lead_roots_route_to_workflow_routes_references():
    for skill, trigger in (
        ("skills/agent/SKILL.md", "workflow-routes.md"),
        ("skills/lead/SKILL.md", "workflow-routes.md"),
    ):
        text = _skill(skill)
        assert trigger in text
        ref = REPO_ROOT / skill.rsplit("/", 1)[0] / "references" / trigger
        assert ref.exists(), ref
