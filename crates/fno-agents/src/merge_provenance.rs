//! Merge provenance: who merged PR N, by which path. One `decision_span`
//! row per merge hop in the project journal, adopting the envelope (the
//! schema rule: add a `span_kind` value, never a new event type):
//! `merge_landed` and `merge_armed` from the merge owner
//! ([`crate::authorized_merge::run`]), `merge_requested` from the gh proxy
//! and the PostToolUse hook, `merged_outside_fno` from reconcile. Reader
//! query and field meanings: docs/architecture/authorized-merge.md.

use crate::authorized_merge::{Effect, Outcome, PrFacts, Request};
use crate::decision_trace::{self, Trace};
use serde_json::{json, Map, Value};
#[cfg(test)]
use std::ffi::OsString;
use std::path::{Path, PathBuf};

thread_local! {
    static TEST_JOURNAL: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// In test builds the writers record only through the calling test's own
/// [`TestJournal`] (thread-local, so parallel tests never share a journal);
/// a production binary always records.
fn recording_enabled() -> bool {
    !cfg!(test) || test_journal_target().is_some()
}

#[cfg(test)]
fn test_journal_target() -> Option<PathBuf> {
    TEST_JOURNAL.with(|j| j.borrow().clone())
}

#[cfg(not(test))]
fn test_journal_target() -> Option<PathBuf> {
    None
}

/// Test-only journal opt-in: a temp `FNO_REPO_ROOT` plus the writer guard
/// flag, restored on drop and held under the shared test env lock so the
/// env-sensitive suites serialize.
#[cfg(test)]
pub(crate) struct TestJournal {
    root: tempfile::TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
    prev: Vec<(&'static str, Option<OsString>)>,
}

#[cfg(test)]
impl TestJournal {
    pub(crate) fn opt_in() -> Self {
        let lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().expect("temp journal root");
        let mut prev = ["FNO_AGENTS_HOME"]
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect::<Vec<_>>();
        let agents_home = root.path().join("agents-home");
        std::fs::create_dir_all(&agents_home).expect("temp agents home");
        std::env::set_var("FNO_AGENTS_HOME", &agents_home);
        // The ambient identity names, scrubbed so actor_kind reads "user"
        // deterministically; saved and restored with the rest.
        for key in crate::claims::AMBIENT_IDENTITY_NAMES
            .iter()
            .copied()
            .chain(
                crate::claims::HARNESS_SESSION_MARKERS
                    .iter()
                    .map(|(k, _)| *k),
            )
            .chain(
                crate::claims::LEGACY_HARNESS_SESSION_MARKERS
                    .iter()
                    .map(|(k, _)| *k),
            )
        {
            prev.push((key, std::env::var_os(key)));
            std::env::remove_var(key);
        }
        TEST_JOURNAL
            .with(|j| {
                *j.borrow_mut() =
                    Some(crate::law_match::project_events_journal_in(root.path()));
            });
        Self {
            root,
            _lock: lock,
            prev,
        }
    }

    /// The journal path under the temp root (the store sits beside it).
    pub(crate) fn journal(&self) -> PathBuf {
        crate::law_match::project_events_journal_in(self.root.path())
    }

    /// The temp root, for fixtures that need the repo root as a cwd.
    pub(crate) fn root(&self) -> PathBuf {
        self.root.path().to_path_buf()
    }

    pub(crate) fn rows(&self) -> Vec<Value> {
        let q = crate::event_store::EventQuery::of_types(&["decision_span"]);
        match crate::event_store::query_events(&self.journal(), &q) {
            Ok(rows) => rows
                .iter()
                .filter_map(|r| serde_json::from_str::<Value>(&r.line).ok())
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
impl Drop for TestJournal {
    fn drop(&mut self) {
        TEST_JOURNAL.with(|j| *j.borrow_mut() = None);
        for (key, value) in &self.prev {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// The journal a node repo resolves to: the canonical-root walk run from
/// the record cwd, no process-wide pin.
pub(crate) fn journal_for(cwd: Option<&str>) -> Option<PathBuf> {
    let cwd = cwd.filter(|c| !c.is_empty())?;
    Some(crate::law_match::project_events_journal_in(Path::new(cwd)))
}

fn trace_for(session: Option<String>, source: &str) -> Trace {
    Trace {
        trace_id: "none".into(),
        span_id: decision_trace::new_span_id(),
        parent_span_id: None,
        actor_session: session.clone(),
        actor_kind: decision_trace::actor_kind(session.as_deref(), source),
        comms: "chat",
        recipient_session: None,
        recipient_kind: None,
    }
}

fn write_span(span_kind: &str, trace: Trace, attrs: Map<String, Value>) {
    let journal = match test_journal_target() {
        Some(j) => j,
        None if cfg!(test) => return,
        None => crate::law_match::project_events_journal(),
    };
    if let Err(e) = decision_trace::emit_span_to(&journal, span_kind, &trace, &attrs) {
        eprintln!("fno merge provenance: span write failed: {e}");
    }
}

/// The merge owner record: one span per Merged/Armed outcome of
/// [`crate::authorized_merge::run`], so every fno merge path (the verb,
/// the pr-watch queue, the finalize arm) leaves an actor row behind. A
/// failed write never moves the Outcome; it names itself on stderr.
pub fn record_outcome(request: &Request, facts: &PrFacts, outcome: &Outcome) {
    if !recording_enabled() {
        return;
    }
    let (span_kind, grant) = match outcome {
        Outcome::Merged { merge_grant, .. } => ("merge_landed", merge_grant),
        Outcome::Armed { merge_grant, .. } => ("merge_armed", merge_grant),
        _ => return,
    };
    let path = if request.authority.as_deref() == Some("durable_grant") {
        "pr_watch"
    } else if request.effect == Effect::Arm && request.pr.is_none() {
        "finalize"
    } else {
        "pr_merge"
    };
    let source = if path == "pr_watch" { "daemon" } else { "verb" };
    let session = crate::identity::ambient_agent_handle();
    let mut attrs = Map::new();
    attrs.insert("pr".into(), json!(facts.number));
    attrs.insert(
        "repo".into(),
        json!(
            crate::backlog::pr_link::repo_slug_from_url(Some(&facts.url))
                .unwrap_or_else(|| "unknown-repo".into())
        ),
    );
    attrs.insert("head".into(), json!(facts.head_sha));
    attrs.insert("path".into(), json!(path));
    attrs.insert("grant".into(), json!(grant));
    write_span(span_kind, trace_for(session, source), attrs);
}

/// The merge intent in a gh argv: `Some(Some(n))` names the PR,
/// `Some(None)` is a bare `pr merge` (gh resolves the number itself),
/// `None` is not a merge argv. Reads the tail after the last `gh` word, so
/// a raw shell line and the proxy normalized form both match.
pub(crate) fn merge_argv(command: &[String]) -> Option<Option<u64>> {
    let tail = match command.iter().rposition(|t| t == "gh") {
        Some(i) => &command[i + 1..],
        None => command,
    };
    if let Some(i) = tail
        .windows(2)
        .position(|w| w[0] == "pr" && w[1] == "merge")
    {
        let numbered = |t: &String| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
        return Some(
            tail[i + 2..]
                .iter()
                .find(|t| numbered(t))
                .and_then(|t| t.parse().ok()),
        );
    }
    if let Some(i) = tail.iter().position(|t| t == "api") {
        let rest = &tail[i + 1..];
        let mut put = false;
        let mut j = 0;
        while j < rest.len() {
            if (rest[j] == "-X" || rest[j] == "--method") && j + 1 < rest.len() {
                put |= rest[j + 1].eq_ignore_ascii_case("PUT");
                j += 2;
            } else {
                put |= rest[j] == "-XPUT";
                j += 1;
            }
        }
        if !put {
            return None;
        }
        return rest
            .iter()
            .filter_map(|t| t.split("/pulls/").nth(1))
            .filter_map(|tail_part| tail_part.strip_suffix("/merge"))
            .filter_map(|n| n.parse().ok())
            .map(Some)
            .next();
    }
    None
}

/// The PR a merge-requesting gh argv names, when it names one.
pub fn merge_request_pr(command: &[String]) -> Option<u64> {
    match merge_argv(command) {
        Some(Some(n)) => Some(n),
        _ => None,
    }
}

/// The gh-side record: the proxy draft door and the PostToolUse hook feed
/// every delegated gh argv here, so an agent merge that never ran the fno
/// verb still leaves a `merge_requested` row naming the path.
pub fn record_request(command: &[String], cwd: Option<&str>, path: &str) {
    if !recording_enabled() {
        return;
    }
    let Some(intent) = merge_argv(command) else {
        return;
    };
    let session = crate::identity::ambient_agent_handle();
    let mut attrs = Map::new();
    attrs.insert("pr".into(), json!(intent));
    attrs.insert(
        "repo".into(),
        json!(crate::backlog::pr_link::resolve_current_repo_slug(cwd)
            .unwrap_or_else(|| "unknown-repo".into())),
    );
    attrs.insert("path".into(), json!(path));
    write_span("merge_requested", trace_for(session, "verb"), attrs);
}

/// Does the store already carry an fno merge record for this repo and PR?
/// A store that is truly absent answers false (nothing was ever recorded);
/// a store that exists but cannot be read answers true, so a read failure
/// never mislabels a merge as outside fno.
pub fn has_record(journal: &Path, repo: &str, pr: u64) -> bool {
    let store = crate::event_store::store_path(journal);
    if !store.exists() {
        return false;
    }
    let q = crate::event_store::EventQuery {
        types: vec!["decision_span".into()],
        repo: Some(repo.into()),
        pr_number: Some(pr as i64),
        ..Default::default()
    };
    match crate::event_store::query_events(journal, &q) {
        Ok(rows) => rows.iter().any(|r| {
            serde_json::from_str::<Value>(&r.line)
                .ok()
                .is_some_and(|row| {
                    matches!(
                        row["data"]["span_kind"].as_str(),
                        Some("merge_landed") | Some("merge_requested")
                    )
                })
        }),
        Err(_) => true,
    }
}

/// The reconcile record: a node closes on a PR whose merge the journal
/// never saw, so the merge happened outside fno (github.com, another
/// machine, an untracked path).
pub fn record_outside_merge(pr: u64, repo: &str, merged_at: Option<&str>, merge_sha: Option<&str>) {
    if !recording_enabled() {
        return;
    }
    let session = crate::identity::ambient_agent_handle();
    let mut attrs = Map::new();
    attrs.insert("pr".into(), json!(pr));
    attrs.insert("repo".into(), json!(repo));
    attrs.insert("merged_at".into(), json!(merged_at));
    attrs.insert("merge_sha".into(), json!(merge_sha));
    attrs.insert("path".into(), json!("reconcile"));
    write_span("merged_outside_fno", trace_for(session, "verb"), attrs);
}

/// The `graph-get` stdin door arm (`{"merge_provenance": {"hook": <the
/// PostToolUse payload>}}`), the draft door shape: the Bash hook
/// backstop. A merge argv that exited zero lands one `merge_requested`
/// row with path `hook`; everything else writes nothing.
pub fn run_hook_door(payload: &Value) -> Value {
    let hook = payload.get("merge_provenance").and_then(|m| m.get("hook"));
    let command = hook
        .and_then(|h| h.get("tool_input"))
        .and_then(|t| t.get("command"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    // Both exit-key spellings the real payload carries; absent reads 0,
    // the same reading claim-heartbeat.sh takes.
    let exit = hook
        .and_then(|h| h.get("tool_response"))
        .and_then(|r| {
            r.get("exit_code")
                .or_else(|| r.get("exitCode"))
                .and_then(Value::as_i64)
        })
        .unwrap_or(0);
    let cwd = hook
        .and_then(|h| h.get("cwd"))
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty());
    let tokens: Vec<String> = command.split_whitespace().map(str::to_string).collect();
    if exit == 0 {
        record_request(&tokens, cwd, "hook");
    }
    json!({ "recorded": exit == 0 && merge_argv(&tokens).is_some() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    /// The one span-recording contract: the argv grammar, the owner's
    /// outcome rows with their lane paths, the proxy and hook request rows,
    /// and the has_record read, all against this test's own journal.
    #[test]
    fn merge_provenance_records_one_span_per_hop() {
        let journal = TestJournal::opt_in();
        // The argv grammar: pr merge and the REST merge endpoint, the bare
        // `pr merge` (gh resolves the number), and strangers.
        assert_eq!(
            merge_request_pr(&argv(&["gh", "pr", "merge", "7"])),
            Some(7)
        );
        assert_eq!(
            merge_request_pr(&argv(&["pr", "merge", "7", "--squash"])),
            Some(7)
        );
        assert_eq!(
            merge_request_pr(&argv(&[
                "gh",
                "api",
                "-X",
                "PUT",
                "repos/o/r/pulls/7/merge"
            ])),
            Some(7)
        );
        assert_eq!(
            merge_request_pr(&argv(&[
                "gh",
                "api",
                "--method",
                "PUT",
                "repos/o/r/pulls/9/merge"
            ])),
            Some(9)
        );
        assert_eq!(
            merge_request_pr(&argv(&["gh", "api", "repos/o/r/pulls/7/merge"])),
            None
        );
        assert_eq!(merge_request_pr(&argv(&["gh", "pr", "list"])), None);
        assert_eq!(
            merge_request_pr(&argv(&["cd", "/x", "&&", "gh", "pr", "merge", "9"])),
            Some(9)
        );
        assert_eq!(merge_argv(&argv(&["pr", "merge"])), Some(None));
        assert_eq!(merge_argv(&argv(&["pr", "ready", "7"])), None);

        // The owner's outcomes: pr_merge, pr_watch (durable grant), and the
        // finalize arm (Arm with no PR) each land their own lane path.
        let request = Request {
            cwd: PathBuf::from("/tmp"),
            pr: Some(7),
            effect: Effect::Merge,
            approved: Some(true),
            auto_merge_source: None,
            require_checks: false,
            covered_head: None,
            decide_only: false,
            authority: None,
            accept_flake: false,
            supplied_verdict: None,
            supplied_counts: None,
            supplied_rerun_recovered: None,
            supplied_optional_unresolved: None,
            supplied_github_blockers: None,
            supplied_dispatch_hold: None,
            supplied_review_hold: None,
            supplied_facts: None,
        };
        let facts = PrFacts {
            number: 7,
            head_sha: "abc123".into(),
            head_ref: "feature/x".into(),
            base_ref: "main".into(),
            url: "https://github.com/o/r/pull/7".into(),
            body: None,
            state: "OPEN".into(),
            armed: false,
        };
        record_outcome(
            &request,
            &facts,
            &Outcome::Merged {
                head: "abc123".into(),
                note: None,
                cleanup_failure: None,
                merge_grant: None,
            },
        );
        let mut armed = request.clone();
        armed.effect = Effect::Arm;
        armed.pr = None;
        armed.authority = Some("durable_grant".into());
        record_outcome(
            &armed,
            &facts,
            &Outcome::Armed {
                head: "abc123".into(),
                merge_grant: None,
            },
        );
        let mut finalize = request.clone();
        finalize.effect = Effect::Arm;
        finalize.pr = None;
        record_outcome(
            &finalize,
            &facts,
            &Outcome::Armed {
                head: "abc123".into(),
                merge_grant: None,
            },
        );

        // The gh-side requests: the proxy's normalized argv and the hook
        // door, whose zero-exit gate reads both exit-key spellings.
        let cwd = journal.root().to_string_lossy().into_owned();
        record_request(&argv(&["pr", "merge", "7"]), Some(&cwd), "gh_proxy");
        let payload = json!({
            "merge_provenance": {
                "hook": {
                    "cwd": cwd,
                    "tool_input": {"command": "gh pr merge 8 --squash"},
                    "tool_response": {"exit_code": 0}
                }
            }
        });
        assert_eq!(run_hook_door(&payload)["recorded"], true);
        let refused = json!({
            "merge_provenance": {
                "hook": {
                    "cwd": "",
                    "tool_input": {"command": "gh pr merge 9"},
                    "tool_response": {"exitCode": 1}
                }
            }
        });
        assert_eq!(run_hook_door(&refused)["recorded"], false);
        let plain = json!({
            "merge_provenance": {
                "hook": {
                    "cwd": "",
                    "tool_input": {"command": "gh pr status 9"},
                    "tool_response": {"exit_code": 0}
                }
            }
        });
        assert_eq!(run_hook_door(&plain)["recorded"], false);

        let rows = journal.rows();
        let kind = |r: &Value, k: &str| r["data"]["span_kind"] == k;
        let landed: Vec<_> = rows.iter().filter(|r| kind(r, "merge_landed")).collect();
        assert_eq!(landed.len(), 1, "{rows:?}");
        assert_eq!(landed[0]["data"]["path"], "pr_merge");
        assert_eq!(landed[0]["data"]["pr"], 7);
        assert_eq!(landed[0]["data"]["repo"], "o/r");
        assert_eq!(landed[0]["data"]["head"], "abc123");
        let armed_rows: Vec<_> = rows.iter().filter(|r| kind(r, "merge_armed")).collect();
        assert_eq!(armed_rows.len(), 2, "{rows:?}");
        let paths: Vec<_> = armed_rows
            .iter()
            .map(|r| r["data"]["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"pr_watch"), "{paths:?}");
        assert!(paths.contains(&"finalize"), "{paths:?}");
        let requested: Vec<_> = rows.iter().filter(|r| kind(r, "merge_requested")).collect();
        assert_eq!(requested.len(), 2, "{rows:?}");
        assert_eq!(requested[0]["data"]["path"], "gh_proxy");
        assert_eq!(requested[0]["data"]["pr"], 7);
        assert_eq!(requested[1]["data"]["path"], "hook");
        assert_eq!(requested[1]["data"]["pr"], 8);

        // has_record: a recorded PR reads true, an unrecorded one false.
        assert!(has_record(&journal.journal(), "unknown-repo", 7));
        assert!(has_record(&journal.journal(), "unknown-repo", 8));
        assert!(!has_record(&journal.journal(), "unknown-repo", 9));
        assert!(!has_record(&journal.journal(), "nothing/here", 1));
    }
}
