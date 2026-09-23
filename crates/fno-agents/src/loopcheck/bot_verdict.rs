//! What did a review bot conclude? Clean-pass markers, usage-limit and refusal parsing, and the per-bot verdict.

use super::*;

/// Every usage-limit marker across all bot profiles, unioned. A rate-limited
/// review bot posts one of these as an ISSUE comment when it never posts a review
/// object (PR #214). Matched case-insensitively via `contains` against a
/// lowercased body, mirroring the pinned-string approach in `blocking_severity`.
/// Unioned rather than scoped per-login to stay byte-identical to the old flat
/// `USAGE_LIMIT_MARKERS` const it replaced.
///
/// The asymmetry here INVERTED with and the marker list must be read in
/// the new direction: an under-match leaves the bot in `missing_bots`, which
/// blocks (safe, just slow), while an over-match now PARKS the PR at
/// `DoneAwaitingReview` with no automatic path back, rather than dropping the
/// bot and proceeding. Add a marker only for a string the bot posts when it
/// truly will not review; a phrase a real review could quote is not one.
pub(crate) fn body_is_usage_limit(body: &str) -> bool {
    let lower = body.to_lowercase();
    BOT_PROFILES
        .iter()
        .flat_map(|p| p.usage_markers.iter())
        .any(|m| lower.contains(m))
}

pub(super) fn body_is_reviewer_refusal(body: &str) -> bool {
    if body_is_usage_limit(body) {
        return true;
    }
    let lower = body.trim().to_lowercase();
    BOT_PROFILES
        .iter()
        .flat_map(|profile| profile.refusal_markers.iter())
        .any(|marker| lower.starts_with(marker))
}

/// The clean-pass markers for a CONFIGURED login (may differ from the comment
/// author by case or a `[bot]` suffix, and may be the config's short name -
/// hence the SYMMETRIC correspond test, not the one-way matches). Empty for a
/// login with no measured clean-pass shape, which the caller treats as "no
/// clean-pass evidence".

pub(super) fn clean_pass_markers_for(login: &str) -> &'static [&'static str] {
    BOT_PROFILES
        .iter()
        .find(|p| logins_correspond(login, p.login))
        .map(|p| p.clean_pass_markers)
        .unwrap_or(&[])
}

/// The commit a bot comment says it reviewed, from a `Reviewed commit: <hex>`
/// line. Returns "" when the line is absent or the token is not hex, which
/// makes the comment unpinned and therefore not evidence: a clean-pass comment
/// that names no commit cannot be aged, so counting it would be exactly the
/// unpinned-attestation hole the local axis already refuses. Trailing
/// punctuation (`.`, `)`, `,`) is stripped so a sentence-embedded sha parses.

pub(super) fn reviewed_commit_from_body(body: &str) -> &str {
    const MARKER: &str = "Reviewed commit:";
    // The marker match lowercases the body, so the lowercased spelling parses
    // here too; any other casing stays unpinned, which is the fail-closed no.
    // A reply QUOTING an earlier marker ("Reviewed commit: (see previous)")
    // before its own pin must not shadow the real one, so every occurrence
    // is tried until one yields a hex token.
    let marker_lc = MARKER.to_lowercase();
    let mut from = 0;
    while let Some(idx) = body[from..]
        .find(MARKER)
        .map(|i| from + i)
        .or_else(|| body[from..].find(marker_lc.as_str()).map(|i| from + i))
    {
        let token = body[idx + MARKER.len()..]
            .trim_start()
            .split_whitespace()
            .next()
            .unwrap_or("");
        // Trimmed on BOTH ends, not just the trailing one: the bot writes
        // markdown, so the sha arrives wrapped as often as it arrives bare
        // (`` `abc1234` ``, `(abc1234)`), and a leading wrapper character left
        // in place fails the all-hex test and drops a genuine pinned pass.
        let hex: &str = token.trim_matches(|c: char| !c.is_ascii_hexdigit());
        if hex.len() >= 7 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return hex;
        }
        from = idx + MARKER.len();
    }
    ""
}

/// A clean-pass issue comment by `login` that pins the commit it read, as
/// `(sha, freshness, createdAt)`. The FRESHEST pinned comment wins, selected
/// by `freshness_rank` exactly as the review-object path selects among a
/// bot's reviews: comments arrive oldest-first, so first-match would keep the
/// verdict pinned to the sha of the FIRST clean pass forever - a bot that
/// re-reviews after a head move posts a second comment the scan would never
/// reach, and the gate could never clear through this lane again. A marker
/// with no pinned sha is not evidence (returns None, never an invented sha),
/// and the marker must sit at a sentence boundary (`marker_at_sentence_end`).
/// `createdAt` rides along so `bot_verdict` can order a pass against a later
/// quota bounce; an absent timestamp compares as the empty string.
/// A usage-limit comment by a characterized bot login. Shared by rule 2's
/// later-refusal ordering and rule 3, which previously re-implemented the
/// same three-part filter inline. The author check is two-part (round 3):
/// the author's login contains the configured name AND the author resolves
/// to a known bot profile, so a config short name ("codex") cannot draft
/// every human whose login contains it into the bot's marker lane.

pub(super) fn usage_limit_comment_by(login: &str, c: &Value) -> bool {
    let author = c
        .pointer("/author/login")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    login_matches_bot(author, login)
        && profile_by_author(author).is_some()
        && body_is_usage_limit(c.get("body").and_then(|v| v.as_str()).unwrap_or(""))
}

pub(super) fn refusal_comment_by(login: &str, c: &Value) -> bool {
    let author = c
        .pointer("/author/login")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    login_matches_bot(author, login)
        && profile_by_author(author).is_some()
        && body_is_reviewer_refusal(c.get("body").and_then(|v| v.as_str()).unwrap_or(""))
}

/// A comment's `createdAt`, empty when absent (empty orders as "unknown",
/// which each caller resolves fail-closed for its own rule).

pub(super) fn comment_ts(c: &Value) -> &str {
    c.get("createdAt").and_then(|v| v.as_str()).unwrap_or("")
}

pub(super) fn clean_pass_review(
    comments: &[Value],
    login: &str,
    freshness: &dyn Fn(&str) -> Freshness,
) -> Option<(String, Freshness, Option<String>)> {
    let markers = clean_pass_markers_for(login);
    if markers.is_empty() {
        return None;
    }
    let mut best: Option<(String, Freshness, Option<String>)> = None;
    for c in comments {
        let author = c
            .pointer("/author/login")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // One-way: the AUTHOR's login must contain the configured name. The
        // symmetric test lets a human whose login is a substring of the bot's
        // ("codex" vs "chatgpt-codex-connector") post the pass comment. The
        // profile guard closes the remaining short-name route (round 3): with
        // the config naming "codex", containment alone drafts any human whose
        // login merely contains it, so the author must also resolve to a
        // characterized bot profile.
        if !login_matches_bot(author, login) || profile_by_author(author).is_none() {
            continue;
        }
        let body = c.get("body").and_then(|v| v.as_str()).unwrap_or("");
        if body_is_usage_limit(body) {
            // A refusal quoting an earlier pass is still a refusal: the usage
            // marker inside the same body beats the clean-pass quotation that
            // body carries, exactly as a within-body tie must resolve.
            continue;
        }
        let lower = body.to_lowercase();
        if !markers.iter().any(|m| marker_at_sentence_end(&lower, m)) {
            continue;
        }
        let sha = reviewed_commit_from_body(body);
        if sha.is_empty() {
            continue;
        }
        let fresh = freshness(sha);
        let ts = c
            .get("createdAt")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // On EQUAL rank the later comment wins: keeping first-seen pins the
        // verdict to the earliest pass of that rank and lets a quota bounce
        // BETWEEN two equal-rank passes outdate the newer one (PR 917 round
        // two: the newest pass postdates the bounce and must be the evidence).
        // "Later" is decided by `ts_after` on the parsed `createdAt`, never by
        // iteration order: array order is the payload's promise, not ours, and
        // a raw string compare mis-orders offset-suffixed against Z-suffixed
        // stamps. Same predicate `bot_verdict` uses on `submittedAt`, so the
        // two arms of one verdict cannot order the same evidence differently.
        let better = match best.as_ref() {
            None => true,
            Some((_, b, kept_ts)) => {
                freshness_rank(fresh) > freshness_rank(*b)
                    || (freshness_rank(fresh) == freshness_rank(*b)
                        && ts_after(
                            ts.as_deref().unwrap_or(""),
                            kept_ts.as_deref().unwrap_or(""),
                        ))
            }
        };
        if better {
            best = Some((sha.to_string(), fresh, ts));
        }
    }
    best
}

/// A clean-pass marker only counts when the SENTENCE it ends is the pass
/// sentence and what follows is the measured shape: end of body, the `Bravo`
/// flourish (itself sentence-final), or the `Reviewed commit:` pin (often on
/// its own line). The bot posts the same clause while REPORTING findings -
/// `Didn't find any major issues, but 2 minor ones...` and the period form
/// `...issues. 2 minor ones need attention.` - and a punctuation peek alone
/// cannot tell a pass from a findings note sharing the clause; the FOLLOW-UP
/// sentence can. `Bravo` must therefore END its sentence too (round 3):
/// `Bravo, but...` and `Bravo. 2 minor ones...` are findings notes wearing
/// the flourish, and a bare `starts_with("bravo")` counted both as passes.
/// Characterized from the measured specimens (PR #947): a new bot's shape
/// gets its own profile entry, never a loosened predicate.

pub(super) fn marker_at_sentence_end(lower: &str, marker: &str) -> bool {
    // The measured follow-ups to a genuine pass sentence. A newline counts as
    // a terminator exactly like a period: the measured pin posts as its own
    // line, and `\n` inside the bot's body is a sentence boundary, not prose.
    // A quoted line is transparent: the bot's own next sentence is what the
    // shape judges, and a reply may quote an earlier marker before its pin.
    fn shaped(t: &str) -> bool {
        if t.is_empty() {
            return true;
        }
        if let Some(rest) = t.strip_prefix('>') {
            return shaped(
                rest.split_once('\n')
                    .map(|(_, next)| next.trim_start())
                    .unwrap_or(""),
            );
        }
        t.starts_with("reviewed commit:")
            || match t.strip_prefix("bravo") {
                Some(rest) => {
                    rest.is_empty()
                        || rest.starts_with(['.', '!', '?', '\n']) && shaped(rest[1..].trim_start())
                }
                None => false,
            }
    }
    lower.match_indices(marker).any(|(i, _)| {
        let rest = lower[i + marker.len()..].trim_start();
        if shaped(rest) {
            return true;
        }
        for term in ['.', '!', '?', '\n'] {
            if let Some(next) = rest.strip_prefix(term) {
                if shaped(next.trim_start()) {
                    return true;
                }
            }
        }
        false
    })
}

/// One per-bot verdict from ALL of that bot's evidence. The coverage axis
/// (`classify_coverage`) and the presence gate (`compute_review_info`) ask
/// this ONE question through this ONE predicate: independent scans of the
/// same payload are how a PR ends up `all_required_passed` while coverage
/// reads `Refused` (or the inverse) and the run wedges between two gates
/// that never reconcile - the reader-divergence class exists to
/// delete. Precedence, each rule falling through only when its evidence is
/// absent or does not count:
/// 1. a review object whose commit still matches HEAD -> `Reviewed`
/// 2. a pinned clean-pass comment that still counts, unless a usage-limit
///    comment by the same login POSTDATES it -> `Reviewed`
/// 3. a usage-limit comment -> `Refused`
/// 4. a review object that no longer counts -> `Stale` (recorded, not
///    dropped)
/// 5. a pinned clean-pass comment that no longer counts -> `Stale`
/// 6. nothing -> `Absent`
///
/// Rule 2 orders by comment `createdAt`: a quota bounce AFTER a pass
/// outdates it (fail closed on both axes), a bounce BEFORE it does not (the
/// bot recovered and read the code - a historical refusal is not a life
/// sentence). Both orderings require positive timestamp evidence; on ties
/// or absent timestamps the pass stands, symmetrically with the refusal.

pub(crate) fn bot_verdict(
    login: &str,
    reviews: &[Value],
    comments: &[Value],
    freshness: &dyn Fn(&str) -> Freshness,
) -> (CoverageVerdict, String, Option<Freshness>) {
    // That login's freshest review object, selected by `freshness_rank`
    // exactly as classify_coverage's known-author scan selects, carrying its
    // `submittedAt` so the refusal scan below can order it against a later
    // bounce. The author match is ONE-WAY (login_matches_bot: the author's
    // login contains the configured name), restoring the presence gate's old
    // pre-filter: the symmetric test let a drive-by human whose login is a
    // substring of the bot's ("codex" vs "chatgpt-codex-connector") satisfy
    // the gate. On EQUAL rank the later-submitted review is the evidence,
    // mirroring clean_pass_review's equal-rank rule.
    let mut review: Option<(String, Freshness, String)> = None;
    for r in reviews {
        let author = r
            .pointer("/author/login")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !login_matches_bot(author, login) {
            continue;
        }
        if r.get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
        {
            continue;
        }
        let oid = r
            .pointer("/commit/oid")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let ts = r
            .get("submittedAt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let fresh = freshness(oid);
        let better = match review.as_ref() {
            None => true,
            Some((_, b, kept_ts)) => {
                freshness_rank(fresh) > freshness_rank(*b)
                    || (freshness_rank(fresh) == freshness_rank(*b) && ts_after(&ts, kept_ts))
            }
        };
        if better {
            review = Some((oid.to_string(), fresh, ts));
        }
    }
    if let Some((sha, fresh, _)) = review.as_ref() {
        if fresh.counts() {
            return (CoverageVerdict::Reviewed, sha.clone(), Some(*fresh));
        }
    }
    let clean = clean_pass_review(comments, login, freshness);
    if let Some((sha, fresh, clean_ts)) = clean.as_ref() {
        if fresh.counts() {
            let later_refusal = comments.iter().any(|c| {
                if !refusal_comment_by(login, c) {
                    return false;
                }
                // ts_after, never a raw compare: offset-suffixed and Z-suffixed
                // RFC3339 forms mis-order lexicographically, and an UNPARSEABLE
                // side (an absent clean_ts) reads false - the pass stands,
                // which is what the rule-2 doc above promises.
                ts_after(comment_ts(c), clean_ts.as_deref().unwrap_or(""))
            });
            if !later_refusal {
                return (CoverageVerdict::Reviewed, sha.clone(), Some(*fresh));
            }
        }
    }
    // Rule 3 is timestamp-aware (round 3): a bounce that PREDATES the bot's
    // own latest evidence does not park the verdict at Refused. The bot
    // recovered and reviewed; the honest label for a pass that no longer
    // counts is Stale - ask a re-read, a state with a path back - rather
    // than Refused, whose only exit is a quota reset nobody can schedule.
    // Fail direction differs from rule 2 on purpose: rule 2 guards evidence
    // of a review (absent timestamps keep the pass), this rule guards
    // permission to arm (an absent side keeps the refusal).
    let latest_evidence_ts = {
        let r_ts = review.as_ref().map(|(_, _, t)| t.as_str()).unwrap_or("");
        let c_ts = clean
            .as_ref()
            .map(|(_, _, t)| t.as_deref().unwrap_or(""))
            .unwrap_or("");
        if ts_after(c_ts, r_ts) {
            c_ts
        } else {
            r_ts
        }
    };
    let refused = comments.iter().any(|c| {
        refusal_comment_by(login, c)
            && (comment_ts(c).is_empty()
                || latest_evidence_ts.is_empty()
                || ts_after(comment_ts(c), latest_evidence_ts))
    });
    if refused {
        return (CoverageVerdict::Refused, String::new(), None);
    }
    if let Some((sha, fresh, _)) = review {
        return (CoverageVerdict::Stale, sha, Some(fresh));
    }
    if let Some((sha, fresh, _)) = clean {
        return (CoverageVerdict::Stale, sha, Some(fresh));
    }
    (CoverageVerdict::Absent, String::new(), None)
}
