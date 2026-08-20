"""The PR-creation path writes the exact `Backlog-Closure:` trailer (x-49ec).

The generator and the checker both already existed; nothing ran the generator
before the checker saw the body, so every PR opened by a bare `gh pr create`
red on scripts/ci/check-pr-node-closure.sh. These tests pin the producer, its
two Python call sites, and the stdlib-only hook copy that covers the prose path.

The specimen refs are real: PR 981 shipped from `x-4271-x-5a83-pr-status-tally`
(two ids, `--extra` needed by hand) and PR 971 from `feature/x-76d1`.
"""
from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from fno.pr.closure import (
    branch_node_ids,
    ensure_closure_trailer,
    parse_closure_trailer,
)

REPO = Path(__file__).resolve().parents[3]
GATE = REPO / "scripts" / "ci" / "check-pr-node-closure.sh"
HOOK = REPO / "hooks" / "git-protection.py"


def _load_hook():
    spec = importlib.util.spec_from_file_location("_git_protection_x49ec", HOOK)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _gate(body: str, head_ref: str) -> int:
    """The real CI gate's exit code for this (body, ref) pair."""
    return subprocess.run(
        ["bash", str(GATE)],
        env={"PATH": "/usr/bin:/bin", "PR_BODY": body, "PR_HEAD_REF": head_ref},
        capture_output=True,
        text=True,
    ).returncode


# ---- branch_node_ids: the same candidates the gate extracts ----

@pytest.mark.parametrize(
    "head_ref, expected",
    [
        ("feature/x-76d1", ["x-76d1"]),                          # PR 971
        ("x-4271-x-5a83-pr-status-tally", ["x-4271", "x-5a83"]),  # PR 981
        ("feature/x-49ec", ["x-49ec"]),
        ("target/some-slug-ab-55ba9adb", ["ab-55ba9adb"]),
        ("chore/tidy-docs", []),
        ("", []),
    ],
)
def test_branch_node_ids_extracts_gate_candidates(head_ref, expected):
    assert branch_node_ids(head_ref) == expected


def test_branch_node_ids_never_invents_a_glued_second_id():
    # The gate skips BOTH consumed segments because an all-hex suffix is itself
    # a valid prefix shape; re-gluing would demand a "cdef-1234" that names
    # nothing. Non-overlapping scanning gives the producer the same answer.
    assert branch_node_ids("feature/x-cdef-1234") == ["x-cdef"]


def test_branch_node_ids_rejects_a_bare_substring():
    # x-5b66 is a prefix of x-5b667; an unbounded match would claim the wrong node.
    assert branch_node_ids("feature/x-5b667") == ["x-5b667"]


# ---- ensure_closure_trailer ----

def test_appends_the_trailer_when_the_body_has_none():
    out = ensure_closure_trailer("Some summary.", "x-4271-x-5a83-pr-status-tally")
    assert parse_closure_trailer(out) == ["x-4271", "x-5a83"]
    assert out.startswith("Some summary.")


def test_is_a_noop_on_a_compliant_body():
    body = "Summary.\n\nBacklog-Closure: x-76d1\n"
    assert ensure_closure_trailer(body, "feature/x-76d1") == body


def test_is_a_noop_on_a_branch_naming_no_node():
    body = "Summary."
    assert ensure_closure_trailer(body, "chore/tidy-docs") == body


def test_is_idempotent():
    once = ensure_closure_trailer("Summary.", "feature/x-49ec")
    assert ensure_closure_trailer(once, "feature/x-49ec") == once


def test_keeps_existing_claims_when_adding_a_missing_one():
    body = "Summary.\n\nBacklog-Closure: x-1111\n"
    out = ensure_closure_trailer(body, "feature/x-49ec")
    assert parse_closure_trailer(out) == ["x-1111", "x-49ec"]


def test_extra_ids_join_the_claim():
    out = ensure_closure_trailer("Summary.", "batch/code", extra_ids=["x-aaaa", "x-bbbb"])
    assert parse_closure_trailer(out) == ["x-aaaa", "x-bbbb"]


def test_malformed_extra_ids_are_dropped():
    out = ensure_closure_trailer("Summary.", "feature/x-49ec", extra_ids=["NOT-AN-ID"])
    assert parse_closure_trailer(out) == ["x-49ec"]


def test_an_empty_body_still_gets_a_parseable_trailer():
    assert parse_closure_trailer(ensure_closure_trailer("", "feature/x-49ec")) == ["x-49ec"]


# ---- Parity with the real gate, both directions ----

@pytest.mark.parametrize(
    "head_ref", ["feature/x-76d1", "x-4271-x-5a83-pr-status-tally", "feature/x-49ec"]
)
def test_produced_body_passes_the_real_gate(head_ref):
    body = "Summary of the change."
    # Positive control on the instrument: the untreated body must FAIL, so a
    # green on the treated body is the producer working and not the gate
    # skipping this ref.
    assert _gate(body, head_ref) == 1
    assert _gate(ensure_closure_trailer(body, head_ref), head_ref) == 0


# ---- Call site: fno worker ship ----

def test_worker_ship_passes_the_trailer_to_gh(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    state_dir = tmp_path / ".fno"
    state_dir.mkdir(parents=True)
    (state_dir / "target-state.md").write_text(
        "---\nstatus: IN_PROGRESS\nsession_id: 20260819T000000Z-1-aabbcc\n"
        "artifact_shipped: false\nauto_merge_approved: false\npr_number: null\n---\n"
    )

    mock_run = MagicMock()
    mock_run.side_effect = [
        MagicMock(returncode=0, stdout="feature/x-49ec\n", stderr=""),   # git rev-parse
        MagicMock(returncode=0, stdout="[]", stderr=""),                 # gh pr list
        MagicMock(returncode=0, stdout="https://x/pull/42", stderr=""),  # gh pr create
    ]
    with patch("subprocess.run", mock_run), patch(
        "fno.pr._preflight.check_stale_base", return_value=(0, None)
    ), patch("fno.pr._preflight.local_verification_required", lambda **_k: (False, "")):
        from fno.worker.ship import ship

        ship(
            state_path=state_dir / "target-state.md",
            title="feat: x",
            body="Auto-generated PR body",
            artifacts_dir=tmp_path / ".fno" / "artifacts",
        )

    argv = mock_run.call_args_list[2][0][0]
    sent_body = argv[argv.index("--body") + 1]
    assert parse_closure_trailer(sent_body) == ["x-49ec"]


# ---- Call site: the batch lane ----

def test_batch_ship_claims_every_member(monkeypatch):
    from fno.backlog import batch as batch_mod

    calls = []

    def fake_run(argv, cwd=None, **_kwargs):
        calls.append(argv)
        if argv[:3] == ["gh", "pr", "list"]:
            return subprocess.CompletedProcess(argv, 0, "[]", "")
        return subprocess.CompletedProcess(argv, 0, "https://x/pull/7", "")

    monkeypatch.setattr(
        batch_mod, "read_batch", lambda *_a, **_k: {
            "status": "open", "worktree": "/tmp/wt", "branch": "batch/code",
            "members": ["x-aaaa", "x-bbbb"],
        }
    )
    monkeypatch.setattr(batch_mod, "member_ids", lambda _b: ["x-aaaa", "x-bbbb"])
    monkeypatch.setattr(batch_mod, "_batch_pr_body", lambda _b: "Batch body.")
    monkeypatch.setattr(batch_mod, "_set_member_pr_refs", lambda *_a, **_k: None)
    monkeypatch.setattr(batch_mod, "close_batch", lambda **_k: None)
    monkeypatch.setattr(
        "fno.pr._preflight.local_verification_required", lambda **_k: (False, "")
    )
    monkeypatch.setattr("fno.pr._preflight.check_stale_base", lambda **_k: (0, None))

    batch_mod.ship_batch(domain="code", root=Path("/tmp"), run=fake_run)

    create = next(a for a in calls if a[:3] == ["gh", "pr", "create"])
    sent_body = create[create.index("--body") + 1]
    assert parse_closure_trailer(sent_body) == ["x-aaaa", "x-bbbb"]


# ---- The stdlib-only hook copy ----

@pytest.mark.parametrize(
    "head_ref",
    [
        "feature/x-76d1", "x-4271-x-5a83-pr-status-tally", "feature/x-cdef-1234",
        "target/some-slug-ab-55ba9adb", "chore/tidy-docs", "feature/x-5b667", "",
    ],
)
def test_hook_node_id_extraction_matches_the_python_authority(head_ref):
    # The hook keeps its own regex because it runs stdlib-only under a bare
    # interpreter. This pins the two so a change to one is a failing test, not
    # a silent divergence between the local guard and the producer.
    assert _load_hook()._branch_node_ids(head_ref) == branch_node_ids(head_ref)


def _hook_decision(command, branch, env=None, monkeypatch=None):
    hook = _load_hook()
    monkeypatch.setattr(hook, "get_current_branch", lambda: branch)
    for key, value in (env or {}).items():
        monkeypatch.setenv(key, value)
    return hook._closure_trailer_refusal(command)


def test_hook_denies_a_bare_create_from_a_node_branch(monkeypatch):
    reason = _hook_decision(
        'gh pr create --title t --body "$BODY"', "feature/x-49ec", monkeypatch=monkeypatch
    )
    assert reason is not None
    assert "Backlog-Closure: x-49ec" in reason
    assert "fno pr closure-trailer x-49ec" in reason


def test_hook_allows_a_non_node_branch(monkeypatch):
    assert _hook_decision(
        'gh pr create --title t --body "$BODY"', "chore/tidy-docs", monkeypatch=monkeypatch
    ) is None


def test_hook_allows_when_the_composition_step_is_visible(monkeypatch):
    assert _hook_decision(
        'gh pr create --title t --body "${BODY}\n${CLOSURE_TRAILER}"',
        "feature/x-49ec",
        monkeypatch=monkeypatch,
    ) is None


def test_hook_allows_a_literal_trailer_in_the_command(monkeypatch):
    assert _hook_decision(
        'gh pr create --title t --body "s\n\nBacklog-Closure: x-49ec"',
        "feature/x-49ec",
        monkeypatch=monkeypatch,
    ) is None


def test_hook_reads_a_body_file_and_allows_a_real_trailer(monkeypatch, tmp_path):
    # A --body-file names a real path, so this spelling is judged on the file
    # rather than on a marker. Denying it would be the false DENY the docstring
    # says cannot happen.
    body = tmp_path / "pr-body.md"
    body.write_text(ensure_closure_trailer("Summary.", "feature/x-49ec"))
    assert _hook_decision(
        f"gh pr create --title t --body-file {body}", "feature/x-49ec",
        monkeypatch=monkeypatch,
    ) is None


def test_hook_denies_a_body_file_missing_the_claim(monkeypatch, tmp_path):
    body = tmp_path / "pr-body.md"
    body.write_text("Summary.\n\nBacklog-Closure: x-1111\n")
    reason = _hook_decision(
        f"gh pr create --title t --body-file {body}", "feature/x-49ec",
        monkeypatch=monkeypatch,
    )
    assert reason is not None and "Backlog-Closure: x-49ec" in reason


def test_hook_denies_an_unreadable_body_file(monkeypatch, tmp_path):
    assert _hook_decision(
        f"gh pr create --title t --body-file {tmp_path / 'gone.md'}", "feature/x-49ec",
        monkeypatch=monkeypatch,
    ) is not None


def test_hook_escape_hatch_clears_the_gate(monkeypatch):
    assert _hook_decision(
        'gh pr create --title t --body "$BODY"',
        "feature/x-49ec",
        env={"FNO_PR_CLOSURE_OK": "1"},
        monkeypatch=monkeypatch,
    ) is None


def _hook_main(command, cwd):
    result = subprocess.run(
        [sys.executable, str(HOOK)],
        input=json.dumps({"tool_name": "Bash", "tool_input": {"command": command}}),
        capture_output=True, text=True, cwd=str(cwd),
    )
    return json.loads(result.stdout)["hookSpecificOutput"] if result.stdout.strip() else None


@pytest.fixture
def node_branch_repo(tmp_path):
    """A throwaway repo checked out on a node-bearing branch.

    The hook reads the CURRENT branch, so anchoring this to the test runner's
    own checkout would pass here and silently skip in CI, where a PR build sits
    on a detached HEAD and `git symbolic-ref` fails.
    """
    for argv in (
        ["git", "init", "-q", "-b", "feature/x-49ec"],
        ["git", "config", "user.email", "t@example.com"],
        ["git", "config", "user.name", "t"],
        ["git", "commit", "-q", "--allow-empty", "-m", "init"],
    ):
        subprocess.run(argv, cwd=tmp_path, check=True, capture_output=True)
    return tmp_path


def test_hook_denies_the_full_pretooluse_payload(node_branch_repo):
    # End to end through main(), not just the predicate: a helper that returns a
    # reason nothing calls is the decorative-guard shape this node exists to fix.
    payload = _hook_main('gh pr create --title t --body "$BODY"', node_branch_repo)
    assert payload["permissionDecision"] == "deny"
    assert "Backlog-Closure: x-49ec" in payload["permissionDecisionReason"]


def test_hook_main_allows_a_composed_body(node_branch_repo):
    payload = _hook_main(
        'gh pr create --title t --body "${BODY}\n${CLOSURE_TRAILER}"', node_branch_repo
    )
    assert payload is None or payload["permissionDecision"] != "deny"


def test_hook_main_gates_gh_behind_a_repo_flag(node_branch_repo):
    # `gh --repo o/r pr create` is the same operation; the global-option strip
    # is what keeps this from being a one-spelling guard.
    payload = _hook_main('gh --repo o/r pr create --body "$BODY"', node_branch_repo)
    assert payload["permissionDecision"] == "deny"
