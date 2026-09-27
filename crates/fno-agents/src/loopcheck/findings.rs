//! Which review findings are still open on the PR? Inline finding parsing, severity, and wontfix handling.

use super::*;

/// A blocking inline finding: a root review comment (in_reply_to_id == null)
/// authored by a required bot whose body carries a blocking severity badge.
#[derive(Debug, Clone)]
pub(super) struct Finding {
    pub(super) id: i64,
    /// Bot login that posted the finding (REST `user.login`).
    pub(super) author: String,
    pub(super) path: String,
    pub(super) line: i64,
    pub(super) created_at: String,
    /// Parsed severity label (P1 / critical / high).
    pub(super) severity: &'static str,
    /// Whether this finding's thread had ANY non-bot reply. False is the
    /// silent-failure shape (PR #447, #787): the finding was answered with a
    /// top-level PR comment, which this gate cannot read, so it reads as
    /// unaddressed with no named cause. Carried per-finding so the block
    /// reason can name the count and the missing in_reply_to mechanism.
    pub(super) had_reply: bool,
}

/// Parse a blocking severity from the bot's own badge markup. The exact
/// strings are pinned from PR #447 ground truth; both the alt-text and the
/// badge-URL forms are matched so a partial render still classifies:
///   codex:  `![P1 Badge](https://img.shields.io/badge/P1-orange?style=flat)`
///   gemini: `![high](https://www.gstatic.com/codereviewagent/high-priority.svg)`
/// Anything unparseable is advisory, never blocking (locked decision 4:
/// under-blocking is the only safe failure - the agent cannot edit a bot's
/// comment, and PR history is the post-hoc backstop).
pub(super) fn blocking_severity(body: &str) -> Option<&'static str> {
    if body.contains("![P1 Badge]") || body.contains("badge/P1-") {
        return Some("P1");
    }
    if body.contains("![critical]") || body.contains("critical-priority.svg") {
        return Some("critical");
    }
    if body.contains("![high]") || body.contains("high-priority.svg") {
        return Some("high");
    }
    None
}

/// Max of two timestamp strings, treating "none"/"" as the lowest value.
/// Both sides are compared chronologically when they parse (gemini HIGH on
/// #448: an offset-suffixed timestamp can sort above a Zulu one
/// lexicographically while being earlier in UTC); the returned value is
/// always one of the ORIGINAL strings so the fingerprint stays byte-stable.
/// Unparseable-but-real strings fall back to lexicographic comparison.
pub(super) fn max_ts(a: &str, b: &str) -> String {
    if let (Ok(da), Ok(db)) = (a.parse::<DateTime<Utc>>(), b.parse::<DateTime<Utc>>()) {
        return if da >= db {
            a.to_string()
        } else {
            b.to_string()
        };
    }
    let a_real = !a.is_empty() && a != "none";
    let b_real = !b.is_empty() && b != "none";
    match (a_real, b_real) {
        (true, true) => {
            if a >= b {
                a.to_string()
            } else {
                b.to_string()
            }
        }
        (true, false) => a.to_string(),
        (false, true) => b.to_string(),
        (false, false) => "none".to_string(),
    }
}

/// The `wontfix:` decline marker (documented in skills/check-pr). Matched
/// case-insensitively in a non-bot reply body.
pub(super) const WONTFIX_MARKER: &str = "wontfix:";

/// True iff `a` is strictly after `b`. Both sides parse as RFC3339; an
/// unparseable timestamp returns false, so a blocking finding is never
/// cleared on garbage data. Raw string comparison is NOT used here because
/// offset-suffixed and Z-suffixed forms mis-order lexicographically
/// (e.g. "...T23:30:00+13:00" sorts above "...T11:00:00Z" as a string but
/// is 30 minutes EARLIER in UTC).
pub(super) fn ts_after(a: &str, b: &str) -> bool {
    match (a.parse::<DateTime<Utc>>(), b.parse::<DateTime<Utc>>()) {
        (Ok(da), Ok(db)) => da > db,
        _ => false,
    }
}

/// Walk the `/pulls/N/comments` array (REST shape: `user.login`,
/// `in_reply_to_id`, `created_at`). Returns the newest comment timestamp
/// (fingerprint contribution) and the UNADDRESSED blocking findings.
///
/// A blocking finding is addressed iff its thread has a non-bot reply AND
/// (a commit landed after the finding's created_at OR a non-bot reply body
/// carries `wontfix:`). The reply is mandatory: a commit alone must not
/// silently clear a P1 (anti-gaming, locked decision 3).
pub(super) fn compute_unaddressed_findings(
    comments: &[Value],
    commit_dates: &[String],
    required_bots: &[String],
    external_reviewers: &[String],
) -> (String, Vec<Finding>) {
    let mut latest_ts = String::new();
    let mut candidates: Vec<Finding> = Vec::new();
    // finding id -> non-bot replies' bodies
    let mut replies: std::collections::HashMap<i64, Vec<String>> = std::collections::HashMap::new();

    for c in comments {
        let created_at = c.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        if !created_at.is_empty() && created_at > latest_ts.as_str() {
            latest_ts = created_at.to_string();
        }

        let login = c
            .pointer("/user/login")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let body = c.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let in_reply_to = c.get("in_reply_to_id").and_then(|v| v.as_i64());

        match in_reply_to {
            Some(parent_id) => {
                // A reply. Only non-bot replies count as the agent's ack.
                if !is_bot_reviewer(login, external_reviewers) {
                    replies.entry(parent_id).or_default().push(body.to_string());
                }
            }
            None => {
                // A root comment: a finding when a required bot posted it
                // with a blocking badge.
                let by_required_bot = required_bots
                    .iter()
                    .any(|bot| login_matches_bot(login, bot));
                if by_required_bot {
                    if let Some(severity) = blocking_severity(body) {
                        // A REST comment always carries an integer id; a row
                        // without one is schema drift. Skip it rather than
                        // pooling id-less findings on a shared default bucket
                        // where a single stray reply could mark them all
                        // addressed (under-blocking is the safe direction per
                        // locked decision 4; PR history is the backstop).
                        let Some(id) = c.get("id").and_then(|v| v.as_i64()) else {
                            eprintln!(
                                "loop-check: skipping blocking finding with missing id (author={login})"
                            );
                            continue;
                        };
                        candidates.push(Finding {
                            id,
                            author: login.to_string(),
                            path: c
                                .get("path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string(),
                            line: c
                                .get("line")
                                .and_then(|v| v.as_i64())
                                .or_else(|| c.get("original_line").and_then(|v| v.as_i64()))
                                .unwrap_or(0),
                            created_at: created_at.to_string(),
                            severity,
                            had_reply: false,
                        });
                    }
                }
            }
        }
    }

    let unaddressed: Vec<Finding> = candidates
        .into_iter()
        .filter_map(|mut f| {
            let non_bot_replies = replies.get(&f.id);
            let has_reply = non_bot_replies.map(|r| !r.is_empty()).unwrap_or(false);
            // Record whether this finding's thread had any non-bot reply so
            // the block reason can name the top-level-comment blind spot.
            f.had_reply = has_reply;
            if !has_reply {
                return Some(f); // no ack -> unaddressed
            }
            let commit_after = commit_dates.iter().any(|d| ts_after(d, &f.created_at));
            let wontfix = non_bot_replies
                .map(|rs| rs.iter().any(|b| b.to_lowercase().contains(WONTFIX_MARKER)))
                .unwrap_or(false);
            if !(commit_after || wontfix) {
                Some(f)
            } else {
                None
            }
        })
        .collect();

    let final_ts = if latest_ts.is_empty() {
        "none".to_string()
    } else {
        latest_ts
    };
    (final_ts, unaddressed)
}
