//! When is a silent review bot nudged? Bot profiles, nudge configs and classes, and the giveup message.

use super::*;

/// Post a bot's review trigger to the PR once, returning true on success (
/// section 5). `FNO_LOOPCHECK_NO_COMMENT=1` suppresses the post so the test suite
/// never comments on a real PR, mirroring `FNO_LOOPCHECK_NO_NOTIFY`.
///
/// Idempotency is the PR itself, not a counter: this fires only on a NeedsNudge
/// classification, which means zero qualifying mentions exist within the wait
/// window - the same read every participant makes. A sibling worktree, a
/// `/fno:pr check` cron, a human, and a restarted-after-compaction session all
/// see the same PR and reach the same decision, so there is nothing to double.
pub(super) fn post_nudge_comment(
    gh_bin: &str,
    cwd: &Path,
    pr_number: i64,
    review_handle: &str,
) -> bool {
    if std::env::var("FNO_LOOPCHECK_NO_COMMENT").as_deref() == Ok("1") {
        return false;
    }
    let pr_arg = pr_number.to_string();
    matches!(
        bounded_read(
            gh_bin.as_ref(),
            &["pr", "comment", &pr_arg, "--body", review_handle],
            cwd,
            "nudge_comment",
            stopgate_read_timeout(),
        ),
        Ok(out) if out.status.success()
    )
}

/// The first missing bot that has been nudged to its ceiling and gone silent, if
/// any. The NoProgress backstop names it instead of a bare fingerprint streak.

pub(super) fn unresponsive_bot(pr: &PrInfo) -> Option<&BotNudge> {
    pr.bot_nudges
        .iter()
        .find(|n| n.class == NudgeClass::Unresponsive)
}

/// The commit-status context the merge ruleset requires. One const for
/// both emitter call sites and the standalone verb arm; the Python publisher
/// and refresher workflow pin the same name from their own surfaces, and a
/// context string that splits in two is a green marker on nothing.

/// The give-up line for an unresponsive nudged bot: the operator's
/// two questions ("will it finish, must I act") answered in one line.
pub(super) fn nudge_giveup_message(n: &BotNudge) -> String {
    format!(
        "{} did not review after {} nudges over {}m; giving up (NoProgress). \
         Move it to config.review.optional_apps or review by hand.",
        n.login, n.nudges, n.span_min
    )
}

/// Per-bot knowledge, login-keyed: the ONE table the review-gate code reads for
/// "what is this bot and how do we reach it". Replaces the scattered `KNOWN_BOTS`
/// membership list and the `USAGE_LIMIT_MARKERS` body-string list.
///
/// One bot wears three names and they are NOT interchangeable at the three sites
/// that use them:
///   - `login`         the review author, what `login_matches_bot` compares against
///   - `review_handle` what a PR comment must CONTAIN to trigger a fresh review
///   - `reply_handle`  what an in-thread reply must ADDRESS to reach the bot
/// A `github-app` reviewer that reviews on mention (not on push) is `nudgeable`:
/// footnote may post its `review_handle` to un-stick a required gate that nobody
/// mentioned. Nudge timing (`wait_minutes`, `ceiling`, `enabled`) is
/// config, not code - see `[review.nudge]` / `resolved_nudge_configs`.

pub(super) struct BotProfile {
    pub(super) login: &'static str,
    pub(super) review_handle: &'static str,
    pub(super) reply_handle: &'static str,
    /// ISSUE-comment body markers this bot posts when it is rate-limited and will
    /// never post a review object (PR #214). Empty for a bot never seen to do so.
    pub(super) usage_markers: &'static [&'static str],
    /// Other characterized ISSUE-comment bodies that mean the bot declined to
    /// review. Kept separate from quota markers because telemetry still names
    /// quota exhaustion specifically.
    pub(super) refusal_markers: &'static [&'static str],
    /// Body markers for a CLEAN pass this bot posts as a plain issue comment
    /// rather than a review object. Codex submits a formal review only when it
    /// has findings (measured on PR #947: the clean pass is the comment `Codex
    /// Review: Didn't find any major issues. Bravo. Reviewed commit: <sha>`),
    /// so without reading it a clean pass can never clear the gate and the
    /// gate is strictly easier to satisfy with a flawed PR than a clean one.
    /// Lowercased comparison; both apostrophe forms because the bot posts a
    /// typographic one and a human quoting it may not. Empty for a bot whose
    /// clean-pass shape was not measured - do not guess a marker.
    pub(super) clean_pass_markers: &'static [&'static str],
    pub(super) nudgeable: bool,
}

/// The shipped bot table. `chatgpt-codex-connector` is characterized from PR #618
/// (mention-triggered, ~4-7m latency, 5/5 mentions answered) and PR #947 (the
/// clean-pass comment); `gemini-code-assist` stays `nudgeable: false` with an
/// empty `review_handle` until its trigger is characterized (Evidence Gaps),
/// which is strictly more than the old lists knew.

pub(super) const BOT_PROFILES: &[BotProfile] = &[
    BotProfile {
        login: "chatgpt-codex-connector",
        review_handle: "@codex review",
        reply_handle: "@chatgpt-codex-connector",
        usage_markers: &["usage limits for code reviews", "codex usage limits"],
        refusal_markers: &["authentication failed"],
        clean_pass_markers: &[
            "didn't find any major issues",
            "didn\u{2019}t find any major issues",
        ],
        nudgeable: true,
    },
    BotProfile {
        login: "gemini-code-assist",
        review_handle: "",
        reply_handle: "@gemini-code-assist",
        usage_markers: &[],
        refusal_markers: &[],
        clean_pass_markers: &[],
        nudgeable: false,
    },
];

/// The profile for an actual review/comment AUTHOR login (may carry gh's `[bot]`
/// suffix or be the full login): the profile login is a substring of the author,
/// matching `login_matches_bot(author, profile.login)`. Used to reach a finding
/// author's `reply_handle`.

pub(super) fn profile_by_author(author: &str) -> Option<&'static BotProfile> {
    BOT_PROFILES
        .iter()
        .find(|p| login_matches_bot(author, p.login))
}

/// Two login strings name the same bot when either is a case-insensitive
/// substring of the other (so a config short name "codex", a full login, and a
/// "[bot]"-suffixed author all correspond). Symmetric superset of
/// `login_matches_bot`.

pub(super) fn logins_correspond(a: &str, b: &str) -> bool {
    login_matches_bot(a, b) || login_matches_bot(b, a)
}

/// Default nudge cadence. 15 minutes is the observed 6m55s worst-case
/// latency on PR #618 with headroom, not a guess; 3 nudges bounds the give-up at
/// ~45 minutes of *asked-for* waiting versus the unbounded budget burn today.

pub(super) const DEFAULT_NUDGE_WAIT_MINUTES: i64 = 15;

pub(super) const DEFAULT_NUDGE_CEILING: usize = 3;

/// Sanity ceilings for `[review.nudge]` override integers. A value
/// beyond these is a typo, not a cadence: `wait_minutes` is bounded well under
/// `i64::MAX/60` so `chrono::Duration::minutes` can never overflow-panic in the
/// stop gate, and a nudge cadence past a week / 1000 asks is meaningless anyway.

pub(super) const MAX_NUDGE_WAIT_MINUTES: i64 = 7 * 24 * 60; // one week

pub(super) const MAX_NUDGE_CEILING: i64 = 1000;

/// A nudgeable bot login with its resolved cadence: BOT_PROFILES defaults
/// overlaid with `[review.nudge]` overrides. ONLY nudgeable logins appear here
/// (enabled, non-empty review_handle, not malformed); any other missing bot
/// classifies `NotNudgeable`.

#[derive(Debug, Clone)]
pub(crate) struct NudgeConfig {
    pub(super) login: String,
    pub(super) review_handle: String,
    pub(super) wait_minutes: i64,
    pub(super) ceiling: usize,
}

/// Resolve the nudgeable-bot set for this repo: the built-in profiles, then the
/// `[review.nudge]` overrides. A malformed or `enabled = false` override REMOVES
/// its login from the set (opting out is never opting into a faster give-up);
/// an override with no resolvable `review_handle` (neither its own nor a base
/// profile's) is likewise dropped, since there is nothing to post.

pub(super) fn resolved_nudge_configs(settings: &Settings) -> Vec<NudgeConfig> {
    let mut out: Vec<NudgeConfig> = BOT_PROFILES
        .iter()
        .filter(|p| p.nudgeable && !p.review_handle.is_empty())
        .map(|p| NudgeConfig {
            login: p.login.to_string(),
            review_handle: p.review_handle.to_string(),
            wait_minutes: DEFAULT_NUDGE_WAIT_MINUTES,
            ceiling: DEFAULT_NUDGE_CEILING,
        })
        .collect();

    for ov in &settings.nudge_overrides {
        let base = out
            .iter()
            .find(|c| logins_correspond(&c.login, &ov.login))
            .cloned();
        // Drop first so an override always replaces (or removes) its login.
        out.retain(|c| !logins_correspond(&c.login, &ov.login));
        if ov.malformed || !ov.enabled {
            continue; // opt-out / bad entry -> non-nudgeable
        }
        let handle = ov
            .review_handle
            .clone()
            .or_else(|| base.as_ref().map(|b| b.review_handle.clone()))
            .filter(|h| !h.is_empty());
        let Some(review_handle) = handle else {
            continue; // no trigger to post -> not nudgeable
        };
        out.push(NudgeConfig {
            login: ov.login.clone(),
            review_handle,
            wait_minutes: ov
                .wait_minutes
                .or_else(|| base.as_ref().map(|b| b.wait_minutes))
                .unwrap_or(DEFAULT_NUDGE_WAIT_MINUTES),
            ceiling: ov
                .ceiling
                .or_else(|| base.as_ref().map(|b| b.ceiling))
                .unwrap_or(DEFAULT_NUDGE_CEILING),
        });
    }
    out
}

/// The nudge config for a configured missing-bot login, or None (non-nudgeable).

pub(super) fn nudge_config_for<'a>(
    configs: &'a [NudgeConfig],
    bot: &str,
) -> Option<&'a NudgeConfig> {
    configs.iter().find(|c| logins_correspond(&c.login, bot))
}

/// A missing bot's nudge classification for this fire. Derived fresh
/// from PR comments every fire - no durable counter - so a mention posted by a
/// human, `/fno:pr check`, or a sibling worktree counts identically and
/// self-heals across restart / compaction / handoff.

#[derive(Debug, Clone, PartialEq)]
pub(super) enum NudgeClass {
    /// No mention within the wait window: work to DO (post the trigger). Never
    /// idlable.
    NeedsNudge,
    /// Newest mention still inside the wait window: a genuine async wait. The
    /// only idlable nudge state.
    Awaiting,
    /// Ceiling reached and the newest mention timed out: nobody will end this
    /// wait. Never idlable, so the NoProgress backstop reaps it.
    Unresponsive,
    /// Login footnote cannot nudge (no profile/override, disabled, or a peer
    /// sentinel): today's block-and-wait behavior, unchanged. Idlable (status
    /// quo).
    NotNudgeable,
}

/// One missing bot's classification plus the facts the block message renders.

#[derive(Debug, Clone)]
pub(super) struct BotNudge {
    pub(super) login: String,
    pub(super) class: NudgeClass,
    /// The trigger to post; "" when NotNudgeable.
    pub(super) review_handle: String,
    pub(super) ceiling: usize,
    /// Mention count on the PR (every issue comment containing review_handle).
    pub(super) nudges: usize,
    /// Minutes since the newest mention (0 when there is none).
    pub(super) newest_age_min: i64,
    /// Minutes from the oldest mention to now (0 when there is none), for the
    /// "did not review after N nudges over Mm" give-up line.
    pub(super) span_min: i64,
}

/// Whether this state may idle on a `<watching>` tag: only a genuine async wait
/// (Awaiting) or a login we never nudge (NotNudgeable, status quo). NeedsNudge is
/// work to do; Unresponsive is a wait nobody ends.
pub(super) fn nudge_class_idlable(class: &NudgeClass) -> bool {
    matches!(class, NudgeClass::Awaiting | NudgeClass::NotNudgeable)
}

/// Classify one missing bot against the PR's issue comments. A mention is every
/// issue comment whose body contains the trigger handle, author unrestricted (a
/// mention is a request from anyone; only a usage-limit *claim* is scoped to the
/// bot's own login). Reads NO review timestamp and NO `reviews[].commit`: the
/// bot gate is PR-lifetime, and touching either silently re-pins it to head.

pub(super) fn classify_bot_nudge(
    login: &str,
    comments: &[Value],
    cfg: Option<&NudgeConfig>,
    now: DateTime<Utc>,
) -> BotNudge {
    let Some(cfg) = cfg else {
        return BotNudge::not_nudgeable(login);
    };
    if cfg.review_handle.is_empty() {
        return BotNudge::not_nudgeable(login);
    }
    let mut total = 0usize;
    let mut times: Vec<DateTime<Utc>> = Vec::new();
    for c in comments {
        let body = c.get("body").and_then(|v| v.as_str()).unwrap_or("");
        if !body.contains(&cfg.review_handle) {
            continue;
        }
        total += 1;
        // A malformed/missing createdAt must NOT push toward Unresponsive:
        // giving up on a parse error is not reversible, asking again is (AC-ERR).
        if let Some(dt) = c
            .get("createdAt")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        {
            times.push(dt);
        }
    }
    if total == 0 {
        return BotNudge {
            login: login.to_string(),
            class: NudgeClass::NeedsNudge,
            review_handle: cfg.review_handle.clone(),
            ceiling: cfg.ceiling,
            nudges: 0,
            newest_age_min: 0,
            span_min: 0,
        };
    }
    let (Some(newest), Some(oldest)) = (times.iter().max().copied(), times.iter().min().copied())
    else {
        // Mentions exist but none carried a usable timestamp: ask again (cheap).
        return BotNudge {
            login: login.to_string(),
            class: NudgeClass::NeedsNudge,
            review_handle: cfg.review_handle.clone(),
            ceiling: cfg.ceiling,
            nudges: total,
            newest_age_min: 0,
            span_min: 0,
        };
    };
    let newest_age_min = (now - newest).num_minutes().max(0);
    let span_min = (now - oldest).num_minutes().max(0);
    let class = if (now - newest) < chrono::Duration::minutes(cfg.wait_minutes) {
        NudgeClass::Awaiting
    } else if total >= cfg.ceiling {
        NudgeClass::Unresponsive
    } else {
        NudgeClass::NeedsNudge // previous mention timed out; ask again
    };
    BotNudge {
        login: login.to_string(),
        class,
        review_handle: cfg.review_handle.clone(),
        ceiling: cfg.ceiling,
        nudges: total,
        newest_age_min,
        span_min,
    }
}
