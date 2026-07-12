"""Tests for the status-sink fanout (x-2057).

Layer 2 of the status-breakpoints protocol: a dumb, config-driven dispatcher
that sweeps ``.fno/events.jsonl`` and routes x-dbaf protocol-family events to
external sinks. This file covers all ACs across the six user stories; the
``-k`` filters in the plan's per-task verify lines select the relevant subset.
"""
from __future__ import annotations

import pytest

from fno.config import ConfigBlock, StatusFanoutConfig, StatusSinkConfig


# ── US1: config model ───────────────────────────────────────────────────────


def test_config_fanout_defaults() -> None:
    f = StatusFanoutConfig()
    assert f.interval_secs == 5
    assert f.http_timeout_secs == 5
    assert f.retries == 2


def test_config_sink_minimal_text_webhook_valid() -> None:
    s = StatusSinkConfig(
        name="ops-discord",
        type="text-webhook",
        events=["blocked"],
        url="https://discord.com/api/webhooks/x",
        template="{from} blocked on {node}",
        field="content",
    )
    assert s.name == "ops-discord"
    assert s.enabled is True  # default on


def test_config_empty_sinks_is_default_noop() -> None:
    assert ConfigBlock().status_sinks == []


def test_config_status_sinks_nonlist_coerces_empty() -> None:
    # A container-level typo (a scalar where a list belongs) fails safe to [],
    # never bricks settings load for the whole project.
    assert ConfigBlock(status_sinks=42).status_sinks == []


def test_config_duplicate_sink_name_rejected() -> None:
    with pytest.raises(ValueError, match="[Dd]uplicate"):
        ConfigBlock(
            status_sinks=[
                {"name": "dup", "type": "backlog-progress"},
                {"name": "dup", "type": "backlog-progress"},
            ]
        )


def test_config_bad_match_key_rejected_names_allowed_keys() -> None:
    # AC1-UI: a match key outside the envelope whitelist fails validation with a
    # message naming the allowed keys.
    with pytest.raises(ValueError) as ei:
        StatusSinkConfig(
            name="s",
            type="backlog-progress",
            match={"projct": "fno"},  # typo
        )
    msg = str(ei.value)
    assert "projct" in msg
    assert "project" in msg  # the allowed-keys list is surfaced


def test_config_valid_match_keys_pass() -> None:
    s = StatusSinkConfig(
        name="s",
        type="backlog-progress",
        match={"project": "fno", "outcome": "FAILED"},
    )
    assert s.match == {"project": "fno", "outcome": "FAILED"}


def test_config_match_on_data_rejected() -> None:
    # `data` is an envelope key but is a nested object, not an equality target.
    with pytest.raises(ValueError):
        StatusSinkConfig(name="s", type="backlog-progress", match={"data": "x"})


def test_config_webhook_requires_exactly_one_of_url_url_env_both() -> None:
    with pytest.raises(ValueError, match="url"):
        StatusSinkConfig(
            name="s",
            type="json-webhook",
            url="https://x",
            url_env="OPS_URL",
        )


def test_config_webhook_requires_exactly_one_of_url_url_env_neither() -> None:
    with pytest.raises(ValueError, match="url"):
        StatusSinkConfig(name="s", type="json-webhook")


def test_config_url_env_alone_valid() -> None:
    s = StatusSinkConfig(name="s", type="json-webhook", url_env="OPS_URL")
    assert s.url_env == "OPS_URL"
    assert s.url is None


def test_config_backlog_progress_needs_no_url() -> None:
    # backlog-progress is a local write; neither url nor url_env applies.
    s = StatusSinkConfig(name="s", type="backlog-progress")
    assert s.url is None and s.url_env is None


def test_config_unknown_type_rejected() -> None:
    with pytest.raises(ValueError, match="type"):
        StatusSinkConfig(name="s", type="carrier-pigeon", url="https://x")
