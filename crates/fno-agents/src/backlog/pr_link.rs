//! PR attribution for the update verb. The url grammar is ported from
//! `cli/src/fno/graph/_reconcile.py` verbatim: the host is anchored, never
//! searched for, and the two-segment path depth is the contract a looser
//! parser would break.

use regex::Regex;
use std::sync::OnceLock;

fn pr_url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)^(?:[A-Za-z][A-Za-z0-9+.-]*://)?(?:[^/@]*@)?github\.com(?::\d+)?/([^/\s]+)/([^/\s]+?)(?:\.git)?/(?:pull|issues)/(\d+)(?:[/?#]|$)"#,
        )
        .expect("PR url regex")
    })
}
/// `owner/repo` from a GitHub PR url, or None.
pub fn repo_slug_from_url(url: Option<&str>) -> Option<String> {
    let url = url?;
    let m = pr_url_re().captures(url.trim())?;
    Some(format!("{}/{}", &m[1], &m[2]))
}

/// The PR number a GitHub PR url names, or None.
pub fn pr_number_from_url(url: Option<&str>) -> Option<i64> {
    let url = url?;
    let m = pr_url_re().captures(url.trim())?;
    m.get(3)?.as_str().parse().ok()
}

/// The one place a PR url is built, so `repo_slug_from_url` round-trips it.
pub fn pr_url_from_slug(slug: &str, pr: i64) -> String {
    format!("https://github.com/{slug}/pull/{pr}")
}

/// True for a bare `owner/repo` slug, the shape `--repo` accepts.
pub fn is_repo_slug(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    let Some((owner, repo)) = value.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !repo.is_empty()
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        && repo
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}
