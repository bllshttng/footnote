"""Error taxonomy normalizer for provider failover.

Phase 01 of the provider rotation failover spec (ab-9728b70b). Single
classifier: maps a provider call's outcome (HTTP status + body, or CLI
subprocess exit code + stderr) to a structured ``NormalizedError`` so the
failover controller can decide swap vs surface vs retry.

The taxonomy is intentionally fixed (see plan: "Error taxonomy is fixed"
in Spec 1's Locked Decisions). New error classes require a spec update,
not a code-only patch.
"""
from __future__ import annotations

from dataclasses import dataclass
from enum import Enum

_BODY_EXCERPT_LEN = 256

# Body-string matches that turn an HTTP-200 (or successful subprocess) into
# a swap-triggering quota error. Match is case-insensitive and matches
# anywhere in the excerpt.
# "usage limit" is the claude CLI's own exhaustion phrasing ("Claude usage limit
# reached"), distinct from the API-path "rate limit" body; without it a claude bg
# worker dying on its usage limit classified UNKNOWN and got nudged at the dead
# provider instead of triggering a multi-account auto-switch (US3).
_QUOTA_BODY_MARKERS = ("rate limit", "quota exceeded", "usage limit")

# Status codes that classify as PROVIDER_5XX swap triggers. 529 is
# Anthropic's "overloaded" non-standard code; we treat it as a 5XX.
_PROVIDER_5XX_STATUSES = frozenset({500, 502, 503, 504, 529})

# Auth/credential status codes - swap triggers because creds are bound to
# the provider record and a swap might land on a working set.
_PROVIDER_4XX_AUTH_STATUSES = frozenset({401, 403})

# Quota / rate-limit status codes - swap triggers (give the next provider
# a turn while this one cools off).
_PROVIDER_4XX_QUOTA_STATUSES = frozenset({402, 429})


class ErrorClass(str, Enum):
    """Closed taxonomy of normalized provider call outcomes."""

    PROVIDER_5XX = "provider_5xx"
    PROVIDER_4XX_AUTH = "provider_4xx_auth"
    PROVIDER_4XX_QUOTA = "provider_4xx_quota"
    PARSER_ERROR = "parser_error"
    UNKNOWN = "unknown"


_SWAP_TRIGGER_CLASSES = frozenset({
    "provider_5xx",
    "provider_4xx_auth",
    "provider_4xx_quota",
})


_MODEL_ID_MAX_LEN = 256


@dataclass(frozen=True)
class NormalizedError:
    """Structured outcome the failover controller consumes.

    ``triggers_swap`` is convenience: True iff ``error_class`` is one of
    PROVIDER_5XX, PROVIDER_4XX_AUTH, PROVIDER_4XX_QUOTA. Callers should
    branch on this bit, not re-derive it from the enum.

    The ``__post_init__`` guard validates that ``triggers_swap`` matches
    the taxonomy so a hand-constructed instance (e.g., a test fixture)
    can't lie to the failover controller.

    ``model`` (Plan A1, ab-7fe3cdaf) is the optional model identifier
    that errored - lets downstream code (``update_provider_health``,
    ``is_in_cooldown``) lock only that model rather than the whole
    provider record. None when the caller doesn't know which model the
    request targeted; backward-compat with all Plan A call sites.
    Producers are responsible for clamping to ``_MODEL_ID_MAX_LEN``
    before construction (symmetric with ``body_excerpt`` truncation in
    ``normalize``).
    """

    error_class: ErrorClass
    raw_status: int | None
    raw_exit_code: int | None
    body_excerpt: str
    triggers_swap: bool
    model: str | None = None

    def __post_init__(self) -> None:
        expected = self.error_class.value in _SWAP_TRIGGER_CLASSES
        if self.triggers_swap != expected:
            raise ValueError(
                f"NormalizedError.triggers_swap={self.triggers_swap} "
                f"inconsistent with error_class={self.error_class.value} "
                f"(expected {expected})"
            )
        # Close the producer-consumer gap: an empty model id would
        # later raise ValueError from ProviderHealth.__post_init__
        # deep inside the failover swap path (whose `try/except` is
        # narrowed to OSError/JSONDecodeError per the narrow-catch
        # contract). Reject at construction so the error surfaces in
        # CI and the failover swap stays robust against future
        # producers that pass model="" by accident.
        if self.model is not None and not self.model:
            raise ValueError(
                "NormalizedError.model must be a non-empty string when set"
            )


def _matches_quota_body(body: str) -> bool:
    lowered = body.lower()
    return any(marker in lowered for marker in _QUOTA_BODY_MARKERS)


def _classify(
    http_status: int | None,
    body: str,
    parser_failed: bool,
) -> ErrorClass:
    if http_status in _PROVIDER_5XX_STATUSES:
        return ErrorClass.PROVIDER_5XX
    if http_status in _PROVIDER_4XX_AUTH_STATUSES:
        return ErrorClass.PROVIDER_4XX_AUTH
    if http_status in _PROVIDER_4XX_QUOTA_STATUSES:
        return ErrorClass.PROVIDER_4XX_QUOTA

    body_says_quota = _matches_quota_body(body)
    if body_says_quota:
        return ErrorClass.PROVIDER_4XX_QUOTA

    if parser_failed:
        return ErrorClass.PARSER_ERROR

    if http_status == 200 and body.lstrip().startswith("<"):
        # Unparseable HTML where JSON was expected - classic upstream error
        # page rendered through a 200. The detection is a cheap heuristic;
        # callers that have actually attempted to parse should pass
        # ``parser_failed=True`` instead of relying on this.
        return ErrorClass.PARSER_ERROR

    return ErrorClass.UNKNOWN


# Plan A (ab-6534a78a): priority-ordered ErrorRule list ported from
# 9router (~/code/tools/9router/open-sse/config/errorConfig.js:59-76).
# These rules are SUPPLEMENTARY to the closed ErrorClass taxonomy: they
# produce the COOLDOWN-shaping rule (fixed cooldown_ms vs exponential
# backoff), not a new ErrorClass. The classifier walks ERROR_RULES
# top-to-bottom; text rules (priority 1) match the response body
# case-insensitively; status rules (priority 2) match the HTTP status
# exactly. First match wins.

# Cooldown bands borrowed from 9router. LONG = auth/credential errors
# (operator must intervene); SHORT = transient request-shape problems
# (next call from a different provider may succeed immediately).
COOLDOWN_LONG_MS = 2 * 60 * 1000  # 2 min
COOLDOWN_SHORT_MS = 5 * 1000  # 5 s


@dataclass(frozen=True)
class ErrorRule:
    """A single rule in the priority-ordered ERROR_RULES list.

    Exactly one of (text, status) must be set; exactly one of
    (cooldown_ms, backoff) must be set. ``__post_init__`` enforces this
    so a hand-constructed rule cannot ship a contradictory shape.
    """

    text: str | None = None
    status: int | None = None
    cooldown_ms: int | None = None
    backoff: bool = False

    def __post_init__(self) -> None:
        if (self.text is None) == (self.status is None):
            raise ValueError(
                "ErrorRule requires exactly one of text or status"
            )
        if (self.cooldown_ms is None) == (not self.backoff):
            raise ValueError(
                "ErrorRule requires exactly one of cooldown_ms or backoff"
            )
        # cooldown_ms=0 silently produces a no-op cooldown; reject so a
        # misconfigured rule fails loudly at construction.
        if self.cooldown_ms is not None and self.cooldown_ms <= 0:
            raise ValueError(
                f"ErrorRule.cooldown_ms must be positive, got {self.cooldown_ms}"
            )


ERROR_RULES: tuple[ErrorRule, ...] = (
    # Text-based rules (priority 1: substring match, case-insensitive).
    ErrorRule(text="no credentials", cooldown_ms=COOLDOWN_LONG_MS),
    ErrorRule(text="request not allowed", cooldown_ms=COOLDOWN_SHORT_MS),
    ErrorRule(text="improperly formed request", cooldown_ms=COOLDOWN_LONG_MS),
    ErrorRule(text="rate limit", backoff=True),
    ErrorRule(text="too many requests", backoff=True),
    ErrorRule(text="quota exceeded", backoff=True),
    ErrorRule(text="capacity", backoff=True),
    ErrorRule(text="overloaded", backoff=True),
    # Status-based rules (priority 2: HTTP status fallback).
    ErrorRule(status=401, cooldown_ms=COOLDOWN_LONG_MS),
    ErrorRule(status=402, cooldown_ms=COOLDOWN_LONG_MS),
    ErrorRule(status=403, cooldown_ms=COOLDOWN_LONG_MS),
    ErrorRule(status=404, cooldown_ms=COOLDOWN_LONG_MS),
    ErrorRule(status=429, backoff=True),
)


def classify_error(
    status: int | None,
    body: str | None,
) -> ErrorRule | None:
    """Return the first matching ErrorRule from ERROR_RULES.

    Walks the rules top-to-bottom. Text rules: case-insensitive
    substring match against ``body`` (skipped silently when body is
    None). Status rules: exact ``status`` equality. First match wins.
    Returns None when no rule matches; callers should fall back to the
    existing ``normalize()`` taxonomy classification.

    This function is supplementary to ``normalize()``; it shapes COOLDOWN
    behavior but does not produce an ErrorClass.
    """
    body_lower = body.lower() if body is not None else None
    for rule in ERROR_RULES:
        if rule.text is not None:
            if body_lower is None:
                continue
            if rule.text in body_lower:
                return rule
        elif rule.status is not None:
            if status == rule.status:
                return rule
    return None


def normalize(
    http_status: int | None,
    exit_code: int | None,
    body: str,
    *,
    parser_failed: bool = False,
    model: str | None = None,
) -> NormalizedError:
    """Classify a provider call outcome.

    Args:
        http_status: HTTP response code, or None if the subprocess never
            reached HTTP (e.g., transport error before connect).
        exit_code: CLI subprocess exit code, or None if the call was a
            direct HTTP request.
        body: Response body or stderr text. Truncated to 256 chars in the
            returned ``body_excerpt``.
        parser_failed: True iff the caller already tried to parse the body
            with the provider's expected schema and the parser raised. This
            is the authoritative PARSER_ERROR signal; the body-shape
            heuristic is a fallback for direct callers that didn't attempt
            a parse.
        model: Optional model identifier (Plan A1, ab-7fe3cdaf). When
            provided, plumbed through ``NormalizedError.model`` so the
            failover controller can write a model-specific lock instead
            of a provider-level one. Clamped to 256 bytes before
            construction (symmetric with ``body_excerpt`` truncation).

    Returns:
        ``NormalizedError`` with ``error_class`` set per the taxonomy and
        ``triggers_swap`` derived from it.
    """
    error_class = _classify(http_status, body, parser_failed)
    triggers_swap = error_class in {
        ErrorClass.PROVIDER_5XX,
        ErrorClass.PROVIDER_4XX_AUTH,
        ErrorClass.PROVIDER_4XX_QUOTA,
    }
    clamped_model = (
        model[:_MODEL_ID_MAX_LEN] if isinstance(model, str) else model
    )
    return NormalizedError(
        error_class=error_class,
        raw_status=http_status,
        raw_exit_code=exit_code,
        body_excerpt=body[:_BODY_EXCERPT_LEN],
        triggers_swap=triggers_swap,
        model=clamped_model,
    )
