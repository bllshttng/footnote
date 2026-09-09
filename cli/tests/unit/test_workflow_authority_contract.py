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


# ---- wave 2: harness and terminal routes -------------------------------------


def test_reign_branches_on_harness_capability_before_arming():
    text = _skill("skills/reign/SKILL.md")
    arm = text[text.index("## Arm the beat") :]
    assert "Branch once on what the harness supports" in arm
    assert "On Claude, arm six monitors" in arm
    assert "arm nothing native" in arm
    assert "wake arm" in arm  # the codex beat is the external wake contract


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


def test_execute_kill_criteria_reads_frontmatter_owner():
    text = _skill("skills/execute/references/flat.md")
    assert "kill_criteria:" in text
    assert "frontmatter" in text
    assert "`## Kill Criteria` fenced YAML block" not in text


def test_review_lanes_names_retired_spawned_reviewer_law_not_the_recipe():
    text = _skill("docs/architecture/review-lanes.md")
    assert "d-384d967c" in text
    assert "No spawned-reviewer lane" in text
    assert "--model opus" not in text
    assert "the peer lane" in text
    assert "NO `--fix` remains the review contract" in text


def test_king_rule_and_exit_name_the_king_channel_not_decide():
    text = _skill("skills/king-for-a-day/SKILL.md")
    rule = text[text.index("**Rule.**") :]
    assert "fno backlog note <node> <text>" in rule
    assert "--authority" in rule
    exit_section = text[text.index("Before you abdicate") :]
    assert "fno backlog note" in exit_section
    assert "refuses every agent session" in exit_section


def test_king_mailbox_addresses_full_session_ids():
    text = _skill("skills/king-for-a-day/SKILL.md")
    assert "the bare 8-hex session prefix, the same id" not in text
    assert "FULL session id" in text
    assert "refuses an ambiguous short form" in text


def test_speculate_ends_at_comparison_and_binds_workers_to_real_worktrees():
    text = _skill("skills/speculate/SKILL.md")
    assert 'model="sonnet"' not in text
    assert 'isolation="worktree"' not in text
    assert "absolute worktree path from Step 3" in text
    assert "separately explicit action" in text
    assert "never selects or merges a winner on its own" in text


def test_audit_deliverable_is_bounded_artifact_plans_only_when_authorized():
    text = _skill("skills/audit/SKILL.md")
    assert "only on an authorized run" in text
    assert "until ALL features are planned" not in text
    assert "Linear" not in text
    assert "the resolved `--perspectives` set" in text


def test_ship_and_using_fno_delegate_worker_choice_to_configured_routing():
    ship = _skill("skills/ship/SKILL.md")
    assert "Haiku-capable provider" not in ship
    assert "configured role routing" in ship
    using = _skill("skills/using-fno/SKILL.md")
    assert "Haiku worker" not in using
    assert "routed create worker" in using
    assert "a skill spawns a new agent context" not in using


# ---- wave 3: conditional recipes behind triggers, concrete drift --------------


def test_agent_and_king_roots_route_to_workflow_routes_references():
    for skill, trigger in (
        ("skills/agent/SKILL.md", "workflow-routes.md"),
        ("skills/king-for-a-day/SKILL.md", "workflow-routes.md"),
    ):
        text = _skill(skill)
        assert trigger in text
        ref = REPO_ROOT / skill.rsplit("/", 1)[0] / "references" / trigger
        assert ref.exists(), ref


def test_fix_root_is_contract_and_body_lives_behind_mode():
    root = _skill("skills/fix/SKILL.md")
    assert "references/fix-loop.md" in root
    assert "Ask only unresolved consequential choices" in root
    assert "fix_score" not in root  # the unexplained weighted score moved out
    body = _skill("skills/fix/references/fix-loop.md")
    assert "fix_score" in body  # and now states what it is for
    assert "Anti-Pattern Blocklist" in body


def test_tdd_recovery_rule_preserves_unrelated_work():
    text = _skill("skills/tdd/SKILL.md")
    assert "keep everything else" in text
    assert "PROVES the behavior is missing" in text
    examples = _skill("skills/tdd/references/examples.md")
    assert "ratio-compliance" in examples


def test_cache_keepalive_reports_unverified_not_assumed_warmth():
    text = _skill("skills/cache-keepalive/SKILL.md")
    assert "Capability gate" in text
    assert "schedule unavailable" in text
    assert "warmth unverified" in text.lower() or "Warmth is measured" in text
    assert "use ONLY these values" not in text
    assert "$0.52" not in text
    assert "pricing unverified" in text


def test_autocorrect_cadence_matches_the_scheduler_once():
    text = _skill("skills/autocorrect/SKILL.md")
    assert "quarterly" not in text
    assert "--severity S1,S2" in text
    assert "Harness boundary" in text


def test_growth_launch_byline_resolved_once_and_inline_fallback_is_bounded():
    text = _skill("skills/growth-launch/SKILL.md")
    assert "never" in text and "internal artifacts" in text
    assert "every published text draft" in text
    assert "the draft-only boundary is the tool list" in text


def test_diet_doc_measures_payload_bytes():
    text = _skill("docs/architecture/skill-body-diet.md")
    assert "UTF-8 payload byte count" in text
    assert "selected-reference payload" in text
    assert "wc -l skills/{name}" not in text


def test_how_to_guides_has_no_retired_config_gate():
    text = _skill("skills/ship-docs/references/how-to-guides.md")
    assert "config.docs" not in text
    assert "/setup --full" not in text


# ---- wave 4: seeded cases grade the graders (AC4-HP) -------------------------
# The graders above pass on the real files; these cases prove they DISCRIMINATE.
# A prohibited action or a false completion, seeded as a fragment, must fail the
# same grading logic the real files pass; the authorized twin must pass it.


def _grade_confirm_prose(text: str) -> bool:
    """Helper-based confirmation: the skill obeys confirm_required, keeps the
    always opt-in, and never restates caveat-confirms or degrade-to-always."""
    hard = text[text.index("Hard rules") :]
    return (
        "confirm-decision.sh" in hard
        and "confirm_required" in hard
        and "always confirms under" not in text
        and "degrades to `always`" not in text
        and "confirm even the free lane" in text
    )


def _grade_triage_fences(text: str) -> bool:
    """Pre-approval triage: no mutating fence before the approval boundary,
    and the approved apply path still exists after it."""
    boundary = text.index("Present to user")
    before = [b for off, b in _fences(text) if off < boundary]
    after = [b for off, b in _fences(text) if off > boundary]
    return not any(
        c in b for b in before for c in ("fno backlog update", "triage apply")
    ) and any("triage apply" in b for b in after)


def _grade_review_guard(text: str) -> bool:
    """Explicit PR targeting: the guard branches on the resolved target."""
    return 'if [ -z "$REVIEW_TARGET" ]' in text and "gh pr view" in text


def _grade_reign_arm(text: str) -> bool:
    """Supported harness wake: one capability branch; codex arms nothing native."""
    arm = text[text.index("## Arm the beat") :]
    return "Branch once on what the harness supports" in arm and "arm nothing native" in arm


def _grade_flat_verify(text: str) -> bool:
    """Failed verification repaired in scope; CI green never substitutes."""
    return (
        "REPAIR it and re-run" in text
        and "neither substitutes for the configured review count" in text
    )


def test_seeded_prohibited_cases_fail_the_graders():
    bad_confirm = "# x\n\nHard rules (non-negotiable)\n\na caveat always confirms under `auto`; a failed read degrades to `always`.\n"
    assert not _grade_confirm_prose(bad_confirm)
    bad_triage = (
        "reason first.\n\n```bash\nfno backlog update ab-1 --priority p1\n```\n\n"
        "then Present to user via AskUserQuestion.\n"
    )
    assert not _grade_triage_fences(bad_triage)
    assert not _grade_review_guard("bare guard: plain diff check with no target branch")
    assert not _grade_reign_arm("## Arm the beat\n\nArm six Monitors on every harness.")
    assert not _grade_flat_verify("If any verification fails, stop and report what failed.")


def test_seeded_authorized_cases_pass_the_graders():
    good_confirm = (
        "Hard rules (non-negotiable)\n\nObey `confirm-decision.sh` and follow its "
        "`confirm_required` field; `always` is the cautious opt-in: confirm even "
        "the free lane.\n"
    )
    assert _grade_confirm_prose(good_confirm)
    good_triage = (
        "Present to user via AskUserQuestion.\n\n```bash\n"
        "fno backlog triage apply /tmp/triage-cleaned.json\n```\n"
    )
    assert _grade_triage_fences(good_triage)
    assert _grade_review_guard(
        'if [ -z "$REVIEW_TARGET" ]; then gh pr view <n> --json headRefName; fi'
    )
    assert _grade_reign_arm(
        "## Arm the beat\n\nBranch once on what the harness supports.\n"
        "Codex: arm nothing native; the wake arm is the beat.\n"
    )
    assert _grade_flat_verify(
        "REPAIR it and re-run within the bound; neither substitutes for the "
        "configured review count."
    )


def test_eval_bank_declares_the_workflow_authority_regression_grade():
    import yaml

    bank = yaml.safe_load(
        (REPO_ROOT / "evals" / "bank" / "workflow-authority-regression.yaml").read_text()
    )
    assert bank["tier"] == "regression"
    grades = bank["grade"]
    assert grades, "positive control: the bank entry must carry a grade leg"
    commands = " ".join(g["command"] for g in grades)
    assert "test_workflow_authority_contract" in commands
