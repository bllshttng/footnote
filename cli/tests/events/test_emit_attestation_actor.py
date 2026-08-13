"""emit-attestation.sh records the attesting actor on review_attestation.

The producer resolves session_id + harness from the live session manifest so a
review_attestation carries WHO certified the diff alongside WHAT. Without it an
author attesting its own diff is indistinguishable from an independent reviewer.
This is the producer-level check; the validator-level accept/reject lives in
test_python_validator.py and the parity corpus.
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

_REPO_ROOT = Path(__file__).resolve().parents[3]
_SCRIPT = _REPO_ROOT / "skills" / "review" / "scripts" / "emit-attestation.sh"


def _temp_git_repo(tmp_path: Path, manifest: str | None) -> Path:
    """A throwaway git repo with one commit; optionally a session manifest."""
    sub = tmp_path / "repo"
    sub.mkdir()
    for args in (
        ["git", "init", "-q"],
        ["git", "config", "user.email", "t@t.t"],
        ["git", "config", "user.name", "t"],
        ["git", "commit", "-q", "--allow-empty", "-m", "init"],
    ):
        subprocess.run(args, cwd=sub, check=True)
    if manifest is not None:
        (sub / ".fno").mkdir()
        (sub / ".fno" / "target-state.md").write_text(manifest)
    return sub


def _last_event(repo: Path) -> dict:
    lines = [
        ln for ln in (repo / ".fno" / "events.jsonl").read_text().splitlines()
        if ln.strip()
    ]
    return json.loads(lines[-1])


def test_attestation_records_session_and_harness(tmp_path: Path) -> None:
    """A manifest present -> the emitted attestation carries the actor."""
    manifest = (
        "session_id: 20260806T225503Z-cl84104-d4f619\n"
        "harness: claude\n"
    )
    repo = _temp_git_repo(tmp_path, manifest)
    env = {**os.environ, "FNO": "fno-py"}
    r = subprocess.run(
        ["bash", str(_SCRIPT), "sigma", "pass"],
        cwd=repo, env=env, capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr
    ev = _last_event(repo)
    assert ev["type"] == "review_attestation"
    assert ev["data"]["session_id"] == "20260806T225503Z-cl84104-d4f619"
    assert ev["data"]["harness"] == "claude"


def test_attestation_records_the_routed_model(tmp_path: Path) -> None:
    """A routed session -> the attestation names the model that rendered the
    verdict. Routing stamps ANTHROPIC_MODEL for the whole worker process, so a
    build-lane worker reviews its own diff on the routed model and no per-spawn
    role guard can see it."""
    repo = _temp_git_repo(tmp_path, "session_id: s\nharness: claude\n")
    env = {
        **os.environ,
        "FNO": "fno-py",
        "ANTHROPIC_MODEL": "glm-5.2[1m]",
        # Userinfo included on purpose: a base_url may carry a credential, and
        # the event log is durable - neither the path nor the key may land in it.
        "ANTHROPIC_BASE_URL": "https://sk-secret@api.z.ai/api/anthropic",
    }
    r = subprocess.run(
        ["bash", str(_SCRIPT), "code-review", "pass"],
        cwd=repo, env=env, capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr
    data = _last_event(repo)["data"]
    assert data["model"] == "glm-5.2[1m]"
    assert data["provider"] == "api.z.ai"
    assert "sk-secret" not in json.dumps(data) and "sk-secret" not in r.stderr


def test_attestation_model_is_empty_not_guessed_when_unrouted(tmp_path: Path) -> None:
    """No routing -> empty, never a guessed default. A fabricated model name
    would be read later as evidence of which model reviewed."""
    repo = _temp_git_repo(tmp_path, "session_id: s\nharness: claude\n")
    env = {
        k: v
        for k, v in os.environ.items()
        if k not in ("ANTHROPIC_MODEL", "ANTHROPIC_BASE_URL")
    }
    env["FNO"] = "fno-py"
    r = subprocess.run(
        ["bash", str(_SCRIPT), "code-review", "pass"],
        cwd=repo, env=env, capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr
    data = _last_event(repo)["data"]
    assert data["model"] == ""
    assert data["provider"] == ""


def test_attestation_attester_session_from_env_marker(tmp_path: Path) -> None:
    """CLAUDE_CODE_SESSION_ID set -> attester_session_id carries it, and it is
    NOT the manifest session_id. session_id is the worktree run id (grepped from
    the manifest), so it equals manifest.session_id for every emitter including a
    reviewer who is not the author; attester_session_id is the live process's
    session and is the one field that varies with who emitted. Asserting the two
    differ is the test that would have caught the tautology the join was built
    on."""
    manifest = (
        "session_id: 20260806T225503Z-cl84104-d4f619\n"
        "harness: claude\n"
    )
    repo = _temp_git_repo(tmp_path, manifest)
    env = {
        **os.environ,
        "FNO": "fno-py",
        "CLAUDE_CODE_SESSION_ID": "3abddea3-ad19-481f-b0c1-af19043c95fe",
    }
    r = subprocess.run(
        ["bash", str(_SCRIPT), "code-review", "pass"],
        cwd=repo, env=env, capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr
    data = _last_event(repo)["data"]
    assert data["attester_session_id"] == "3abddea3-ad19-481f-b0c1-af19043c95fe"
    assert data["session_id"] == "20260806T225503Z-cl84104-d4f619"
    # The two differ: a join on attester_session_id is not the tautology the
    # session_id join is.
    assert data["attester_session_id"] != data["session_id"]


def test_attestation_attester_session_empty_without_marker(tmp_path: Path) -> None:
    """No session marker in env -> attester_session_id is empty, never fallen
    back to the manifest session_id. A fallback would restore the tautology
    session_id already carries, so the producer must NOT default it."""
    manifest = (
        "session_id: 20260806T225503Z-cl84104-d4f619\n"
        "harness: claude\n"
    )
    repo = _temp_git_repo(tmp_path, manifest)
    env = {
        k: v
        for k, v in os.environ.items()
        if k
        not in (
            "CODEX_THREAD_ID",
            "CLAUDE_CODE_SESSION_ID",
            "CODEX_SESSION_ID",
            "GEMINI_SESSION_ID",
            "OPENCODE_SESSION_ID",
        )
    }
    env["FNO"] = "fno-py"
    r = subprocess.run(
        ["bash", str(_SCRIPT), "code-review", "pass"],
        cwd=repo, env=env, capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr
    data = _last_event(repo)["data"]
    assert data["attester_session_id"] == ""
    # The manifest value did NOT leak in as a fallback.
    assert data["session_id"] == "20260806T225503Z-cl84104-d4f619"


def test_attestation_attester_session_marker_precedence(tmp_path: Path) -> None:
    """The marker precedence matches init: CODEX_THREAD_ID > CLAUDE_CODE_SESSION_ID
    > CODEX_SESSION_ID > GEMINI_SESSION_ID. First non-blank wins."""
    repo = _temp_git_repo(tmp_path, "session_id: s\nharness: claude\n")
    env = {
        **os.environ,
        "FNO": "fno-py",
        "CODEX_THREAD_ID": "codex-thread-wins",
        "CLAUDE_CODE_SESSION_ID": "claude-loses",
    }
    r = subprocess.run(
        ["bash", str(_SCRIPT), "code-review", "pass"],
        cwd=repo, env=env, capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr
    assert _last_event(repo)["data"]["attester_session_id"] == "codex-thread-wins"


def test_attestation_attester_session_reads_opencode_marker(tmp_path: Path) -> None:
    """OPENCODE_SESSION_ID is a first-class harness marker (harness_identity.py);
    an opencode-authored review must carry it, or the whole opencode lane reads
    attester_session_id empty and can never classify as self_attested."""
    repo = _temp_git_repo(tmp_path, "session_id: s\nharness: opencode\n")
    env = {
        **os.environ,
        "FNO": "fno-py",
        "OPENCODE_SESSION_ID": "opencode-session-cccc",
    }
    r = subprocess.run(
        ["bash", str(_SCRIPT), "code-review", "pass"],
        cwd=repo, env=env, capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr
    assert _last_event(repo)["data"]["attester_session_id"] == "opencode-session-cccc"


def test_a_flag_shaped_reviewer_emits_nothing(tmp_path: Path) -> None:
    """`--help` must not write gate evidence.

    `$1` was taken as the reviewer name with no validation, so
    `emit-attestation.sh --help` emitted a real `verdict=pass`
    review_attestation against the current HEAD: ASKING THE SCRIPT HOW TO RUN
    IT stamped the merge gate. Any typo'd or stray flag did the same. No
    reviewer name in any registry begins with `-`, so the whole shape is
    refused rather than the one spelling.
    """
    repo = _temp_git_repo(tmp_path, "session_id: s\nharness: claude\n")
    env = {**os.environ, "FNO": "fno-py"}

    helped = subprocess.run(
        ["bash", str(_SCRIPT), "--help"],
        cwd=repo, env=env, capture_output=True, text=True,
    )
    assert helped.returncode == 0
    assert "Usage:" in helped.stdout

    flagged = subprocess.run(
        ["bash", str(_SCRIPT), "--bogus"],
        cwd=repo, env=env, capture_output=True, text=True,
    )
    assert flagged.returncode == 2
    assert "not a reviewer name" in flagged.stderr

    # The positive control: neither run wrote a journal, and a REAL reviewer
    # name still does. Without this half, a script that refused everything
    # would pass the two assertions above.
    assert not (repo / ".fno" / "events.jsonl").exists()
    ok = subprocess.run(
        ["bash", str(_SCRIPT), "code-review", "pass"],
        cwd=repo, env=env, capture_output=True, text=True,
    )
    assert ok.returncode == 0, ok.stderr
    assert _last_event(repo)["data"]["reviewer"] == "code-review"
