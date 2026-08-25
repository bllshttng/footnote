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
import re
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

# Every branch-derived claim below is verified against the graph by the
# producer, so these tests name their own universe rather than depending on
# whatever ~/.fno/graph.json happens to hold. Passing an explicit set is also
# what keeps them pure: the default path reads the real graph.
KNOWN = frozenset({"x-4271", "x-5a83", "x-76d1", "x-49ec", "x-1111", "ab-55ba9adb",
                   "x-5b667", "x-cdef"})


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
    out = ensure_closure_trailer("Some summary.", "x-4271-x-5a83-pr-status-tally", known_ids=KNOWN)
    assert parse_closure_trailer(out) == ["x-4271", "x-5a83"]
    assert out.startswith("Some summary.")


def test_is_a_noop_on_a_compliant_body():
    body = "Summary.\n\nBacklog-Closure: x-76d1\n"
    assert ensure_closure_trailer(body, "feature/x-76d1", known_ids=KNOWN) == body


def test_is_a_noop_on_a_branch_naming_no_node():
    body = "Summary."
    assert ensure_closure_trailer(body, "chore/tidy-docs", known_ids=KNOWN) == body


def test_is_idempotent():
    once = ensure_closure_trailer("Summary.", "feature/x-49ec", known_ids=KNOWN)
    assert ensure_closure_trailer(once, "feature/x-49ec", known_ids=KNOWN) == once


def test_keeps_existing_claims_when_adding_a_missing_one():
    body = "Summary.\n\nBacklog-Closure: x-1111\n"
    out = ensure_closure_trailer(body, "feature/x-49ec", known_ids=KNOWN)
    assert parse_closure_trailer(out) == ["x-1111", "x-49ec"]


def test_extra_ids_join_the_claim():
    out = ensure_closure_trailer("Summary.", "batch/code", extra_ids=["x-aaaa", "x-bbbb"], known_ids=KNOWN)
    assert parse_closure_trailer(out) == ["x-aaaa", "x-bbbb"]


def test_malformed_extra_ids_are_dropped():
    out = ensure_closure_trailer("Summary.", "feature/x-49ec", extra_ids=["NOT-AN-ID"], known_ids=KNOWN)
    assert parse_closure_trailer(out) == ["x-49ec"]


def test_an_empty_body_still_gets_a_parseable_trailer():
    assert parse_closure_trailer(ensure_closure_trailer("", "feature/x-49ec", known_ids=KNOWN)) == ["x-49ec"]


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
    assert _gate(ensure_closure_trailer(body, head_ref, known_ids=KNOWN), head_ref) == 0


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
    assert _gate(ensure_closure_trailer("Summary.", head_ref, known_ids=KNOWN), head_ref) == 0


# ---- Call site: fno agents worker ship ----

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
    ), patch("fno.pr._preflight.local_verification_required", lambda **_k: (False, "")), \
            patch("fno.pr.closure.known_node_ids", lambda: KNOWN):
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


def test_worker_ship_reports_incomplete_delivery_when_graph_binding_fails(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    state_dir = tmp_path / ".fno"
    state_dir.mkdir(parents=True)
    (state_dir / "target-state.md").write_text(
        "---\nstatus: IN_PROGRESS\nsession_id: session-1\n"
        "artifact_shipped: false\nauto_merge_approved: false\n---\n"
        "graph_node_id: x-49ec\n"
    )
    pr_url = "https://github.com/o/r/pull/42"
    mock_run = MagicMock()
    mock_run.side_effect = [
        MagicMock(returncode=0, stdout="feature/x-49ec\n", stderr=""),
        MagicMock(returncode=0, stdout="[]", stderr=""),
        MagicMock(returncode=0, stdout=f"{pr_url}\n", stderr=""),
        MagicMock(returncode=1, stdout="", stderr="binding refused"),
    ]
    with patch("subprocess.run", mock_run), patch(
        "fno.pr._preflight.check_stale_base", return_value=(0, None)
    ), patch("fno.pr._preflight.local_verification_required", lambda **_k: (False, "")), \
            patch("fno.pr.closure.known_node_ids", lambda: KNOWN):
        from fno.worker.ship import ship

        result = ship(
            state_path=state_dir / "target-state.md",
            title="feat: x",
            body="Auto-generated PR body",
            artifacts_dir=tmp_path / ".fno" / "artifacts",
        )

    assert result["action"] == "incomplete_delivery"
    assert result["pr_number"] == 42
    assert result["pr_url"] == pr_url
    assert result["binding_error"] == "binding refused"
    assert result["repair_command"] == (
        f"fno do pr bind-created --url {pr_url} --repo {tmp_path} --node x-49ec"
    )


# ---- The canonical create instructions route through bind-created (x-d3c6) ----
#
# The prose lane is the third producer beside worker.ship and the hook: two
# instruction surfaces a human or dispatch worker follows verbatim. A recipe
# that stamps via `backlog update --pr-number` is a SECOND writer that skips
# branch fallback and ship provenance, which is how a created PR ended up
# unbound from its own node three times in one day.

CREATE_REFERENCE = REPO / "skills" / "pr" / "references" / "create.md"
PR_CREATOR = REPO / "skills" / "pr" / "agents" / "pr-creator.md"


def _section_55(path: Path) -> str:
    """Step 5.5 only, from its heading to the next heading."""
    text = path.read_text(encoding="utf-8")
    assert text, f"{path.name} read as empty, so any marker check is vacuous"
    start = text.index("### 5.5 ")
    return text[start : text.index("### 6.", start)]


@pytest.mark.parametrize(
    "surface", [CREATE_REFERENCE, PR_CREATOR], ids=["create-reference", "pr-creator"]
)
def test_pr_creator_step_55_binds_the_created_pr(surface):
    section = _section_55(surface)
    # The shared binder: URL, repo worktree, and the manifest node when the
    # manifest carries one (branch fallback covers the rest).
    assert 'fno do pr bind-created --url "$PR_URL" --repo "$(pwd)"' in section
    assert '--node "$NODE_ID"' in section
    # One writer: the manifest-only stamp recipe is gone from this step, or a
    # create worker following the old line skips branch fallback and ship
    # provenance while still reading "bound" to its dispatcher.
    assert 'fno backlog update "$NODE_ID" --pr-number' not in section


@pytest.mark.parametrize(
    "surface", [CREATE_REFERENCE, PR_CREATOR], ids=["create-reference", "pr-creator"]
)
def test_pr_creator_refusal_names_the_pr_and_one_repair_command(surface):
    section = _section_55(surface)
    # The refusal receipt must name the already-created PR and the exact
    # rerunnable binder command, never report clean delivery for an unbound PR.
    assert "UNBOUND" in section
    assert "repair: ${BIND_ARGS[*]}" in section


@pytest.mark.parametrize(
    "surface", [CREATE_REFERENCE, PR_CREATOR], ids=["create-reference", "pr-creator"]
)
def test_pr_creator_keeps_finalize_as_the_backstop_not_the_path(surface):
    # The finalizer remains the idempotent backstop; the binder above is the
    # primary path. Losing the backstop note invites skipping the fast path.
    assert "fno-agents finalize" in _section_55(surface)


def test_pr_creator_and_create_reference_ship_one_binder_block():
    # The two instruction surfaces must carry the byte-identical step-5.5 bash
    # block, or one can regain the manifest-only recipe while the other is
    # clean - the same divergence failure the OOS contract test pins.
    import re as _re

    def _block(path: Path) -> str:
        return _re.search(r"### 5\.5 .*?```bash\n(.*?)```", _section_55(path), _re.DOTALL).group(1)

    a, b = _block(CREATE_REFERENCE), _block(PR_CREATOR)
    assert a and a == b


def test_a_graph_unknown_branch_candidate_is_never_claimed():
    # "cache-dead" is cache plus the hex dead, so it parses as a node id and is
    # ordinary English. Claiming it passed CI and then made bind_closure_claims
    # refuse the WHOLE binding at merge, so the real node never closed and
    # nothing said so. A gate may DEMAND liberally; a producer that MINTS must
    # be right.
    out = ensure_closure_trailer(
        "Summary.", "feature/x-49ec-cache-dead", known_ids=KNOWN
    )
    assert parse_closure_trailer(out) == ["x-49ec"]


def test_an_unreadable_graph_claims_nothing_from_the_branch():
    # Empty is the safe direction: nothing verified means nothing claimed, and
    # the CI gate reds loudly where a bogus claim would have passed silently.
    out = ensure_closure_trailer("Summary.", "feature/x-49ec", known_ids=frozenset())
    assert parse_closure_trailer(out) == []


def test_extra_ids_are_an_assertion_and_bypass_verification():
    # The caller knows these ship here; the branch is only ever a guess.
    out = ensure_closure_trailer(
        "Summary.", "chore/tidy-docs", extra_ids=["x-aaaa"], known_ids=frozenset()
    )
    assert parse_closure_trailer(out) == ["x-aaaa"]


# ---- Call site: the batch lane ----

def test_batch_ship_claims_every_member(monkeypatch):
    from fno.backlog import batch as batch_mod

    calls = []

    def fake_run(argv, cwd=None, **_kwargs):
        calls.append(argv)
        if argv[:3] == ["gh", "pr", "list"]:
            return subprocess.CompletedProcess(argv, 0, "[]", "")
        return subprocess.CompletedProcess(argv, 0, "https://x/pull/7", "")

    # The PRE-RENAME branch shape on purpose. A batch already open when the
    # rename landed still carries it, so pinning only the new shape tests a
    # generation that cannot exist yet. The ship path passes no head ref at
    # all, which is what makes both generations bind their real members.
    monkeypatch.setattr(
        batch_mod, "read_batch", lambda *_a, **_k: {
            "status": "open", "worktree": "/tmp/wt", "branch": "feature/batch-code-a1b2c3",
            "members": ["x-aaaa", "x-bbbb"],
        }
    )
    monkeypatch.setattr(batch_mod, "member_ids", lambda _b: ["x-aaaa", "x-bbbb"])
    # The graph is not consulted for members: they are the caller's assertion.
    monkeypatch.setattr("fno.pr.closure.known_node_ids", lambda: frozenset())
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
    # The refusal NAMES the candidate and never prescribes a trailer to paste.
    # A gate's remediation advice is a PRODUCER of claims, and this producer
    # reads no graph: `_branch_node_ids` matches the id GRAMMAR, which ordinary
    # English also fits (`fix-dead-code` yields `fix-dead`). The old text told
    # the author to write that id, which greens CI and then voids the whole
    # binding at merge - the exact claim `ensure_closure_trailer` refuses.
    assert "x-49ec" in reason
    assert "Backlog-Closure: x-49ec" not in reason, (
        "the refusal prescribes a literal trailer again, which is the "
        "unverified-mint defect this text was rewritten to close"
    )
    assert "fno do pr closure-trailer" in reason


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
    assert _gate(ensure_closure_trailer("Batch body.", ref, known_ids=KNOWN), ref) == 0


@pytest.mark.parametrize(
    "head_ref",
    [
        # The regression. `cache-dead` fits the id grammar and is ordinary
        # English, so the producer refuses to claim it while the gate demanded
        # it, and NO body satisfied both. Measured against the pre-fix gate:
        # this ref exits 1 there and 0 here. It is the docstring's own example.
        "feature/x-49ec-cache-dead",
        # These two already passed both gates and are pinned so they keep
        # passing. Saying which is which matters: three green rows over a
        # one-row fix reads as threefold proof.
        "feature/x-49ec-tidy-docs",
        "feature/x-49ec",
    ],
)
def test_what_the_producer_writes_always_satisfies_the_gate(head_ref):
    # The property the two halves must share, run END TO END rather than
    # asserted on the producer alone. The previous parity test pinned the
    # producer half and never invoked `_gate`, so a gate demanding an
    # unclaimable id passed it.
    body = ensure_closure_trailer("Summary.", head_ref, known_ids=KNOWN)
    assert _gate(body, head_ref) == 0, (
        f"producer wrote a body the gate rejects for {head_ref!r}: {body!r}"
    )


def test_a_branch_naming_no_real_node_still_fails_the_gate():
    # The other half of the property, and NOT a bug. On `fix-dead-code` the
    # only candidate is `fix-dead`, which the graph does not carry, so the
    # producer writes no trailer and the gate correctly demands a real claim.
    # The author names the real node or takes the documented hatch.
    # Without this case the parity test above reads as "the producer can always
    # satisfy the gate", which is false and would justify deleting the gate.
    body = ensure_closure_trailer("Summary.", "fix-dead-code", known_ids=KNOWN)
    assert "Backlog-Closure" not in body
    assert _gate(body, "fix-dead-code") != 0


def test_the_gate_still_fails_a_body_that_claims_nothing():
    # The negative control for the parity test. Loosening the gate to "at least
    # one" must not loosen it to "never fails", which is the shape that reads
    # as a passing gate while checking nothing.
    assert _gate("Summary with no trailer at all.", "feature/x-49ec") != 0


def test_hook_docstrings_do_not_claim_creation_is_ungated():
    # A PR must not ship a doc its own code disproves.
    #
    # Assert a POSITIVE marker, never an absence. The old body checked that one
    # exact phrase was gone, so any REWORD of the same stale claim passed and
    # the test read as protection. An absence has two explanations, the real
    # outcome and "the instrument never ran", and this one could not tell them
    # apart: it passes identically against an empty file.
    text = HOOK.read_text(encoding="utf-8")
    assert text, "hook source read as empty, so any absence check here is vacuous"
    # The docstring must state the gate that actually exists.
    assert "_closure_trailer_refusal" in text
    assert re.search(r"gh pr create.{0,400}closure", text, re.IGNORECASE | re.DOTALL), (
        "the hook no longer documents that it gates `gh pr create` on a "
        "closure trailer, so its docstring and its code disagree"
    )


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
    # A crashed hook prints nothing, and silence here is the ALLOW shape. So a
    # NameError from a bad edit once made every case in a sweep "pass",
    # including the control. Exit code first, then the payload.
    assert result.returncode == 0, (
        f"hook exited {result.returncode}, so its empty stdout is a crash "
        f"rather than an allow: {result.stderr.strip()[:400]}"
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


# Wrapped creates, which had no test at all before this.
#
# Measured against the pre-unification hook rather than assumed: of the seven
# cases below, exactly ONE changed behavior when `_pr_create_signals` stopped
# re-implementing the prefix walk. `timeout 60 FNO_PR_CLOSURE_OK=1 gh pr
# create` denied before and allows now, because the thinner copy treated the
# wrapper's own option as a hard stop and never reached the assignment. The
# other six already behaved correctly and are pinned so they keep doing so.
#
# Saying that plainly matters more than the count. A comment claiming all
# seven caught the bug would read as sevenfold proof of a onefold fix.

@pytest.mark.parametrize(
    "command",
    [
        # A shell runner holds the whole create in one quoted token.
        'bash -c "gh pr create --title t --body no-trailer-here"',
        'sh -c "gh pr create --title t --body no-trailer-here"',
        # A wrapper carrying its OWN option, which used to hide the verb.
        "timeout 60 gh pr create --title t --body no-trailer-here",
        "env -i gh pr create --title t --body no-trailer-here",
    ],
)
def test_hook_denies_a_wrapped_create_with_no_trailer(command, node_branch_repo):
    payload = _hook_main(command, node_branch_repo)
    assert payload["permissionDecision"] == "deny"


@pytest.mark.parametrize(
    "command",
    [
        # The hatch travels INSIDE the runner's quoted command.
        'bash -c "FNO_PR_CLOSURE_OK=1 gh pr create --title t --body b"',
        # The hatch sits after a wrapper that took its own option first.
        "timeout 60 FNO_PR_CLOSURE_OK=1 gh pr create --title t --body b",
        # And before it, where a shell applies it to the wrapped command too.
        "FNO_PR_CLOSURE_OK=1 timeout 60 gh pr create --title t --body b",
    ],
)
def test_hook_honors_a_real_hatch_through_a_wrapper(command, node_branch_repo):
    # The assignment is a real prefix of the command that runs, so it is the
    # operator's stated intent whichever side of the wrapper it sits on.
    payload = _hook_main(command, node_branch_repo)
    assert payload is None or payload["permissionDecision"] != "deny"


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
    # The --head id is still what gets NAMED, and is still not prescribed.
    reason = payload["permissionDecisionReason"]
    assert "x-1111" in reason
    assert "Backlog-Closure: x-1111" not in reason


@pytest.mark.parametrize("flag", ["--body-file", "-F"])
def test_hook_reads_a_body_file_under_either_spelling(flag, node_branch_repo, tmp_path):
    body = tmp_path / "pr-body.md"
    body.write_text(ensure_closure_trailer("Summary.", "feature/x-49ec", known_ids=KNOWN))
    payload = _hook_main(f"gh pr create --title t {flag} {body}", node_branch_repo)
    assert payload is None or payload["permissionDecision"] != "deny"


def test_hook_never_denies_on_a_body_file_it_cannot_judge(node_branch_repo, tmp_path):
    # A stale file must NOT deny. This hook runs BEFORE the command, so a
    # fixed, never-cleaned path like .fno/pr-body.md holds the PREVIOUS PR's
    # body while the very same command is about to overwrite it. Judging that
    # content denied the compose-then-pass flow skills/pr/references/create.md
    # prescribes. The hook cannot tell stale from final, so it never had a
    # sound deny here, and its contract already accepts a false ALLOW that CI
    # catches while ruling out a false DENY.
    body = tmp_path / "pr-body.md"
    body.write_text("Summary.\n\nBacklog-Closure: x-1111\n")
    payload = _hook_main(f"gh pr create --title t --body-file {body}", node_branch_repo)
    assert payload is None or payload["permissionDecision"] != "deny"


def test_hook_never_denies_on_a_body_file_that_does_not_exist_yet(node_branch_repo, tmp_path):
    # The same defect with the opposite symptom, and the reason the sound
    # answer is "always allow" rather than "read it more carefully".
    payload = _hook_main(
        f"gh pr create --title t --body-file {tmp_path / 'not-written-yet.md'}",
        node_branch_repo,
    )
    assert payload is None or payload["permissionDecision"] != "deny"


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
    reason = payload["permissionDecisionReason"]
    assert "x-49ec" in reason
    assert "fno do pr closure-trailer" in reason
    assert "Backlog-Closure: x-49ec" not in reason


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


def _pending(node_id: str, successor: str, surfaces: list[str]) -> dict:
    return {
        "id": node_id,
        "superseded_by": successor,
        "supersession": {
            "successor": successor,
            "cause": "c",
            "surfaces": surfaces,
            "verified_at": None,
        },
    }


def test_dotfile_surface_does_not_match_a_dot_stripped_lookalike():
    """lstrip('./') ate the leading dot, so `github/ci.yml` matched `.github/ci.yml`."""
    from fno.graph._reconcile import verify_pending_supersessions

    entries = [_pending("x-old", "x-new", ["github/ci.yml"])]
    receipts = verify_pending_supersessions(
        entries, successor="x-new", changed_files=[".github/ci.yml"], evidence_pr=1
    )
    assert [r["kind"] for r in receipts] == ["supersession_unverified"]
    assert entries[0]["supersession"]["verified_at"] is None


def test_leading_dot_slash_is_still_stripped_on_both_sides():
    from fno.graph._reconcile import verify_pending_supersessions

    entries = [_pending("x-old", "x-new", ["./cli/a.py"])]
    receipts = verify_pending_supersessions(
        entries, successor="x-new", changed_files=["cli/a.py"], evidence_pr=1
    )
    assert receipts == []
    assert entries[0]["supersession"]["verified_at"]


def test_truncated_evidence_blames_the_truncation_not_the_surface():
    """A miss against a short file list is an absence with two explanations."""
    from fno.graph._reconcile import verify_pending_supersessions

    entries = [_pending("x-old", "x-new", ["cli/untouched.py"])]
    receipts = verify_pending_supersessions(
        entries,
        successor="x-new",
        changed_files=["cli/a.py"],
        evidence_pr=1,
        evidence_complete=False,
    )
    assert [r["kind"] for r in receipts] == ["supersession_evidence_truncated"]
    assert entries[0]["supersession"]["verified_at"] is None


def test_complete_rest_file_list_is_not_marked_truncated():
    from fno.graph._reconcile import query_pr_merge_state

    state = query_pr_merge_state(
        5,
        info_reader=lambda number, *, repo=None, cwd=None: ({
            "pr": 5,
            "state": "MERGED",
            "url": "u",
            "merged_at": "t",
            "merge_sha": "sha",
        }, ""),
        files_reader=lambda number, *, repo=None, cwd=None: ([f"f{i}.py" for i in range(100)], ""),
    )
    assert state.files_truncated is False


def test_query_composes_injected_rest_readers_and_complete_files():
    from fno.graph._reconcile import query_pr_merge_state

    calls: list[tuple[str, int, str | None]] = []

    def info_reader(number, *, repo=None, cwd=None):
        calls.append(("info", int(number), repo))
        return ({
            "pr": int(number),
            "url": "https://github.com/o/r/pull/5",
            "state": "MERGED",
            "head_sha": "head",
            "head_ref": "feature/test",
            "base_ref": "main",
            "mergeable": "UNKNOWN",
            "merged_at": "2026-08-25T12:00:00Z",
            "merge_sha": "merge",
        }, "")

    def files_reader(number, *, repo=None, cwd=None):
        calls.append(("files", int(number), repo))
        return (["cli/a.py", "cli/b.py"], "")

    state = query_pr_merge_state(
        5,
        repo="o/r",
        info_reader=info_reader,
        files_reader=files_reader,
    )

    assert state.state == "MERGED"
    assert state.url == "https://github.com/o/r/pull/5"
    assert state.merged_at == "2026-08-25T12:00:00Z"
    assert state.merge_sha == "merge"
    assert state.changed_files == ["cli/a.py", "cli/b.py"]
    assert state.files_truncated is False
    assert calls == [("info", 5, "o/r"), ("files", 5, "o/r")]


def test_query_source_contains_no_graphql_view_read():
    import inspect

    from fno.graph._reconcile import query_pr_merge_state

    assert "gh pr view" not in inspect.getsource(query_pr_merge_state)


def test_query_runner_guard_sees_only_routed_rest_argv():
    from fno.graph._reconcile import query_pr_merge_state
    from fno.pr._proc import Result

    calls: list[list[str]] = []
    pull = {
        "number": 5,
        "html_url": "https://github.com/o/r/pull/5",
        "state": "closed",
        "merged": True,
        "merged_at": "2026-08-25T12:00:00Z",
        "merge_commit_sha": "merge",
        "head": {"sha": "head", "ref": "feature/test"},
        "base": {"ref": "main"},
    }

    def runner(cmd, **kwargs):
        calls.append(list(cmd))
        assert cmd[:2] == ["gh", "api"], f"unrouted PR read: {cmd}"
        if "/files?" in cmd[-1]:
            return Result(0, '[{"filename":"cli/a.py"}]', "")
        return Result(0, json.dumps(pull), "")

    state = query_pr_merge_state(5, repo="o/r", runner=runner)

    assert state.state == "MERGED"
    assert state.changed_files == ["cli/a.py"]
    assert all(cmd[:3] != ["gh", "pr", "view"] for cmd in calls)


def test_query_rest_timeout_is_retryable_availability_failure():
    import subprocess

    from fno.graph._reconcile import ReconcileError, query_pr_merge_state

    def runner(cmd, **kwargs):
        raise subprocess.TimeoutExpired(cmd, 30)

    with pytest.raises(ReconcileError) as raised:
        query_pr_merge_state(5, repo="o/r", runner=runner)

    assert raised.value.kind == "availability"
    assert raised.value.retryable is True
    assert "retry" in raised.value.remedy_for(pr_number=5, repo="o/r").lower()


def test_manifest_node_id_beats_a_stale_branch():
    """A reused worktree's branch still names the PREVIOUS node; the manifest wins."""
    from fno.pr.closure import bind_created_pr

    entries = [{"id": "x-9f0c"}, {"id": "x-1a2b"}]
    result = bind_created_pr(
        entries,
        head_ref="feature/x-prev",
        pr_url="https://github.com/o/r/pull/7",
        node_id="x-1a2b",
    )
    assert result.outcome == "bound"
    assert result.bound_ids == ["x-1a2b"]
    assert {e["id"]: e.get("pr_number") for e in entries} == {"x-9f0c": None, "x-1a2b": 7}


def test_manifest_id_binds_when_the_branch_names_no_node():
    from fno.pr.closure import bind_created_pr

    entries = [{"id": "x-1a2b"}]
    result = bind_created_pr(
        entries, head_ref="tmp-scratch", pr_url="https://github.com/o/r/pull/7",
        node_id="x-1a2b",
    )
    assert result.outcome == "bound"
    assert result.bound_ids == ["x-1a2b"]


def test_unknown_manifest_id_falls_back_to_the_branch():
    from fno.pr.closure import bind_created_pr

    entries = [{"id": "x-1a2b"}]
    result = bind_created_pr(
        entries, head_ref="feature/x-1a2b", pr_url="https://github.com/o/r/pull/7",
        node_id="x-dead",
    )
    assert result.outcome == "bound"
    assert result.bound_ids == ["x-1a2b"]


def test_supersede_keeps_the_human_reason(tmp_path, monkeypatch):
    """--reason is accepted and documented, so it must land somewhere."""
    import json
    from typer.testing import CliRunner
    from fno.graph.cli import cli

    # The graph path is frozen at import time (see conftest), so point at the
    # already-sandboxed location rather than a fresh env var.
    from fno.graph._constants import GRAPH_JSON as graph

    graph.parent.mkdir(parents=True, exist_ok=True)
    graph.write_text(json.dumps({"entries": [
        {"id": "x-1a2b", "title": "new"},
        {"id": "x-9f0c", "title": "old"},
    ]}))

    result = CliRunner().invoke(cli, [
        "supersede", "x-1a2b", "--replaces", "x-9f0c",
        "--cause", "owned the parser", "--surface", "cli/p.py",
        "--reason", "folded into the rewrite",
    ])
    assert result.exit_code == 0, result.output
    rows = {e["id"]: e for e in json.loads(graph.read_text())["entries"]}
    assert rows["x-9f0c"]["supersession"]["reason"] == "folded into the rewrite"


def test_supersede_without_surface_names_a_runnable_example():
    from typer.testing import CliRunner
    from fno.graph.cli import cli

    result = CliRunner().invoke(cli, [
        "supersede", "x-1a2b", "--replaces", "x-9f0c", "--cause", "c",
    ])
    assert result.exit_code == 1
    assert "--surface" in result.output
    assert "fno backlog supersede" in result.output


def test_a_closed_successor_still_owes_its_predecessor_a_verdict():
    """The close path is not the only way a successor reaches done."""
    from fno.graph._reconcile import successors_owing_verification

    entries = [
        {"id": "x-9f0c", "superseded_by": "x-1a2b",
         "supersession": {"successor": "x-1a2b", "surfaces": ["cli/p.py"], "verified_at": None}},
        {"id": "x-1a2b", "completed_at": "2026-08-20T00:00:00Z", "pr_number": 7},
    ]
    assert list(successors_owing_verification(entries)) == ["x-1a2b"]


def test_an_open_successor_is_not_owed_yet():
    """Its ordinary close still lies ahead, and that path already verifies."""
    from fno.graph._reconcile import successors_owing_verification

    entries = [
        {"id": "x-9f0c", "superseded_by": "x-1a2b",
         "supersession": {"successor": "x-1a2b", "surfaces": ["cli/p.py"], "verified_at": None}},
        {"id": "x-1a2b", "pr_number": 7},
    ]
    assert successors_owing_verification(entries) == {}


def test_an_already_verified_predecessor_is_not_owed():
    from fno.graph._reconcile import successors_owing_verification

    entries = [
        {"id": "x-9f0c", "superseded_by": "x-1a2b",
         "supersession": {"successor": "x-1a2b", "surfaces": [], "verified_at": "t"}},
        {"id": "x-1a2b", "completed_at": "t", "pr_number": 7},
    ]
    assert successors_owing_verification(entries) == {}
