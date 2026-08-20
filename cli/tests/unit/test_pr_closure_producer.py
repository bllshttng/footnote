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


@pytest.mark.parametrize(
    "head_ref", ["feat/cafe", "fix/abc123", "target/deadbeef", "chore/fee1dead"]
)
def test_the_gate_never_demands_an_id_spanning_a_slash(head_ref):
    # The producer requires a literal '-' between prefix and hex, so it writes
    # no trailer for these refs. The gate used to re-glue across '/' and demand
    # "feat-cafe" / "target-deadbeef" - reddening a PR over a line nothing on
    # the producer side could generate, which is the whole defect this pair
    # exists to close.
    assert branch_node_ids(head_ref) == []
    assert _gate(ensure_closure_trailer("Summary.", head_ref), head_ref) == 0


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

    # The branch is the shape ship_batch actually produces. Patching it to a
    # tidier "batch/code" pinned the tag and not the destination: the real
    # "feature/batch-code-a1b2c3" parsed as node-bearing, so the trailer
    # carried a "code-a1b2c3" the graph does not carry, and
    # bind_closure_claims voided every genuine member claim beside it.
    monkeypatch.setattr(
        batch_mod, "read_batch", lambda *_a, **_k: {
            "status": "open", "worktree": "/tmp/wt", "branch": "feature/batch-code.a1b2c3",
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


@pytest.mark.parametrize("domain", ["code", "docs", "front-end"])
def test_a_batch_branch_never_parses_as_node_bearing(domain):
    # <word>-<hex> IS the node-id grammar, so every batch-code-a1b2c3 branch
    # read as node-bearing. The gate then demanded an id the graph does not
    # carry, and one unknown id refuses the WHOLE binding.
    ref = f"feature/batch-{domain}.a1b2c3"
    assert branch_node_ids(ref) == []
    assert _gate(ensure_closure_trailer("Batch body.", ref), ref) == 0


def test_hook_docstrings_do_not_claim_creation_is_ungated():
    # A PR must not ship a doc its own code disproves.
    text = HOOK.read_text(encoding="utf-8")
    assert "it's always allowed" not in text


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


# The hatch is judged by POSITION, so every case here goes through main() with
# a real segment list. Calling the predicate directly cannot test position.

def test_hook_honors_an_inline_hatch_prefixing_the_gh_call(node_branch_repo):
    # A PreToolUse hook is a separate process, so an inline assignment never
    # reaches its os.environ. Reading only the env made the documented hatch a
    # dead end, and the refusal message taught that spelling.
    payload = _hook_main(
        'FNO_PR_CLOSURE_OK=1 gh pr create --title t --body "$BODY"', node_branch_repo
    )
    assert payload is None or payload["permissionDecision"] != "deny"


@pytest.mark.parametrize(
    "command",
    [
        # In a commit message, in a compound command.
        'git commit -m "note: FNO_PR_CLOSURE_OK=1 was needed" && gh pr create --body "$B"',
        # In the PR body itself.
        'gh pr create --body "we set FNO_PR_CLOSURE_OK=1 here"',
        # Echoed before the call. This one already denied before the position
        # fix, but for the WRONG reason: the quote characters broke a
        # whitespace boundary. A test pinning a right answer for a wrong reason
        # rots the moment the quoting changes, so it is pinned here on position.
        'echo "FNO_PR_CLOSURE_OK=1" ; gh pr create --body "$B"',
    ],
)
def test_hook_ignores_a_hatch_that_is_not_an_assignment_prefix(command, node_branch_repo):
    # Position, never presence. A whole-command substring search read the
    # string out of any quoted argument and opened the gate - the same
    # marker-substring false allow this guard exists to close, reintroduced by
    # its own fix.
    payload = _hook_main(command, node_branch_repo)
    assert payload["permissionDecision"] == "deny"


@pytest.mark.parametrize(
    "command",
    [
        # A --body that merely QUOTES the words "--head <ref>" retargeted the
        # gate at a ref this PR does not ship.
        'gh pr create --body "see --head chore/tidy-docs for context"',
        # A --body naming a --body-file path in prose satisfied the file read.
        'gh pr create --body "pass --body-file .fno/pr-body.md next time"',
    ],
)
def test_hook_ignores_flags_quoted_inside_an_argument(command, node_branch_repo):
    # Same defect as the hatch, on the two neighbouring flags. Fixing one of
    # three instances is the decorative guard this whole node exists to close:
    # a reader sees a position check on the hatch and assumes its neighbours
    # got the same treatment. shlex collapses a quoted argument into ONE token,
    # so argv position is what separates a flag from a word.
    payload = _hook_main(command, node_branch_repo)
    assert payload["permissionDecision"] == "deny"


# --head and --body-file are POSITION-read from the segment's argv, so every
# case for them runs through main() with a real segment list.

def test_hook_judges_the_explicit_head_ref_not_the_checkout(node_branch_repo):
    # The PR closes what its HEAD ref names, which is what the CI gate reads.
    payload = _hook_main(
        'gh pr create --head chore/tidy-docs --body "$BODY"', node_branch_repo
    )
    assert payload is None or payload["permissionDecision"] != "deny"


def test_hook_reads_the_short_head_flag(node_branch_repo):
    # `-H` is gh's short --head, the same one-spelling gap the -F fix closed.
    payload = _hook_main(
        'gh pr create -H chore/tidy-docs --body "$BODY"', node_branch_repo
    )
    assert payload is None or payload["permissionDecision"] != "deny"


def test_hook_still_denies_a_node_bearing_explicit_head(node_branch_repo):
    payload = _hook_main(
        'gh pr create --head feature/x-1111 --body "$BODY"', node_branch_repo
    )
    assert payload["permissionDecision"] == "deny"
    assert "Backlog-Closure: x-1111" in payload["permissionDecisionReason"]


@pytest.mark.parametrize("flag", ["--body-file", "-F"])
def test_hook_reads_a_body_file_under_either_spelling(flag, node_branch_repo, tmp_path):
    body = tmp_path / "pr-body.md"
    body.write_text(ensure_closure_trailer("Summary.", "feature/x-49ec"))
    payload = _hook_main(f"gh pr create --title t {flag} {body}", node_branch_repo)
    assert payload is None or payload["permissionDecision"] != "deny"


def test_hook_denies_a_body_file_missing_the_claim(node_branch_repo, tmp_path):
    body = tmp_path / "pr-body.md"
    body.write_text("Summary.\n\nBacklog-Closure: x-1111\n")
    payload = _hook_main(f"gh pr create --title t --body-file {body}", node_branch_repo)
    assert payload["permissionDecision"] == "deny"
    assert "Backlog-Closure: x-49ec" in payload["permissionDecisionReason"]


def test_hook_allows_a_body_file_the_same_command_is_about_to_write(node_branch_repo, tmp_path):
    # A PreToolUse hook runs BEFORE the command, so the documented spelling
    # names a path that does not exist yet. Denying it is the false DENY the
    # docstring rules out.
    target = tmp_path / "not-yet.md"
    payload = _hook_main(
        f"printf '%s' \"$B\" > {target}\ngh pr create --title t --body-file {target}",
        node_branch_repo,
    )
    assert payload is None or payload["permissionDecision"] != "deny"


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
