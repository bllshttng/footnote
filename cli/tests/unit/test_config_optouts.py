from __future__ import annotations

import logging
import re
import time
from pathlib import Path

import pytest
import yaml

from fno.config import settings_from_files
from fno.config import writer
from fno.claims import Claim, acquire_claim, claim_status
from fno.claims import optout_lease
from fno.claims.core import reap_dead_claims
from fno.claims.io import claim_path, global_claims_root


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
    monkeypatch.setattr(optout_lease, "_resolve_optout_holder", lambda: "session-a")

    result = optout_lease.set_config_value("review.self_review_required", "false")

    assert result.lease["holder"] == "session-a"
    assert result.lease["expires_at"] > result.lease["acquired_at"]
    status = claim_status("config-optout:review.self_review_required")
    assert status["state"] == "live"
    assert status["holder"] == "session-a"
    assert (tmp_path / "config.toml").read_text(encoding="utf-8").strip()


def test_second_holder_cannot_rewrite_a_live_opt_out(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "settings.yaml"))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
    monkeypatch.setattr(optout_lease, "_resolve_optout_holder", lambda: "session-a")
    optout_lease.set_config_value("review.self_review_required", "false")

    monkeypatch.setattr(optout_lease, "_resolve_optout_holder", lambda: "session-b")
    with pytest.raises(writer.ConfigSetError, match="session-a"):
        optout_lease.set_config_value("review.self_review_required", "false")

    assert "self_review_required = false" in (
        tmp_path / "config.toml"
    ).read_text(encoding="utf-8")


def test_bare_writer_refuses_merge_gating_keys_fail_closed(tmp_path, monkeypatch):
    # The lease lane lives in fno.claims.optout_lease; fno.config may not
    # import the claims layer, so a plain writer set/unset without the
    # injected ops must refuse the key instead of writing an unleased opt-out.
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "settings.yaml"))

    with pytest.raises(writer.ConfigSetError, match="claims lane"):
        writer.set_config_value("review.self_review_required", "false")
    with pytest.raises(writer.ConfigSetError, match="claims lane"):
        writer.unset_config_value("review.self_review_required")

    assert not (tmp_path / "settings.yaml").exists()


def test_mid_batch_refusal_releases_the_lease_acquired_earlier(
    tmp_path, monkeypatch
):
    # A batch acquiring two opt-outs whose second claim is held by another
    # session must not strand the first session's lease until TTL.
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "settings.yaml"))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
    acquire_claim(
        "config-optout:review.optional_apps",
        "session-b",
    )
    monkeypatch.setattr(optout_lease, "_resolve_optout_holder", lambda: "session-a")

    with pytest.raises(writer.ConfigSetError, match="session-b"):
        optout_lease.set_config_values(
            [
                ("review.self_review_required", "false"),
                ("review.optional_apps", "[]"),
            ]
        )

    status = claim_status("config-optout:review.self_review_required")
    assert status["state"] == "free"
    assert not (tmp_path / "settings.yaml").exists()


def test_owner_reset_releases_the_opt_out_claim(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "settings.yaml"))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
    monkeypatch.setattr(optout_lease, "_resolve_optout_holder", lambda: "session-a")
    optout_lease.set_config_value("review.self_review_required", "false")

    optout_lease.set_config_value("review.self_review_required", "true")

    status = claim_status("config-optout:review.self_review_required")
    assert status["state"] == "free"
    assert "self_review_required = true" in (
        tmp_path / "config.toml"
    ).read_text(encoding="utf-8")


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
        optout_lease,
        "claim_status",
        lambda *args, **kwargs: (_ for _ in ()).throw(OSError("permission denied")),
    )

    with caplog.at_level(logging.WARNING):
        settings = settings_from_files([config])

    assert settings.review.self_review_required is True
    assert "unreadable" in caplog.text


def test_reaper_restores_the_recorded_prior_value(tmp_path, monkeypatch):
    config = tmp_path / "config.toml"
    config.write_text(
        "[review]\nself_review_required = false\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
    key = "config-optout:review.self_review_required"
    root = global_claims_root()
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
    from fno.claims.optout_lease import restore_reaped_optouts

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
