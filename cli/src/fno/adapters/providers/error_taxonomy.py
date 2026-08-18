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

import re
from dataclasses import dataclass
from datetime import datetime
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

# A refusal body carries WHEN the window reopens, and today that number is
# thrown away: the 429 rule is exponential backoff, so a nine-hour cap wrote a
# 2000ms lock and every reroute path was free to route straight back in.
#
# Three shapes, tried in this order. An offset-bearing stamp and an epoch are
# unambiguous and parse on their own. A naive stamp does NOT: the z.ai stamps
# are Singapore time and the claude weekly reset was quoted Pacific, and being
# wrong by eight hours either unlocks a capped provider early or locks a healthy
# one out for an extra window. So a naive stamp needs an explicit
# ``accounts.<id>.reset_timezone`` and returns None without one. None means
# "keep today's backoff", which is the current behavior, so refusing costs
# nothing and guessing costs a window.
_ISO_OFFSET_STAMP_RE = re.compile(
    r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}(?::\d{2})?(?:\.\d+)?"
    r"(?:Z|[+-]\d{2}:?\d{2})"
)
_ISO_NAIVE_STAMP_RE = re.compile(
    r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}(?::\d{2})?(?:\.\d+)?"
)
# An epoch is read only when a reset-shaped key names it. A bare ten-digit
# number in a refusal body is as likely to be a request id as a timestamp.
# The trailing (?!\d) is load-bearing: without it a MILLISECOND epoch (the
# normal shape for an X-RateLimit-Reset field) is truncated to its first
# eleven digits, and 1786000000000 reads as the year 2535.
_EPOCH_STAMP_RE = re.compile(
    r"""reset[a-zA-Z_]*["'\s:=]+(\d{9,13})(?!\d)""",
)

# The furthest ahead a rate-limit window is allowed to reopen. Every real one
# is hours to a week; a claude weekly limit is the longest measured.
#
# This bound is what keeps a misread stamp from becoming a permanent outage.
# The lock it feeds is read with NO TTL, so a stamp that resolves to next year
# takes the provider out until next year, and `link_is_exhausted` then drops
# the whole harness from the fallback chain with it. Two ways in: a
# millisecond epoch read as seconds, and an offset-bearing stamp in the body
# that is not a reset at all (a token expiry, a subscription renewal, a trial
# end date). Both are far-future by construction, so one ceiling closes both.
# A refused stamp falls back to today's backoff, which is exactly the behavior
# that shipped before this feature.
_MAX_RESET_HORIZON_S = 14 * 24 * 3600


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

    ``resets_at`` is the epoch the refusal body says the window reopens,
    harvested by :func:`_parse_reset_stamp`. It is the number the whole
    provider-cap failover turns on: without it a nine-hour cap writes a
    2000ms backoff lock and every reroute path routes back into the
    provider that just refused. None means "no reset was readable", and
    every consumer must then keep its existing backoff arithmetic.

    ``reset_stamp_unparsed`` is the stamp that WAS present and could not
    be resolved to an epoch - a naive stamp with no configured timezone.
    It exists so a refusal can say "I saw a time and refused to guess it"
    rather than being indistinguishable from a body with no time at all.

    Both fields are optional, following the ``model`` field's precedent:
    the closed part of this taxonomy is the ErrorClass set, not the
    dataclass shape.
    """

    error_class: ErrorClass
    raw_status: int | None
    raw_exit_code: int | None
    body_excerpt: str
    triggers_swap: bool
    model: str | None = None
    resets_at: float | None = None
    reset_stamp_unparsed: str | None = None

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


def _parse_reset_stamp(
    body: str, tz: str | None
) -> tuple[float | None, str | None]:
    """``(epoch, unparsed_stamp)`` for the reset time named in ``body``.

    Exactly one of the two is ever non-None. An offset-bearing ISO-8601 stamp
    and a reset-keyed epoch resolve on their own. A naive stamp resolves only
    against an explicit ``tz`` (IANA name, from ``accounts.<id>.reset_timezone``)
    and is otherwise returned as the unparsed half - Locked Decision 6: a
    wrong-by-eight-hours reset either unlocks a capped provider early or locks a
    healthy one out for an extra window, and both are worse than falling back to
    the existing backoff.

    An unknown timezone name is the same refusal as no timezone at all.
    """
    if not body or not isinstance(body, str):
        return None, None
    import time as _time

    from fno.adapters.providers.usage import _iso_to_epoch

    def _sane(epoch: float | None) -> float | None:
        """Drop a reset further out than any real rate-limit window.

        Returns None rather than the value, so the caller falls back to the
        existing backoff. Silence beats a lock that outlives the outage.
        """
        if epoch is None:
            return None
        return epoch if epoch <= _time.time() + _MAX_RESET_HORIZON_S else None

    m = _ISO_OFFSET_STAMP_RE.search(body)
    if m is not None:
        return _sane(_iso_to_epoch(m.group(0))), None
    m = _EPOCH_STAMP_RE.search(body)
    if m is not None:
        raw = float(m.group(1))
        # A 13-digit value is milliseconds. Nothing else distinguishes the two
        # units, and reading ms as seconds lands five centuries out.
        return _sane(raw / 1000.0 if len(m.group(1)) > 11 else raw), None
    m = _ISO_NAIVE_STAMP_RE.search(body)
    if m is None:
        return None, None
    stamp = m.group(0)
    if not tz:
        return None, stamp
    try:
        from zoneinfo import ZoneInfo

        parsed = datetime.fromisoformat(stamp.replace(" ", "T"))
        resolved = _sane(parsed.replace(tzinfo=ZoneInfo(tz)).timestamp())
        return (resolved, None) if resolved is not None else (None, stamp)
    except Exception:  # noqa: BLE001 - a bad tz name refuses like a missing one
        return None, stamp


def reset_epoch_from(body: str | None, tz: str | None = None) -> float | None:
    """The reset epoch ``body`` names, or None. The one-call form.

    For the three code paths that write the provider lock. Each holds the
    refusal TEXT and the record id but no ``NormalizedError``, and each was
    writing a backoff step over a body that named the real answer. A shared
    helper rather than three parses, so a stamp shape added for one path lands
    on all three.
    """
    return _parse_reset_stamp(body or "", tz)[0]


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
    # Derived from _QUOTA_BODY_MARKERS rather than restated. Both lists answer
    # the same question - "is this body a quota exhaustion?" - and they had
    # already drifted: "usage limit", the claude CLI's own phrasing, reached the
    # normalize() taxonomy but never earned a cooldown here, so a worker that
    # died on its usage limit left that account eligible for the very next
    # dispatch. One list, so a marker added for one path lands on both.
    *(ErrorRule(text=marker, backoff=True) for marker in _QUOTA_BODY_MARKERS),
    ErrorRule(text="too many requests", backoff=True),
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
    reset_timezone: str | None = None,
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
        reset_timezone: IANA timezone name for this record, from
            ``accounts.<id>.reset_timezone``. Only consulted when the
            body carries a NAIVE reset stamp; an offset-bearing stamp
            and an epoch never need it. Absent, a naive stamp is
            refused rather than guessed (Locked Decision 6).

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
    resets_at, unparsed = _parse_reset_stamp(body, reset_timezone)
    return NormalizedError(
        error_class=error_class,
        raw_status=http_status,
        raw_exit_code=exit_code,
        body_excerpt=body[:_BODY_EXCERPT_LEN],
        triggers_swap=triggers_swap,
        model=clamped_model,
        resets_at=resets_at,
        reset_stamp_unparsed=unparsed,
    )
