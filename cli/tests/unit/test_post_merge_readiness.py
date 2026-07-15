"""Post-merge config readiness oracle (ab-dba85fcc, US3).

Covers the pure verdict function ``post_merge_readiness`` and the
``fno config doctor --post-merge [--json]`` surface that exposes it.

The conftest redirects ``$HOME`` to a throwaway dir at module load, so the
oracle's global-graph activity read never sees the real ``~/.fno/graph.json``;
a repo with no local activity signal therefore resolves to ``dormant`` here.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path

import pytest

from fno.config_cli import PostMergeVerdict, app, post_merge_readiness

VALID_STATUSES = {"ready", "unconfigured", "opted_out", "dormant", "error"}


def _repo(tmp_path: Path, settings_body: str | None) -> Path:
    """Make a repo root with an optional ``.fno/settings.yaml``."""
    fno_dir = tmp_path / ".fno"
    fno_dir.mkdir(parents=True, exist_ok=True)
    if settings_body is not None:
        (fno_dir / "settings.yaml").write_text(settings_body, encoding="utf-8")
    return tmp_path


def _mark_active(repo: Path) -> None:
    """Cheapest activity signal: an in-flight target session manifest."""
    (repo / ".fno" / "target-state.md").write_text("session\n", encoding="utf-8")


def _hash_fno(repo: Path) -> str:
    """Stable hash over every file under ``.fno/`` (path + bytes)."""
    h = hashlib.sha256()
    for p in sorted((repo / ".fno").rglob("*")):
        if p.is_file():
            h.update(str(p.relative_to(repo)).encode())
            h.update(p.read_bytes())
    return h.hexdigest()


# --- AC3-EDGE: opted_out is distinct from unconfigured -----------------------


def test_opted_out_distinct_from_unconfigured(tmp_path: Path) -> None:
    repo = _repo(tmp_path, "config:\n  post_merge:\n    enabled: false\n")
    _mark_active(repo)  # active AND unset, but disabled wins
    verdict = post_merge_readiness(repo)
    assert verdict.status == "opted_out"
    assert verdict.status != "unconfigured"
    assert verdict.enabled is False


# --- AC3-FR: unconfigured is the warn case, distinct from dormant ------------


def test_active_unset_is_unconfigured(tmp_path: Path) -> None:
    repo = _repo(tmp_path, "config:\n  post_merge:\n    enabled: true\n")
    _mark_active(repo)
    verdict = post_merge_readiness(repo)
    assert verdict.status == "unconfigured"
    assert verdict.status not in {"dormant", "opted_out"}
    assert verdict.activity is True
    assert verdict.parking_lot_path is None


def test_activity_via_ledger_pr_number(tmp_path: Path) -> None:
    repo = _repo(tmp_path, "config:\n  post_merge:\n    enabled: true\n")
    (repo / ".fno" / "ledger.json").write_text(
        json.dumps({"entries": [{"session_id": "s1", "pr_number": 123}]}),
        encoding="utf-8",
    )
    verdict = post_merge_readiness(repo)
    assert verdict.activity is True
    assert verdict.status == "unconfigured"


# --- AC1-EDGE / dormant: no activity is silent ------------------------------


def test_dormant_when_no_activity(tmp_path: Path) -> None:
    repo = _repo(tmp_path, "config:\n  post_merge:\n    enabled: true\n")
    verdict = post_merge_readiness(repo)
    assert verdict.status == "dormant"
    assert verdict.activity is False


def test_dormant_with_empty_ledger(tmp_path: Path) -> None:
    repo = _repo(tmp_path, "config:\n  post_merge:\n    enabled: true\n")
    (repo / ".fno" / "ledger.json").write_text(
        json.dumps({"entries": [{"session_id": "s1"}]}), encoding="utf-8"
    )
    assert post_merge_readiness(repo).status == "dormant"


# --- ready -------------------------------------------------------------------


def test_ready_when_active_and_configured(tmp_path: Path) -> None:
    repo = _repo(
        tmp_path,
        "config:\n"
        "  post_merge:\n"
        "    parking_lot_path: internal/etl/backlog/parking-lot.md\n"
        "  project:\n"
        "    id: example-pipeline\n",
    )
    _mark_active(repo)
    verdict = post_merge_readiness(repo)
    assert verdict.status == "ready"
    assert verdict.parking_lot_path == "internal/etl/backlog/parking-lot.md"
    assert verdict.project_id == "example-pipeline"
    assert verdict.note is None  # project.id set -> no soft note


def test_local_parking_lot_path_ignored_project_id_still_layers(tmp_path: Path) -> None:
    # x-071c: parking_lot_path left the worktree-local allowlist, so a lane-local
    # settings.local.yaml can no longer supply it - only the shared/canonical
    # config can. With shared post_merge enabled but PATH-less, a local-only
    # parking_lot_path is ignored and the oracle reports unconfigured, NOT ready.
    # project.id (the surviving allowlist key) still layers.
    repo = _repo(tmp_path, "config:\n  post_merge:\n    enabled: true\n")
    (repo / ".fno" / "settings.local.yaml").write_text(
        "config:\n"
        "  post_merge:\n"
        "    parking_lot_path: worktree/parking-lot.md\n"  # ignored (not allowlisted)
        "  project:\n"
        "    id: this-worktree\n",
        encoding="utf-8",
    )
    _mark_active(repo)
    verdict = post_merge_readiness(repo)
    assert verdict.status == "unconfigured"
    assert verdict.parking_lot_path != "worktree/parking-lot.md"


def test_ready_via_shared_path_with_local_project_id(tmp_path: Path) -> None:
    # The path comes from shared/canonical config (x-071c); project.id still
    # layers from the per-worktree local file.
    repo = _repo(
        tmp_path, "config:\n  post_merge:\n    parking_lot_path: docs/parking-lot.md\n"
    )
    (repo / ".fno" / "settings.local.yaml").write_text(
        "config:\n  project:\n    id: this-worktree\n", encoding="utf-8"
    )
    _mark_active(repo)
    verdict = post_merge_readiness(repo)
    assert verdict.status == "ready"
    assert verdict.parking_lot_path == "docs/parking-lot.md"
    assert verdict.project_id == "this-worktree"


def test_ready_carries_soft_note_when_project_id_unset(tmp_path: Path) -> None:
    repo = _repo(
        tmp_path,
        "config:\n  post_merge:\n    parking_lot_path: docs/parking-lot.md\n",
    )
    _mark_active(repo)
    verdict = post_merge_readiness(repo)
    assert verdict.status == "ready"
    assert verdict.note and "project.id" in verdict.note  # scaffold-and-note only


# --- AC3-UI / AC1-ERR: error carries the cause, never bare unconfigured ------


def test_malformed_yaml_is_error_with_cause(tmp_path: Path) -> None:
    repo = _repo(tmp_path, "config:\n  post_merge:\n    bad: [unterminated\n")
    _mark_active(repo)
    verdict = post_merge_readiness(repo)
    assert verdict.status == "error"
    assert verdict.status != "unconfigured"
    assert verdict.cause  # a human-readable cause, never empty


def test_schema_invalid_path_is_error(tmp_path: Path) -> None:
    # An absolute parking_lot_path is rejected by PostMergeBlock's validator;
    # the load surfaces as `error`, not a false `ready`.
    repo = _repo(
        tmp_path,
        "config:\n  post_merge:\n    parking_lot_path: /etc/escape.md\n",
    )
    verdict = post_merge_readiness(repo)
    assert verdict.status == "error"
    assert verdict.cause


def test_missing_settings_is_dormant_not_error(tmp_path: Path) -> None:
    repo = _repo(tmp_path, None)  # no settings.yaml at all
    verdict = post_merge_readiness(repo)
    assert verdict.status == "dormant"  # defaults: enabled, unset, no activity


# --- AC3-ERR: the oracle never writes (read-only invariant) ------------------


@pytest.mark.parametrize(
    "body",
    [
        "config:\n  post_merge:\n    enabled: true\n",  # unconfigured/dormant path
        "config:\n  post_merge:\n    parking_lot_path: a/b.md\n",  # ready path
        "config:\n  post_merge:\n    bad: [unterminated\n",  # error path
    ],
)
def test_oracle_is_read_only(tmp_path: Path, body: str) -> None:
    repo = _repo(tmp_path, body)
    _mark_active(repo)
    before = _hash_fno(repo)
    post_merge_readiness(repo)
    assert _hash_fno(repo) == before  # no file under .fno/ created or modified


# --- AC3-HP: JSON verdict via the CLI surface --------------------------------


def test_cli_post_merge_json_is_machine_readable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from typer.testing import CliRunner

    repo = _repo(tmp_path, "config:\n  post_merge:\n    enabled: true\n")
    _mark_active(repo)
    monkeypatch.chdir(repo)  # _repo_root() falls back to cwd (tmp is not a git repo)

    result = CliRunner().invoke(app, ["doctor", "--post-merge", "--json"])
    assert result.exit_code == 0
    payload = json.loads(result.stdout.strip())
    assert payload["status"] in VALID_STATUSES
    assert payload["status"] == "unconfigured"
    assert "activity" in payload and "enabled" in payload


def test_cli_post_merge_human_line_is_distinguishable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    repo = _repo(tmp_path, "config:\n  post_merge:\n    enabled: true\n")
    _mark_active(repo)
    monkeypatch.chdir(repo)
    from typer.testing import CliRunner

    result = CliRunner().invoke(app, ["doctor", "--post-merge"])
    assert result.exit_code == 0
    # AC1-UI: a single human line naming the key and the fix.
    assert "config.post_merge.parking_lot_path" in result.stdout
    assert "fno setup post-merge" in result.stdout


def test_unrelated_local_key_does_not_error(tmp_path: Path) -> None:
    # codex review (PR #511): a repo that sets config.obsidian.enabled locally
    # while supplying config.obsidian.vault globally would fail a FULL-model
    # validate (ObsidianBlock requires a vault when enabled). The oracle
    # validates ONLY the post_merge block, so an unrelated local key must not
    # turn a perfectly-configured post_merge into a false `error`.
    repo = _repo(
        tmp_path,
        "config:\n"
        "  obsidian:\n"
        "    enabled: true\n"  # no vault locally -> full-model validate would raise
        "  post_merge:\n"
        "    parking_lot_path: docs/parking-lot.md\n",
    )
    _mark_active(repo)
    verdict = post_merge_readiness(repo)
    assert verdict.status == "ready"  # not error
    assert verdict.parking_lot_path == "docs/parking-lot.md"


def test_non_list_ledger_entries_does_not_crash(tmp_path: Path) -> None:
    # gemini review (PR #511): a non-iterable `entries` must not raise a
    # TypeError out of the activity read (the except clause only caught
    # OSError/ValueError); the isinstance(list) guard biases dormant instead.
    repo = _repo(tmp_path, "config:\n  post_merge:\n    enabled: true\n")
    (repo / ".fno" / "ledger.json").write_text(
        json.dumps({"entries": 5}), encoding="utf-8"
    )
    verdict = post_merge_readiness(repo)  # must not raise
    assert verdict.status == "dormant"


def test_verdict_to_dict_roundtrips() -> None:
    v = PostMergeVerdict(status="dormant", enabled=True, activity=False)
    d = v.to_dict()
    assert d["status"] == "dormant"
    assert set(d) >= {"status", "enabled", "activity", "parking_lot_path"}
