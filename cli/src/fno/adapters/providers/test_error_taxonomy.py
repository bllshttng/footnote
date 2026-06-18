"""Tests for the provider error taxonomy normalizer.

Run: cd cli && uv run pytest src/fno/adapters/providers/test_error_taxonomy.py -v

Phase 01 of the provider rotation failover spec (ab-9728b70b). The
normalizer is the single classifier consulted by the failover controller
to decide swap-or-surface for any provider call.
"""
from __future__ import annotations

import pytest

from fno.adapters.providers.error_taxonomy import (
    ERROR_RULES,
    ErrorClass,
    ErrorRule,
    NormalizedError,
    classify_error,
    normalize,
)


class TestSwapTriggers:
    """5XX, 4XX-auth, 4XX-quota all set triggers_swap=True."""

    def test_hp1_529_overloaded_classifies_provider_5xx(self) -> None:
        result = normalize(http_status=529, exit_code=None, body="")
        assert result.error_class is ErrorClass.PROVIDER_5XX
        assert result.triggers_swap is True
        assert result.raw_status == 529

    def test_500_classifies_provider_5xx(self) -> None:
        result = normalize(http_status=500, exit_code=None, body="server error")
        assert result.error_class is ErrorClass.PROVIDER_5XX
        assert result.triggers_swap is True

    def test_502_503_504_all_classify_provider_5xx(self) -> None:
        for status in (502, 503, 504):
            result = normalize(http_status=status, exit_code=None, body="")
            assert result.error_class is ErrorClass.PROVIDER_5XX, status
            assert result.triggers_swap is True, status

    def test_err2_401_empty_body_classifies_4xx_auth(self) -> None:
        result = normalize(http_status=401, exit_code=None, body="")
        assert result.error_class is ErrorClass.PROVIDER_4XX_AUTH
        assert result.triggers_swap is True

    def test_403_classifies_4xx_auth(self) -> None:
        result = normalize(http_status=403, exit_code=None, body="forbidden")
        assert result.error_class is ErrorClass.PROVIDER_4XX_AUTH
        assert result.triggers_swap is True

    def test_402_classifies_4xx_quota(self) -> None:
        result = normalize(http_status=402, exit_code=None, body="payment required")
        assert result.error_class is ErrorClass.PROVIDER_4XX_QUOTA
        assert result.triggers_swap is True

    def test_429_classifies_4xx_quota(self) -> None:
        result = normalize(http_status=429, exit_code=None, body="")
        assert result.error_class is ErrorClass.PROVIDER_4XX_QUOTA
        assert result.triggers_swap is True

    def test_hp2_200_with_rate_limit_body_classifies_4xx_quota(self) -> None:
        body = '{"error":{"type":"overloaded_error","message":"rate limit exceeded for org_..."}}'
        result = normalize(http_status=200, exit_code=None, body=body)
        assert result.error_class is ErrorClass.PROVIDER_4XX_QUOTA
        assert result.triggers_swap is True

    def test_200_with_quota_exceeded_body_classifies_4xx_quota(self) -> None:
        body = '{"error":"quota exceeded"}'
        result = normalize(http_status=200, exit_code=None, body=body)
        assert result.error_class is ErrorClass.PROVIDER_4XX_QUOTA
        assert result.triggers_swap is True

    def test_body_match_is_case_insensitive(self) -> None:
        for body in ("RATE LIMIT exceeded", "Quota Exceeded", "Rate Limit",
                      "QUOTA EXCEEDED"):
            result = normalize(http_status=200, exit_code=None, body=body)
            assert result.error_class is ErrorClass.PROVIDER_4XX_QUOTA, body
            assert result.triggers_swap is True, body


class TestNoSwap:
    """Parser errors and unknown errors do not trigger swap."""

    def test_err1_200_with_html_body_classifies_parser_error(self) -> None:
        result = normalize(
            http_status=200,
            exit_code=None,
            body="<html><body>Server Error</body></html>",
        )
        assert result.error_class is ErrorClass.PARSER_ERROR
        assert result.triggers_swap is False

    def test_200_with_clean_body_no_match_is_unknown(self) -> None:
        # 200 + no parser hint + no quota body = UNKNOWN, NOT PARSER_ERROR.
        # PARSER_ERROR specifically means "the body looked malformed for the
        # parser." A clean 200 with unexpected shape is UNKNOWN.
        result = normalize(
            http_status=200,
            exit_code=None,
            body='{"unexpected":"shape"}',
        )
        assert result.error_class is ErrorClass.UNKNOWN
        assert result.triggers_swap is False

    def test_404_classifies_unknown(self) -> None:
        result = normalize(http_status=404, exit_code=None, body="")
        assert result.error_class is ErrorClass.UNKNOWN
        assert result.triggers_swap is False

    def test_400_classifies_unknown(self) -> None:
        # 400 is bad-request from the caller side, not a swap signal.
        result = normalize(http_status=400, exit_code=None, body="bad input")
        assert result.error_class is ErrorClass.UNKNOWN
        assert result.triggers_swap is False


class TestSubprocessExitCodes:
    """Subprocess exits map onto the same taxonomy."""

    def test_subprocess_exit_zero_with_quota_stderr_is_4xx_quota(self) -> None:
        result = normalize(
            http_status=None,
            exit_code=0,
            body="rate limit exceeded",
        )
        assert result.error_class is ErrorClass.PROVIDER_4XX_QUOTA
        assert result.triggers_swap is True

    def test_subprocess_nonzero_exit_no_match_is_unknown(self) -> None:
        result = normalize(http_status=None, exit_code=1, body="something else")
        assert result.error_class is ErrorClass.UNKNOWN
        assert result.triggers_swap is False
        assert result.raw_exit_code == 1


class TestNormalizedErrorShape:
    """The result dataclass carries forensic fields."""

    def test_body_excerpt_truncates_to_256_chars(self) -> None:
        long_body = "x" * 1024
        result = normalize(http_status=500, exit_code=None, body=long_body)
        assert len(result.body_excerpt) == 256
        assert result.body_excerpt == "x" * 256

    def test_body_excerpt_unchanged_when_short(self) -> None:
        result = normalize(http_status=500, exit_code=None, body="short")
        assert result.body_excerpt == "short"

    def test_normalized_error_is_frozen(self) -> None:
        result = normalize(http_status=500, exit_code=None, body="")
        with pytest.raises((AttributeError, Exception)):
            result.error_class = ErrorClass.UNKNOWN  # type: ignore[misc]

    def test_raw_status_preserved(self) -> None:
        result = normalize(http_status=529, exit_code=None, body="")
        assert result.raw_status == 529
        assert result.raw_exit_code is None

    def test_raw_exit_code_preserved(self) -> None:
        result = normalize(http_status=None, exit_code=42, body="x")
        assert result.raw_exit_code == 42
        assert result.raw_status is None


class TestFailureModeCitations:
    """Edge cases tied to the what-if findings."""

    def test_edge1_stale_credential_returns_401_classifies_auth(self) -> None:
        # Cites what-if finding #6: "Auth-mismatch cascade from non-atomic
        # credential swap." A stale cred causes a 401 from the provider;
        # downstream callers must know to swap, not retry on the same
        # provider with the same creds.
        result = normalize(http_status=401, exit_code=None, body="invalid api key")
        assert result.error_class is ErrorClass.PROVIDER_4XX_AUTH
        assert result.triggers_swap is True

    def test_edge2_openrouter_wrapper_envelope_classifies_parser(self) -> None:
        # Cites what-if finding #13: "OpenRouter response wrapper crashes
        # Anthropic-shaped parser." 200 OK with an unexpected envelope.
        # Healthy provider; bad parser; do NOT exhaust the rotation queue.
        body = '{"choices":[{"message":{"content":"hi"}}]}'  # OpenAI shape
        result = normalize(
            http_status=200,
            exit_code=None,
            body=body,
            parser_failed=True,
        )
        assert result.error_class is ErrorClass.PARSER_ERROR
        assert result.triggers_swap is False


class TestNormalizedErrorModelField:
    """Plan A1 (ab-7fe3cdaf): NormalizedError.model carries model identifier."""

    def test_ac5_1_model_defaults_to_none(self) -> None:
        # AC5.1-FR: existing call sites that don't pass model produce
        # NormalizedError with model=None.
        result = normalize(http_status=500, exit_code=None, body="boom")
        assert result.model is None

    def test_model_passthrough(self) -> None:
        result = normalize(
            http_status=429, exit_code=None, body="rate limit",
            model="claude-opus-4-7",
        )
        assert result.model == "claude-opus-4-7"
        assert result.triggers_swap is True

    def test_ac5_2_long_model_id_truncated(self) -> None:
        # AC5.2-EDGE: producer truncates to 256 bytes before construction.
        huge = "x" * 1024
        result = normalize(
            http_status=429, exit_code=None, body="quota exceeded",
            model=huge,
        )
        assert result.model is not None
        assert len(result.model) == 256

    def test_model_short_unchanged(self) -> None:
        result = normalize(
            http_status=429, exit_code=None, body="rate limit",
            model="gpt-4",
        )
        assert result.model == "gpt-4"

    def test_model_hand_constructed_default(self) -> None:
        # Hand-constructed NormalizedError defaults model to None (backward
        # compat for any existing fixture).
        e = NormalizedError(
            error_class=ErrorClass.PROVIDER_5XX,
            raw_status=500,
            raw_exit_code=None,
            body_excerpt="",
            triggers_swap=True,
        )
        assert e.model is None

    def test_rejects_empty_model_string(self) -> None:
        # Closes producer-consumer gap: empty model would later raise
        # ValueError from ProviderHealth.__post_init__ inside the
        # failover swap path. Reject at the producer boundary.
        with pytest.raises(ValueError, match="non-empty"):
            NormalizedError(
                error_class=ErrorClass.PROVIDER_4XX_QUOTA,
                raw_status=429,
                raw_exit_code=None,
                body_excerpt="",
                triggers_swap=True,
                model="",
            )

    def test_normalize_with_empty_string_model_falls_through_to_none(
        self,
    ) -> None:
        # Defense in depth: normalize() truncates but doesn't auto-reject
        # empty strings. If a future caller passes model="" we want the
        # ValueError to surface (CI catches it) rather than silently
        # producing an invalid lock key downstream.
        with pytest.raises(ValueError):
            normalize(
                http_status=429, exit_code=None, body="rate limit",
                model="",
            )


class TestPostInitInvariants:
    """Sigma-review hardening: NormalizedError catches mismatched
    triggers_swap at construction so a hand-built test fixture can't
    silently lie to the failover controller."""

    def test_inconsistent_triggers_swap_raises(self) -> None:
        # error_class implies triggers_swap=True; passing False is wrong.
        with pytest.raises(ValueError, match="triggers_swap"):
            NormalizedError(
                error_class=ErrorClass.PROVIDER_5XX,
                raw_status=529,
                raw_exit_code=None,
                body_excerpt="",
                triggers_swap=False,
            )

    def test_inconsistent_no_swap_raises(self) -> None:
        # PARSER_ERROR implies triggers_swap=False; passing True is wrong.
        with pytest.raises(ValueError, match="triggers_swap"):
            NormalizedError(
                error_class=ErrorClass.PARSER_ERROR,
                raw_status=200,
                raw_exit_code=None,
                body_excerpt="",
                triggers_swap=True,
            )

    def test_consistent_construction_succeeds(self) -> None:
        # Sanity: the consistent combination still works.
        ok = NormalizedError(
            error_class=ErrorClass.PROVIDER_5XX,
            raw_status=529,
            raw_exit_code=None,
            body_excerpt="",
            triggers_swap=True,
        )
        assert ok.triggers_swap is True


class TestErrorRuleConstructor:
    """ErrorRule must reject ill-formed combinations.

    AC1.3-EDGE: design-doc Failure Modes Errors locks "must reject
    ErrorRule with both text and status set" (and symmetrically both
    cooldown_ms and backoff). The constructor enforces these via
    __post_init__.
    """

    def test_rejects_both_text_and_status(self) -> None:
        with pytest.raises(ValueError, match="text or status"):
            ErrorRule(text="rate limit", status=429, backoff=True)

    def test_rejects_neither_text_nor_status(self) -> None:
        with pytest.raises(ValueError, match="text or status"):
            ErrorRule(backoff=True)

    def test_rejects_both_cooldown_and_backoff(self) -> None:
        with pytest.raises(ValueError, match="cooldown_ms or backoff"):
            ErrorRule(text="rate limit", cooldown_ms=5000, backoff=True)

    def test_rejects_neither_cooldown_nor_backoff(self) -> None:
        with pytest.raises(ValueError, match="cooldown_ms or backoff"):
            ErrorRule(text="rate limit")

    def test_text_with_cooldown_constructs(self) -> None:
        rule = ErrorRule(text="no credentials", cooldown_ms=120_000)
        assert rule.text == "no credentials"
        assert rule.status is None
        assert rule.cooldown_ms == 120_000
        assert rule.backoff is False

    def test_status_with_backoff_constructs(self) -> None:
        rule = ErrorRule(status=429, backoff=True)
        assert rule.status == 429
        assert rule.text is None
        assert rule.cooldown_ms is None
        assert rule.backoff is True

    def test_rejects_zero_cooldown_ms(self) -> None:
        # Zero cooldown silently disables the rule's wait; reject so a
        # misconfigured rule fails loudly at construction time.
        with pytest.raises(ValueError, match="cooldown_ms must be positive"):
            ErrorRule(text="rate limit", cooldown_ms=0)

    def test_rejects_negative_cooldown_ms(self) -> None:
        with pytest.raises(ValueError, match="cooldown_ms must be positive"):
            ErrorRule(status=429, cooldown_ms=-1)


class TestErrorRulesConstant:
    """ERROR_RULES is the priority-ordered list ported from 9router."""

    def test_text_rules_appear_before_status_rules(self) -> None:
        # The classify_error walk relies on this ordering: any text rule
        # must precede every status rule in the tuple.
        last_text_index = -1
        first_status_index = len(ERROR_RULES)
        for i, rule in enumerate(ERROR_RULES):
            if rule.text is not None:
                last_text_index = i
            elif rule.status is not None and i < first_status_index:
                first_status_index = i
        assert last_text_index < first_status_index, (
            "text rules must precede status rules in ERROR_RULES"
        )

    def test_canonical_text_rules_present(self) -> None:
        text_rules = {r.text: r for r in ERROR_RULES if r.text is not None}
        # Sample of 9router-derived rules; full coverage in port verbatim.
        assert "rate limit" in text_rules
        assert text_rules["rate limit"].backoff is True
        assert "quota exceeded" in text_rules
        assert text_rules["quota exceeded"].backoff is True
        assert "no credentials" in text_rules
        assert text_rules["no credentials"].cooldown_ms == 120_000

    def test_canonical_status_rules_present(self) -> None:
        status_rules = {r.status: r for r in ERROR_RULES if r.status is not None}
        assert 429 in status_rules
        assert status_rules[429].backoff is True
        assert 401 in status_rules
        assert status_rules[401].cooldown_ms == 120_000


class TestClassifyError:
    """classify_error walks ERROR_RULES top-to-bottom and returns the first match."""

    def test_hp_overloaded_capacity_text_match(self) -> None:
        # AC1.1-HP: 200 with body "overloaded due to capacity" matches
        # the text rule for "capacity" via priority-1 walk.
        rule = classify_error(200, "overloaded due to capacity")
        assert rule is not None
        assert rule.text == "capacity"
        assert rule.backoff is True

    def test_err_text_wins_over_status_503_with_rate_limit_body(self) -> None:
        # AC1.2-ERR: 503 with body "rate limit exceeded" must match the
        # text rule for "rate limit" BEFORE any status fallback.
        rule = classify_error(503, "rate limit exceeded")
        assert rule is not None
        assert rule.text == "rate limit"
        assert rule.status is None

    def test_edge_status_only_no_text_match(self) -> None:
        # AC1.4-EDGE: 503 with body that has no text-rule match; no rule
        # for plain 503 exists, so classify_error returns None. The
        # existing PROVIDER_5XX taxonomy classification still fires via
        # the separate `normalize()` path.
        rule = classify_error(503, "internal server error")
        assert rule is None

    def test_fr_none_body_skips_text_rules_falls_to_status(self) -> None:
        # AC1.5-FR: 429 with no body (None) skips text rules, matches
        # the status-429 rule via fallback.
        rule = classify_error(429, None)
        assert rule is not None
        assert rule.status == 429
        assert rule.backoff is True

    def test_case_insensitive_text_match(self) -> None:
        rule = classify_error(200, "RATE LIMIT EXCEEDED")
        assert rule is not None
        assert rule.text == "rate limit"

    def test_no_match_returns_none(self) -> None:
        rule = classify_error(200, "OK")
        assert rule is None

    def test_status_only_401_match(self) -> None:
        rule = classify_error(401, "unauthorized")
        assert rule is not None
        assert rule.status == 401
        assert rule.cooldown_ms == 120_000

    def test_priority_ordering_three_cases(self) -> None:
        # AC4.1: at least three priority-ordering test cases.
        # 1. text-wins-over-status
        r1 = classify_error(429, "quota exceeded")
        assert r1 is not None and r1.text == "quota exceeded"
        # 2. no-text-falls-to-status
        r2 = classify_error(429, "unrelated body")
        assert r2 is not None and r2.status == 429
        # 3. status-only (None body)
        r3 = classify_error(401, None)
        assert r3 is not None and r3.status == 401

    def test_substring_match_within_larger_body(self) -> None:
        body = (
            "Provider returned the following error: too many requests "
            "have been issued in this window. Please retry later."
        )
        rule = classify_error(200, body)
        assert rule is not None
        assert rule.text == "too many requests"
