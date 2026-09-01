from __future__ import annotations

import logging
import re
import time
from pathlib import Path

import pytest
import yaml

from fno.config import settings_from_files
from fno.config import optouts
from fno.config import writer
from fno.claims import Claim, acquire_claim, claim_status
from fno.claims.core import reap_dead_claims
from fno.claims.io import claim_path, claims_root_for


def test_unbacked_self_review_opt_out_reverts_to_default(
    tmp_path, monkeypatch, caplog
):
    config = tmp_path / "config.toml"
    config.write_text(
        "[review]\nself_review_required = false\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims-root"))
    caplog.set_level(logging.WARNING)

    settings = settings_from_files([config])

    assert settings.review.self_review_required is True
    assert "review.self_review_required" in caplog.text
    assert "free" in caplog.text


def test_opt_out_write_acquires_global_claim_and_reports_lease(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "settings.yaml"))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
    monkeypatch.setattr(writer, "_resolve_optout_holder", lambda: "session-a")

    result = writer.set_config_value("review.self_review_required", "false")

    assert result.lease["holder"] == "session-a"
    assert result.lease["expires_at"] > result.lease["acquired_at"]
    status = claim_status(
        "config-optout:review.self_review_required",
        root=claims_root_for("config-optout:review.self_review_required"),
    )
    assert status["state"] == "live"
    assert status["holder"] == "session-a"
    assert (tmp_path / "config.toml").read_text(encoding="utf-8").strip()


def test_second_holder_cannot_rewrite_a_live_opt_out(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "settings.yaml"))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
    monkeypatch.setattr(writer, "_resolve_optout_holder", lambda: "session-a")
    writer.set_config_value("review.self_review_required", "false")

    monkeypatch.setattr(writer, "_resolve_optout_holder", lambda: "session-b")
    with pytest.raises(writer.ConfigSetError, match="session-a"):
        writer.set_config_value("review.self_review_required", "false")

    assert "self_review_required = false" in (
        tmp_path / "config.toml"
    ).read_text(encoding="utf-8")


def test_owner_reset_releases_the_opt_out_claim(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "settings.yaml"))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
    monkeypatch.setattr(writer, "_resolve_optout_holder", lambda: "session-a")
    writer.set_config_value("review.self_review_required", "false")

    writer.set_config_value("review.self_review_required", "true")

    status = claim_status(
        "config-optout:review.self_review_required",
        root=claims_root_for("config-optout:review.self_review_required"),
    )
    assert status["state"] == "free"
    assert "self_review_required = true" in (
        tmp_path / "config.toml"
    ).read_text(encoding="utf-8")


def test_explicit_empty_optional_apps_requires_a_live_claim(tmp_path, monkeypatch):
    config = tmp_path / "config.toml"
    config.write_text("[review]\noptional_apps = []\n", encoding="utf-8")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))

    revoked = settings_from_files([config])
    assert revoked.review.optional_apps is None

    acquire_claim(
        "config-optout:review.optional_apps",
        "session-a",
        root=claims_root_for("config-optout:review.optional_apps"),
    )
    honored = settings_from_files([config])
    assert honored.review.optional_apps == []


def test_unreadable_claim_instrument_revokes_and_names_instrument(
    tmp_path, monkeypatch, caplog
):
    config = tmp_path / "config.toml"
    config.write_text(
        "[review]\nself_review_required = false\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
    monkeypatch.setattr(
        optouts,
        "claim_status",
        lambda *args, **kwargs: (_ for _ in ()).throw(OSError("permission denied")),
    )

    with caplog.at_level(logging.WARNING):
        settings = settings_from_files([config])

    assert settings.review.self_review_required is True
    assert "unreadable" in caplog.text


def test_config_set_output_names_the_opt_out_lease(tmp_path, monkeypatch, capsys):
    from typer.testing import CliRunner

    from fno.config_cli import app

    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "settings.yaml"))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
    monkeypatch.setattr(writer, "_resolve_optout_holder", lambda: "session-a")

    result = CliRunner().invoke(
        app, ["set", "review.self_review_required", "false"]
    )

    assert result.exit_code == 0, result.output
    assert "held by session-a" in result.output
    assert "expires at" in result.output
    assert "release: fno config set review.self_review_required true" in result.output


def test_reaper_restores_the_recorded_prior_value(tmp_path, monkeypatch):
    config = tmp_path / "config.toml"
    config.write_text(
        "[review]\nself_review_required = false\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
    key = "config-optout:review.self_review_required"
    root = claims_root_for(key)
    claim = Claim(
        schema_version=2,
        key=key,
        holder="session-a",
        acquired_at=int(time.time() * 1000) - 120_000,
        expires_at=int(time.time() * 1000) - 60_000,
        pid=None,
        pid_unavailable=True,
        host="test-host",
        metadata={
            "config_key": "review.self_review_required",
            "config_path": str(config),
            "scope": "global",
            "prior_present": False,
            "prior_value": None,
        },
    )
    path = claim_path(key, root=root)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        yaml.safe_dump(claim.model_dump(exclude_none=True)),
        encoding="utf-8",
    )

    optout_sink: list = []
    summary = reap_dead_claims(roots=[root], apply=True, optout_sink=optout_sink)

    assert summary["reaped"] == 1
    from fno.config.writer import restore_reaped_optouts

    assert restore_reaped_optouts(optout_sink) == []
    assert "self_review_required = false" not in config.read_text(encoding="utf-8")
    assert "self_review_required = true" not in config.read_text(encoding="utf-8")


def test_doctor_reports_unbacked_file_residue(tmp_path, monkeypatch):
    from fno import config as config_mod
    from fno import doctor

    settings = tmp_path / "settings.yaml"
    settings.write_text(
        "review:\n  self_review_required: false\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
    monkeypatch.setattr(config_mod, "_settings_yaml_locations", lambda: [settings])

    report = doctor._merge_gating_optout_report()

    assert report["residue"] == [
        {
            "key": "review.self_review_required",
            "path": str(settings),
            "claim_state": "free",
            "command": "fno config set review.self_review_required true",
        }
    ]


def test_stale_claim_takeover_preserves_the_original_prior_value(tmp_path, monkeypatch):
    from fno.claims.io import claim_path

    config = tmp_path / "config.toml"
    config.write_text(
        "[review]\nself_review_required = true\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "settings.yaml"))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
    monkeypatch.setattr(writer, "_resolve_optout_holder", lambda: "session-a")
    writer.set_config_value("review.self_review_required", "false")

    path = claim_path(
        "config-optout:review.self_review_required",
        root=claims_root_for("config-optout:review.self_review_required"),
    )
    raw = yaml.safe_load(path.read_text(encoding="utf-8"))
    raw.update(
        {
            "schema_version": 2,
            "pid": None,
            "pid_unavailable": True,
            "expires_at": 1,
        }
    )
    path.write_text(yaml.safe_dump(raw), encoding="utf-8")

    monkeypatch.setattr(writer, "_resolve_optout_holder", lambda: "session-b")
    writer.set_config_value("review.self_review_required", "false")

    replacement = yaml.safe_load(path.read_text(encoding="utf-8"))
    assert replacement["metadata"]["prior_present"] is True
    assert replacement["metadata"]["prior_value"] is True


def test_rust_optout_keys_are_registered_in_python_membership():
    from fno.config.optouts import MERGE_GATING_OPTOUTS

    source = (
        Path(__file__).parents[3] / "crates" / "fno-agents" / "src" / "claims.rs"
    ).read_text(encoding="utf-8")
    start = source.index("MERGE_GATING_OPTOUT_KEYS")
    end = source.index("];", start) + 2
    rust_keys = set(re.findall(r'"([a-z_]+\.[a-z_]+)"', source[start:end]))

    assert rust_keys
    assert rust_keys <= set(MERGE_GATING_OPTOUTS)
