//! Which pull requests fleet automation may act on.
//!
//! Law d-2191724b: no outside PR is merged, healed, bound or reviewed by
//! fleet lanes on its own, and there is no admit door. The owner's one
//! recorded act over any PR is the per-PR hold, an operator law row.

use crate::authorized_merge::{PrFacts, ProbeOutcome};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const HOLD_SUBJECT: &str = "pr-hold";
pub const HOLD_DECISION: &str = "do not merge this pull request";
const INSIDE: [&str; 3] = ["OWNER", "MEMBER", "COLLABORATOR"];

/// Some(cause) when a REST pulls object is from outside; Err when the base
/// repo is unreadable. A null head repo is a deleted fork, so it reads
/// outside; so does any `author_association` that is not owner-class.
pub fn outside_reason(pull: &Value) -> Result<Option<String>, String> {
    let base = pull
        .pointer("/base/repo/full_name")
        .and_then(Value::as_str)
        .ok_or_else(|| "the pulls object named no base repo".to_string())?;
    let head = pull.pointer("/head/repo/full_name").and_then(Value::as_str);
    if head != Some(base) {
        return Ok(Some(match head {
            Some(h) => format!("head repo {h} is not the base repo {base}"),
            None => format!("a deleted fork; the base repo is {base}"),
        }));
    }
    let association = pull
        .get("author_association")
        .and_then(Value::as_str)
        .unwrap_or("NONE");
    if !INSIDE.contains(&association) {
        return Ok(Some(format!(
            "author association {association} is not one of OWNER, MEMBER, COLLABORATOR"
        )));
    }
    Ok(None)
}

/// The subject one per-PR hold lives at. Not head-scoped: a hold survives
/// pushes, and the push that lands is the thing being held.
pub fn hold_subject(slug: &str, pr: u64) -> String {
    format!("{HOLD_SUBJECT}:{slug}#{pr}")
}

/// Ok(Some(decision_id)) when any live operator row at the subject carries
/// HOLD_DECISION. `None` stdout (the read failed) and a malformed payload
/// are Err, never absence: the hold probe builds on this and must fail
/// closed.
pub fn held(decisions_stdout: Option<&[u8]>) -> Result<Option<String>, String> {
    let Some(bytes) = decisions_stdout else {
        return Err("the decisions read did not answer".to_string());
    };
    let Some(rows) = crate::loopcheck::coverage_status::operator_law_rows(bytes) else {
        return Err("malformed decisions payload".to_string());
    };
    for row in rows {
        if row.get("decision").and_then(|d| d.as_str()) == Some(HOLD_DECISION) {
            return Ok(Some(
                row.get("decision_id")
                    .and_then(|d| d.as_str())
                    .unwrap_or("unreadable id")
                    .to_string(),
            ));
        }
    }
    Ok(None)
}

/// The per-PR origin-facts cache directory: `pr_status_cache_dir` with its
/// last segment swapped for `pr-origin`, so `<state>/cache/pr-origin`.
fn pr_origin_cache_dir(cwd: &Path) -> Option<PathBuf> {
    let mut dir = crate::agents_config::pr_status_cache_dir(cwd)?;
    dir.set_file_name("pr-origin");
    Some(dir)
}

fn cache_path(dir: &Path, slug: &str, pr: u64) -> Option<PathBuf> {
    let (owner, repo) = slug.split_once('/')?;
    Some(dir.join(format!("{owner}--{repo}--{pr}.json")))
}

/// The origin facts of one pulls object, exactly as read. `head_repo` is
/// null for a deleted fork.
fn facts_of(pull: &Value) -> Value {
    json!({
        "head_repo": pull.pointer("/head/repo/full_name"),
        "base_repo": pull.pointer("/base/repo/full_name"),
        "author_association": pull.get("author_association"),
        "login": pull.pointer("/user/login"),
    })
}

/// Reshape a cached facts object into a pulls-like object, so the cache hit
/// answers through `outside_reason` and no second predicate exists.
fn pulls_of_facts(facts: &Value) -> Value {
    let head_repo = facts.get("head_repo").cloned().unwrap_or(Value::Null);
    json!({
        "head": {"repo": head_repo},
        "base": {"repo": {"full_name": facts.get("base_repo").cloned().unwrap_or(Value::Null)}},
        "author_association": facts.get("author_association").cloned().unwrap_or(Value::Null),
        "user": {"login": facts.get("login").cloned().unwrap_or(Value::Null)},
    })
}

fn read_cached(dir: &Path, slug: &str, pr: u64) -> Option<Value> {
    let path = cache_path(dir, slug, pr)?;
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// A failed cache write is ignored: the network read is the truth.
fn write_cached(dir: &Path, slug: &str, pr: u64, facts: &Value) {
    let Some(path) = cache_path(dir, slug, pr) else {
        return;
    };
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, facts.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// The real fetch: one `gh api repos/{slug}/pulls/{n}`.
fn real_fetch(slug: &str, pr: u64) -> Result<Value, String> {
    let out = std::process::Command::new("gh")
        .args(["api", &format!("repos/{slug}/pulls/{pr}")])
        .output()
        .map_err(|e| format!("gh api spawn failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "gh api repos/{slug}/pulls/{pr} exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("gh api pulls json: {e}"))
}

pub(crate) fn outside_pr(cwd: &Path, facts: &PrFacts) -> ProbeOutcome {
    match pr_origin_cache_dir(cwd) {
        Some(dir) => outside_pr_in(Some(&dir), facts, &real_fetch),
        None => outside_pr_in(None, facts, &real_fetch),
    }
}

fn outside_pr_in(
    cache_dir: Option<&Path>,
    facts: &PrFacts,
    fetch: &dyn Fn(&str, u64) -> Result<Value, String>,
) -> ProbeOutcome {
    let Some(slug) = crate::merge_grant::repo_slug_from_pr_url(&facts.url) else {
        return ProbeOutcome::Inconclusive(
            "outside_pr: the PR URL named no repository".to_string(),
        );
    };
    let cached = cache_dir.and_then(|dir| read_cached(dir, &slug, facts.number));
    let pull = match cached {
        Some(f) => pulls_of_facts(&f),
        None => match fetch(&slug, facts.number) {
            Ok(pull) => {
                if let Some(dir) = cache_dir {
                    write_cached(dir, &slug, facts.number, &facts_of(&pull));
                }
                pull
            }
            Err(e) => return ProbeOutcome::Inconclusive(format!("outside_pr: {e}")),
        },
    };
    let login = pull
        .pointer("/user/login")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let association = pull
        .get("author_association")
        .and_then(Value::as_str)
        .unwrap_or("NONE");
    match outside_reason(&pull) {
        Ok(None) => ProbeOutcome::Clear,
        Ok(Some(cause)) => ProbeOutcome::Refused(format!(
            "outside_pr: PR #{} is from {cause} by {login} ({association}); \
             fleet automation never merges, heals, binds or reviews an outside pull request. \
             Worth-pursuing work is rebuilt from its backlog node: see \
             docs/architecture/authorized-merge.md \"Outside pull requests\"",
            facts.number
        )),
        Err(e) => ProbeOutcome::Inconclusive(format!("outside_pr: {e}")),
    }
}

/// Ok((success, combined output)) for one `gh` invocation. Injectable so the
/// disarm table needs no network.
pub(crate) type GhRun<'a> = &'a dyn Fn(&[&str]) -> Result<(bool, String), String>;

fn real_gh(args: &[&str]) -> Result<(bool, String), String> {
    let out = std::process::Command::new("gh")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), text))
}

pub(crate) fn pr_hold(cwd: &Path, facts: &PrFacts) -> ProbeOutcome {
    let Some(slug) = crate::merge_grant::repo_slug_from_pr_url(&facts.url) else {
        return ProbeOutcome::Inconclusive("pr_hold: the PR URL named no repository".to_string());
    };
    let out = std::process::Command::new(crate::scrape::fno_bin())
        .args([
            "backlog",
            "decisions",
            &hold_subject(&slug, facts.number),
            "--lane",
            "law",
            "--state",
            "live",
            "--json",
        ])
        .current_dir(cwd)
        .output();
    let stdout = match out {
        Ok(o) if o.status.success() => Some(o.stdout),
        Ok(o) => {
            return ProbeOutcome::Inconclusive(format!(
                "pr_hold: the decisions read exited {}: {}",
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stderr).trim()
            ))
        }
        Err(e) => return ProbeOutcome::Inconclusive(format!("pr_hold: {e}")),
    };
    match held(stdout.as_deref()) {
        Ok(None) => ProbeOutcome::Clear,
        Ok(Some(id)) => {
            if facts.armed {
                crate::merge_hold::disarm_automerge(facts.number);
            }
            ProbeOutcome::Refused(format!(
                "pr_hold: the owner holds PR #{} ({id}); release: fno backlog decide '{}' \
                 'release' --authority operator --supersedes {id}",
                facts.number,
                hold_subject(&slug, facts.number)
            ))
        }
        Err(e) => ProbeOutcome::Inconclusive(format!("pr_hold: {e}")),
    }
}

/// Called by the decide door after a ruling is recorded. Disarms at once for
/// an operator pr-hold row: a queue armed before the hold would otherwise
/// merge server-side with no re-check. Never fails the record; the ruling is
/// already durable. Every other subject or authority returns None with no
/// process spawned.
pub(crate) fn after_record(
    subject: &str,
    decision: &str,
    authority: Option<&str>,
) -> Option<String> {
    after_record_with(&real_gh, subject, decision, authority)
}

pub(crate) fn after_record_with(
    gh: GhRun,
    subject: &str,
    decision: &str,
    authority: Option<&str>,
) -> Option<String> {
    if authority != Some("operator") || decision != HOLD_DECISION {
        return None;
    }
    let rest = subject.strip_prefix(HOLD_SUBJECT)?.strip_prefix(':')?;
    let (slug, pr) = rest.split_once('#')?;
    let pr: u64 = pr.parse().ok()?;
    if slug.is_empty() {
        return None;
    }
    let args = [
        "pr",
        "merge",
        &pr.to_string(),
        "--repo",
        slug,
        "--disable-auto",
    ];
    match gh(&args) {
        Ok((true, _)) => Some(format!(
            "pr-hold: disabled an armed auto-merge on {slug}#{pr}"
        )),
        Ok((false, output)) => {
            let last = output.lines().last().unwrap_or("").trim().to_string();
            Some(format!(
                "pr-hold: {last}; run gh pr merge {pr} --repo {slug} --disable-auto by hand \
                 if auth or network failed"
            ))
        }
        Err(e) => Some(format!(
            "pr-hold: {e}; run gh pr merge {pr} --repo {slug} --disable-auto by hand \
             if auth or network failed"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn pull(head: Value, base: &str, association: &str) -> Value {
        json!({
            "head": {"repo": {"full_name": head}},
            "base": {"repo": {"full_name": base}},
            "author_association": association,
            "user": {"login": "someone"},
        })
    }

    #[test]
    fn the_outside_predicate_tables_fork_association_cache_and_fails_closed() {
        let base = "bllshttng/footnote";
        // Same repo plus OWNER reads inside.
        assert_eq!(
            outside_reason(&pull(json!("bllshttng/footnote"), base, "OWNER")),
            Ok(None)
        );
        // A fork reads outside, naming the head repo.
        let fork = outside_reason(&pull(json!("stranger/footnote"), base, "OWNER")).unwrap();
        assert!(fork.is_some());
        // A deleted fork (null head repo) reads outside.
        let deleted = outside_reason(&pull(Value::Null, base, "OWNER")).unwrap();
        assert!(deleted.is_some());
        // A same-repo non-member reads outside.
        let nonmember = outside_reason(&pull(json!(base), base, "NONE")).unwrap();
        assert!(nonmember.is_some());
        // No base repo is an unreadable pull, not an outside one.
        let no_base = json!({"head": {"repo": json!(base)}, "author_association": "OWNER"});
        assert!(outside_reason(&no_base).is_err());

        // A cache hit answers with no fetch; a corrupt file reads as a miss.
        let tmp = std::env::temp_dir().join(format!("pr-admission-test-{}", std::process::id()));
        let dir = tmp.join("cache");
        let _ = std::fs::remove_dir_all(&dir);
        let pr7 = facts(7);
        // A cache hit: the counting fetch must stay at zero.
        cache_fixture(
            &dir,
            7,
            Some(&json!({
                "head_repo": "stranger/footnote",
                "base_repo": "bllshttng/footnote",
                "author_association": "OWNER",
                "login": "stranger",
            })),
        );
        let calls = std::cell::RefCell::new(0usize);
        let count = |_: &str, _: u64| -> Result<Value, String> {
            *calls.borrow_mut() += 1;
            Ok(Value::Null)
        };
        let out = outside_pr_in(Some(dir.as_path()), &pr7, &count);
        assert_eq!(calls.into_inner(), 0);
        assert!(
            matches!(out, ProbeOutcome::Refused(reason) if reason.contains("stranger/footnote"))
        );
        // A corrupt file reads as a miss: one fetch, then the facts are cached.
        std::fs::write(
            cache_path(&dir, "bllshttng/footnote", 7).unwrap(),
            "{not json",
        )
        .unwrap();
        let fork = pull(
            json!("stranger/footnote"),
            "bllshttng/footnote",
            "FIRST_TIMER",
        );
        let out = outside_pr_in(Some(dir.as_path()), &pr7, &|_, _| Ok(fork.clone()));
        assert!(matches!(out, ProbeOutcome::Refused(_)));
        let cached = read_cached(&dir, "bllshttng/footnote", 7);
        assert_eq!(
            cached.and_then(|c| c.get("author_association").cloned()),
            Some(json!("FIRST_TIMER"))
        );

        // Outside reads Refused whatever the law rows say; unreadable is Inconclusive.
        let tmp = std::env::temp_dir().join(format!("pr-admission-nc-{}", std::process::id()));
        let dir = tmp.join("cache");
        let _ = std::fs::remove_dir_all(&dir);
        // Any law rows at all admit nothing: outside is final.
        let out = outside_pr_in(Some(dir.as_path()), &facts(8), &|_, _| {
            Ok(pull(
                json!("stranger/footnote"),
                "bllshttng/footnote",
                "OWNER",
            ))
        });
        assert!(matches!(out, ProbeOutcome::Refused(reason) if reason.contains("never merges")));
        // A fetch that fails reads Inconclusive, never Clear.
        let out = outside_pr_in(Some(dir.as_path()), &facts(9), &|_, _| {
            Err("gh down".to_string())
        });
        assert!(matches!(out, ProbeOutcome::Inconclusive(e) if e.contains("gh down")));
        // A URL with no repository is Inconclusive.
        let no_slug = PrFacts {
            number: 10,
            url: "not-a-pr-url".to_string(),
            ..Default::default()
        };
        assert!(matches!(
            outside_pr_in(Some(dir.as_path()), &no_slug, &|_, _| Ok(Value::Null)),
            ProbeOutcome::Inconclusive(_)
        ));
    }
    fn decisions(rows: Vec<Value>) -> Vec<u8> {
        json!({"decisions": rows}).to_string().into_bytes()
    }

    fn row(decision: &str, authority: &str, id: &str) -> Value {
        json!({"decision": decision, "authority_source": authority, "decision_id": id})
    }

    fn cache_fixture(dir: &Path, pr: u64, body: Option<&Value>) -> PathBuf {
        let slug = "bllshttng/footnote";
        let path = cache_path(dir, slug, pr).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        if let Some(v) = body {
            std::fs::write(&path, v.to_string()).unwrap();
        }
        path
    }

    fn facts(pr: u64) -> PrFacts {
        PrFacts {
            number: pr,
            url: format!("https://github.com/bllshttng/footnote/pull/{pr}"),
            ..Default::default()
        }
    }

    #[test]
    fn the_pr_hold_row_reads_held_and_disarms_on_record() {
        assert_eq!(
            held(None),
            Err("the decisions read did not answer".to_string())
        );
        assert_eq!(
            held(Some(b"not json")),
            Err("malformed decisions payload".to_string())
        );
        assert_eq!(held(Some(&decisions(vec![]))), Ok(None));
        assert_eq!(
            held(Some(&decisions(vec![row(HOLD_DECISION, "agent", "d-1")]))),
            Ok(None)
        );
        assert_eq!(
            held(Some(&decisions(vec![row(
                "some other decision",
                "operator",
                "d-2"
            )]))),
            Ok(None)
        );
        assert_eq!(
            held(Some(&decisions(vec![row(
                HOLD_DECISION,
                "operator",
                "d-3"
            )]))),
            Ok(Some("d-3".to_string()))
        );

        // Recording an operator pr-hold row disarms exactly once; every
        // other authority, decision or subject runs no gh call.
        let seen = std::cell::RefCell::new(0usize);
        let gh = |_: &[&str]| -> Result<(bool, String), String> {
            *seen.borrow_mut() += 1;
            Ok((true, String::new()))
        };
        let out = after_record_with(
            &gh,
            "pr-hold:bllshttng/footnote#7",
            HOLD_DECISION,
            Some("operator"),
        );
        assert_eq!(*seen.borrow(), 1);
        assert_eq!(
            out,
            Some("pr-hold: disabled an armed auto-merge on bllshttng/footnote#7".to_string())
        );
        assert!(after_record_with(&gh, "pr-hold:o/r#7", HOLD_DECISION, Some("crown")).is_none());
        assert!(after_record_with(&gh, "pr-hold:o/r#7", "release", Some("operator")).is_none());
        assert!(after_record_with(
            &gh,
            "node-subject-that-is-not-a-hold",
            HOLD_DECISION,
            Some("operator")
        )
        .is_none());
        assert_eq!(*seen.borrow(), 1);
        // A gh failure still returns the manual-disarm line.
        let out = after_record_with(
            &|_| Err("spawn failed".to_string()),
            "pr-hold:o/r#9",
            HOLD_DECISION,
            Some("operator"),
        );
        assert!(out
            .unwrap()
            .contains("gh pr merge 9 --repo o/r --disable-auto by hand"));
    }
}
