"""Tests for ProviderRecord and ProvidersConfig models.

Run: cd cli && uv run pytest src/fno/adapters/providers/test_model.py -v
"""
from __future__ import annotations

from pathlib import Path

import pytest
import pydantic


class TestProviderRecordValid:
    """AC01.1-HP: Valid records parse without error."""

    def test_valid_oauth_dir_record(self):
        """AC01.1-HP: oauth_dir record with credentials_source is accepted."""
        from fno.adapters.providers.model import ProviderRecord

        record = ProviderRecord(
            id="claude-max-primary",
            name="Claude Max (primary)",
            cli="claude",
            auth="oauth_dir",
            credentials_source=Path("~/.claude"),
            priority=10,
        )
        assert record.id == "claude-max-primary"
        assert record.cli == "claude"
        assert record.auth == "oauth_dir"
        assert record.priority == 10
        # account_id defaults to id when not set
        assert record.account_id == "claude-max-primary"
        # tags defaults to empty list
        assert record.tags == []
        # description defaults to None
        assert record.description is None

    def test_valid_api_key_record(self):
        """AC01.1-HP: api_key record with recognized env var is accepted."""
        from fno.adapters.providers.model import ProviderRecord

        record = ProviderRecord(
            id="anthropic-api",
            name="Anthropic API",
            cli="openclaw",
            auth="api_key",
            env={"ANTHROPIC_API_KEY": "${KEYCHAIN:anthropic-api-key}"},
            priority=30,
            tags=["api-credits"],
        )
        assert record.id == "anthropic-api"
        assert record.env == {"ANTHROPIC_API_KEY": "${KEYCHAIN:anthropic-api-key}"}
        assert record.tags == ["api-credits"]

    def test_account_id_explicit(self):
        """account_id can be set explicitly and is preserved."""
        from fno.adapters.providers.model import ProviderRecord

        record = ProviderRecord(
            id="gemini-pro",
            name="Gemini Pro",
            cli="gemini",
            auth="api_key",
            env={"GEMINI_API_KEY": "some-key"},
            account_id="secondary",
        )
        assert record.account_id == "secondary"

    def test_all_cli_literals_accepted(self):
        """All five cli literal values are valid."""
        from fno.adapters.providers.model import ProviderRecord

        for cli_val in ("claude", "gemini", "codex", "openclaw", "hermes"):
            record = ProviderRecord(
                id=f"test-{cli_val}",
                name=f"Test {cli_val}",
                cli=cli_val,
                auth="api_key",
                env={"ANTHROPIC_API_KEY": "key"} if cli_val in ("claude", "codex", "openclaw", "hermes") else {"GEMINI_API_KEY": "key"},
            )
            assert record.cli == cli_val


class TestProviderRecordIdValidation:
    """ID regex validation: ^[a-z][a-z0-9-]{0,63}$"""

    def test_uppercase_id_rejected(self):
        """AC01.2-ERR: id with uppercase letters raises ValidationError."""
        from fno.adapters.providers.model import ProviderRecord

        with pytest.raises(pydantic.ValidationError) as exc_info:
            ProviderRecord(
                id="UPPER",
                name="Upper",
                cli="claude",
                auth="oauth_dir",
                credentials_source=Path("~/.claude"),
            )
        assert "id" in str(exc_info.value).lower() or "pattern" in str(exc_info.value).lower()

    def test_id_starting_with_digit_rejected(self):
        """id must start with lowercase letter."""
        from fno.adapters.providers.model import ProviderRecord

        with pytest.raises(pydantic.ValidationError):
            ProviderRecord(
                id="1bad-id",
                name="Bad",
                cli="claude",
                auth="oauth_dir",
                credentials_source=Path("~/.claude"),
            )

    def test_id_with_underscore_rejected(self):
        """Underscore is not allowed in id (only hyphen)."""
        from fno.adapters.providers.model import ProviderRecord

        with pytest.raises(pydantic.ValidationError):
            ProviderRecord(
                id="bad_id",
                name="Bad",
                cli="claude",
                auth="oauth_dir",
                credentials_source=Path("~/.claude"),
            )


class TestProviderRecordAuthValidation:
    """AC01.2-ERR and AC01.4-EDGE: auth strategy / credentials mismatch."""

    def test_oauth_dir_without_credentials_source_raises(self):
        """AC01.2-ERR: oauth_dir auth without credentials_source raises auth_strategy_mismatch."""
        from fno.adapters.providers.model import ProviderRecord

        with pytest.raises(pydantic.ValidationError) as exc_info:
            ProviderRecord(
                id="claude-no-creds",
                name="Claude Missing Creds",
                cli="claude",
                auth="oauth_dir",
                # credentials_source intentionally omitted
            )
        error_str = str(exc_info.value)
        assert "auth_strategy_mismatch" in error_str

    def test_api_key_with_empty_env_raises(self):
        """AC01.4-EDGE: api_key auth with empty env dict raises auth_strategy_mismatch."""
        from fno.adapters.providers.model import ProviderRecord

        with pytest.raises(pydantic.ValidationError) as exc_info:
            ProviderRecord(
                id="api-empty",
                name="Empty Env",
                cli="claude",
                auth="api_key",
                env={},
            )
        error_str = str(exc_info.value)
        assert "auth_strategy_mismatch" in error_str

    def test_api_key_with_unrecognized_key_raises(self):
        """AC01.4-EDGE: api_key with env but no recognized API key name raises auth_strategy_mismatch."""
        from fno.adapters.providers.model import ProviderRecord

        with pytest.raises(pydantic.ValidationError) as exc_info:
            ProviderRecord(
                id="api-wrong-key",
                name="Wrong Key",
                cli="openclaw",
                auth="api_key",
                env={"SOME_OTHER_KEY": "value"},
            )
        error_str = str(exc_info.value)
        assert "auth_strategy_mismatch" in error_str

    def test_api_key_with_gemini_key_accepted(self):
        """GEMINI_API_KEY is a recognized key name."""
        from fno.adapters.providers.model import ProviderRecord

        record = ProviderRecord(
            id="gemini-api",
            name="Gemini API",
            cli="gemini",
            auth="api_key",
            env={"GEMINI_API_KEY": "some-key"},
        )
        assert record.id == "gemini-api"

    def test_api_key_with_openai_key_accepted(self):
        """OPENAI_API_KEY is a recognized key name."""
        from fno.adapters.providers.model import ProviderRecord

        record = ProviderRecord(
            id="codex-api",
            name="Codex API",
            cli="codex",
            auth="api_key",
            env={"OPENAI_API_KEY": "some-key"},
        )
        assert record.id == "codex-api"


class TestProviderRecordCliValidation:
    """CLI literal validation."""

    def test_invalid_cli_raises(self):
        """ProviderRecord(cli='invalid', ...) raises ValidationError on CLI literal."""
        from fno.adapters.providers.model import ProviderRecord

        with pytest.raises(pydantic.ValidationError):
            ProviderRecord(
                id="test-invalid",
                name="Test",
                cli="invalid",
                auth="oauth_dir",
                credentials_source=Path("~/.claude"),
            )


class TestProviderRecordPriorityValidation:
    """Priority must be non-negative."""

    def test_negative_priority_raises(self):
        """ProviderRecord(id='x', priority=-1) raises ValidationError."""
        from fno.adapters.providers.model import ProviderRecord

        with pytest.raises(pydantic.ValidationError):
            ProviderRecord(
                id="x",
                name="X",
                cli="claude",
                auth="oauth_dir",
                credentials_source=Path("~/.claude"),
                priority=-1,
            )

    def test_zero_priority_accepted(self):
        """Priority of 0 is valid (non-negative)."""
        from fno.adapters.providers.model import ProviderRecord

        record = ProviderRecord(
            id="highest-prio",
            name="Highest",
            cli="claude",
            auth="oauth_dir",
            credentials_source=Path("~/.claude"),
            priority=0,
        )
        assert record.priority == 0


class TestProvidersConfig:
    """ProvidersConfig wrapper and by_id property."""

    def test_by_id_property(self):
        """ProvidersConfig.by_id returns dict keyed by record id."""
        from fno.adapters.providers.model import ProviderRecord, ProvidersConfig

        r1 = ProviderRecord(
            id="claude-primary",
            name="Claude Primary",
            cli="claude",
            auth="oauth_dir",
            credentials_source=Path("~/.claude"),
        )
        r2 = ProviderRecord(
            id="gemini-backup",
            name="Gemini Backup",
            cli="gemini",
            auth="api_key",
            env={"GEMINI_API_KEY": "k"},
        )
        cfg = ProvidersConfig(records=[r1, r2], active="claude-primary")
        by_id = cfg.by_id
        assert "claude-primary" in by_id
        assert "gemini-backup" in by_id
        assert by_id["claude-primary"] is r1

    def test_empty_config(self):
        """ProvidersConfig with empty records and None active is valid."""
        from fno.adapters.providers.model import ProvidersConfig

        cfg = ProvidersConfig(records=[], active=None)
        assert cfg.records == []
        assert cfg.active is None
        assert cfg.by_id == {}

    def test_extra_fields_forbidden(self):
        """extra='forbid' is enforced on ProviderRecord."""
        from fno.adapters.providers.model import ProviderRecord

        with pytest.raises(pydantic.ValidationError):
            ProviderRecord(
                id="claude-extra",
                name="Extra",
                cli="claude",
                auth="oauth_dir",
                credentials_source=Path("~/.claude"),
                unknown_field="bad",
            )


# ---------------------------------------------------------------------------
# Fix 2 regression: tilde expansion in credentials_source
# ---------------------------------------------------------------------------

class TestCredentialsSourceTildeExpansion:
    def test_credentials_source_tilde_is_expanded(self):
        """ProviderRecord expands ~/ in credentials_source so Path.exists() works."""
        from fno.adapters.providers.model import ProviderRecord

        record = ProviderRecord(
            id="x",
            name="X",
            cli="claude",
            auth="oauth_dir",
            credentials_source="~/.claude",
        )
        assert record.credentials_source == Path.home() / ".claude"
        assert record.credentials_source != Path("~/.claude")

    def test_credentials_source_none_unchanged(self):
        """None credentials_source is a no-op for the tilde validator."""
        from fno.adapters.providers.model import ProviderRecord

        record = ProviderRecord(
            id="api-only",
            name="API Only",
            cli="openclaw",
            auth="api_key",
            env={"ANTHROPIC_API_KEY": "key"},
        )
        assert record.credentials_source is None

    def test_credentials_source_absolute_path_unchanged(self):
        """Absolute paths without tilde are returned as-is."""
        from fno.adapters.providers.model import ProviderRecord

        record = ProviderRecord(
            id="claude-abs",
            name="Claude Abs",
            cli="claude",
            auth="oauth_dir",
            credentials_source="/opt/creds/.claude",
        )
        assert record.credentials_source == Path("/opt/creds/.claude")


# ---------------------------------------------------------------------------
# Fix 4 regression: ProvidersConfig rejects duplicate record ids
# ---------------------------------------------------------------------------

class TestProvidersConfigDuplicateIds:
    def test_providers_config_rejects_duplicate_record_ids(self):
        """ProvidersConfig raises ValidationError when two records share the same id."""
        from fno.adapters.providers.model import ProviderRecord, ProvidersConfig

        r1 = ProviderRecord(
            id="claude-max-primary",
            name="Claude Max Primary",
            cli="claude",
            auth="oauth_dir",
            credentials_source=Path.home() / ".claude",
        )
        r2 = ProviderRecord(
            id="claude-max-primary",  # duplicate
            name="Claude Max Primary (copy)",
            cli="claude",
            auth="oauth_dir",
            credentials_source=Path.home() / ".claude",
        )

        with pytest.raises(pydantic.ValidationError, match="duplicate_record_ids"):
            ProvidersConfig(records=[r1, r2])

    def test_providers_config_accepts_unique_ids(self):
        """ProvidersConfig accepts records with distinct ids without error."""
        from fno.adapters.providers.model import ProviderRecord, ProvidersConfig

        r1 = ProviderRecord(
            id="claude-primary",
            name="Claude Primary",
            cli="claude",
            auth="oauth_dir",
            credentials_source=Path.home() / ".claude",
        )
        r2 = ProviderRecord(
            id="gemini-backup",
            name="Gemini Backup",
            cli="gemini",
            auth="api_key",
            env={"GEMINI_API_KEY": "key"},
        )
        cfg = ProvidersConfig(records=[r1, r2])
        assert len(cfg.records) == 2


# ---------------------------------------------------------------------------
# Task 2.2: Pricing schema slot.
# Phase 02 of provider rotation failover (ab-9728b70b).
# ---------------------------------------------------------------------------

class TestPricing:
    def test_hp1_complete_pricing_block_parses(self):
        from fno.adapters.providers.model import Pricing

        p = Pricing(
            input_per_million_usd=15.0,
            output_per_million_usd=75.0,
            cache_read_per_million_usd=1.5,
            cache_write_per_million_usd=18.75,
        )
        assert p.input_per_million_usd == 15.0
        assert p.output_per_million_usd == 75.0
        assert p.cache_read_per_million_usd == 1.5
        assert p.cache_write_per_million_usd == 18.75

    def test_provider_record_with_pricing(self):
        from fno.adapters.providers.model import Pricing, ProviderRecord

        record = ProviderRecord(
            id="claude-anthropic",
            name="Claude Anthropic Direct",
            cli="claude",
            auth="oauth_dir",
            credentials_source=Path("~/.claude"),
            pricing=Pricing(
                input_per_million_usd=15.0,
                output_per_million_usd=75.0,
            ),
        )
        assert record.pricing is not None
        assert record.pricing.input_per_million_usd == 15.0
        assert record.pricing.cache_read_per_million_usd is None

    def test_hp2_provider_record_without_pricing_defaults_none(self):
        from fno.adapters.providers.model import ProviderRecord

        record = ProviderRecord(
            id="claude-anthropic",
            name="No pricing set",
            cli="claude",
            auth="oauth_dir",
            credentials_source=Path("~/.claude"),
        )
        assert record.pricing is None

    def test_err1_negative_input_rate_rejected(self):
        from fno.adapters.providers.model import Pricing

        with pytest.raises(pydantic.ValidationError) as exc_info:
            Pricing(
                input_per_million_usd=-1.0,
                output_per_million_usd=75.0,
            )
        # Field name appears in the error
        msg = str(exc_info.value)
        assert "input_per_million_usd" in msg

    def test_err1b_negative_output_rate_rejected(self):
        from fno.adapters.providers.model import Pricing

        with pytest.raises(pydantic.ValidationError):
            Pricing(input_per_million_usd=15.0, output_per_million_usd=-1.0)

    def test_err1c_negative_cache_rate_rejected(self):
        from fno.adapters.providers.model import Pricing

        with pytest.raises(pydantic.ValidationError):
            Pricing(
                input_per_million_usd=15.0,
                output_per_million_usd=75.0,
                cache_read_per_million_usd=-0.5,
            )

    def test_err1d_missing_required_field_rejected(self):
        from fno.adapters.providers.model import Pricing

        with pytest.raises(pydantic.ValidationError):
            Pricing(input_per_million_usd=15.0)  # type: ignore[call-arg]

    def test_edge1_cache_rates_optional(self):
        from fno.adapters.providers.model import Pricing

        p = Pricing(input_per_million_usd=15.0, output_per_million_usd=75.0)
        assert p.cache_read_per_million_usd is None
        assert p.cache_write_per_million_usd is None

    def test_edge2_pricing_round_trips_via_dict(self):
        """Cites what-if finding #9. Pricing must serialize to a dict that can
        round-trip through settings.yaml without losing precision so the
        per-segment cost math (Spec 2.5) has the right number on both sides."""
        from fno.adapters.providers.model import Pricing

        p = Pricing(
            input_per_million_usd=15.0,
            output_per_million_usd=75.0,
            cache_read_per_million_usd=1.5,
            cache_write_per_million_usd=18.75,
        )
        d = p.model_dump()
        assert d["input_per_million_usd"] == 15.0
        assert d["output_per_million_usd"] == 75.0
        assert d["cache_read_per_million_usd"] == 1.5
        assert d["cache_write_per_million_usd"] == 18.75

        p2 = Pricing.model_validate(d)
        assert p2 == p

    def test_provider_record_pricing_validates_via_model(self):
        """Loading from dict (as the loader does from yaml) must run the
        Pricing validator too, not just on direct construction."""
        from fno.adapters.providers.model import ProviderRecord

        record = ProviderRecord.model_validate({
            "id": "claude-anthropic",
            "name": "Claude Anthropic Direct",
            "cli": "claude",
            "auth": "oauth_dir",
            "credentials_source": "~/.claude",
            "pricing": {
                "input_per_million_usd": 15.0,
                "output_per_million_usd": 75.0,
            },
        })
        assert record.pricing is not None
        assert record.pricing.output_per_million_usd == 75.0

        with pytest.raises(pydantic.ValidationError):
            ProviderRecord.model_validate({
                "id": "claude-anthropic",
                "name": "Bad pricing",
                "cli": "claude",
                "auth": "oauth_dir",
                "credentials_source": "~/.claude",
                "pricing": {
                    "input_per_million_usd": -1.0,
                    "output_per_million_usd": 75.0,
                },
            })


class TestCostCapPerSession:
    """Phase 03 task 3.2: per-provider sub-cap on settings.yaml."""

    def test_cost_cap_usd_per_session_defaults_none(self):
        from fno.adapters.providers.model import ProviderRecord

        record = ProviderRecord(
            id="x", name="X", cli="claude", auth="oauth_dir",
            credentials_source=Path("~/.claude"),
        )
        assert record.cost_cap_usd_per_session is None

    def test_cost_cap_usd_per_session_accepts_positive_float(self):
        from fno.adapters.providers.model import ProviderRecord

        record = ProviderRecord(
            id="x", name="X", cli="claude", auth="oauth_dir",
            credentials_source=Path("~/.claude"),
            cost_cap_usd_per_session=30.0,
        )
        assert record.cost_cap_usd_per_session == 30.0

    def test_cost_cap_usd_per_session_rejects_negative(self):
        import pydantic
        from fno.adapters.providers.model import ProviderRecord

        with pytest.raises(pydantic.ValidationError):
            ProviderRecord(
                id="x", name="X", cli="claude", auth="oauth_dir",
                credentials_source=Path("~/.claude"),
                cost_cap_usd_per_session=-5.0,
            )
