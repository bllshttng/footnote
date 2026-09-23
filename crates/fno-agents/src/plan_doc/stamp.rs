//! Stamp, graduate, and set-expected, ported 1:1 from `cli/src/fno/plan/_stamp.py`
//! commands. Exit codes match the Python module: 0 ok, 1 error (missing/parse),
//! 2 validation, 3 missing doc for set-expected.

use std::path::Path;

use serde_json::Value;

use super::codec::{self, Fields, ReadError, Value as Fv};
use super::lock::PlanDocLock;
use super::now_stamp;
use super::status::canonical_status;

/// Outcome of one stamp/graduate/set-expected op: the exit code the Python
/// module returned, plus the message for its stderr.
pub struct OpResult {
    pub exit: i32,
    pub message: String,
}

impl OpResult {
    fn ok() -> Self {
        OpResult {
            exit: 0,
            message: String::new(),
        }
    }
    fn fail(exit: i32, message: impl Into<String>) -> Self {
        OpResult {
            exit,
            message: message.into(),
        }
    }
}

/// True when value parses as an integer >= 1 (the first-writer-wins guard).
fn valid_count(value: Option<&Fv>) -> bool {
    match value {
        Some(Fv::Scalar(s)) => s.trim().parse::<i64>().map(|v| v >= 1).unwrap_or(false),
        _ => false,
    }
}

/// The graph node claim carried by the plan (`claims` scalar or first item).
fn plan_node_id(fields: &Fields) -> Option<String> {
    match fields.get("claims") {
        Some(Fv::Scalar(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Some(Fv::List(items)) | Some(Fv::BlockList(items)) => {
            items.first().map(|s| s.trim().to_string())
        }
        _ => None,
    }
}

/// The doc's urls as a string list (a bare scalar reads as a one-item list).
fn plan_urls(fields: &Fields) -> Vec<String> {
    match fields.get("urls") {
        Some(Fv::Scalar(s)) => {
            if s.is_empty() {
                Vec::new()
            } else {
                vec![s.clone()]
            }
        }
        Some(Fv::List(items)) | Some(Fv::BlockList(items)) => items.clone(),
        _ => Vec::new(),
    }
}

/// The doc's expected_url_count parsed as i64 >= 1, else None.
fn plan_expected_url_count(fields: &Fields) -> Option<i64> {
    match fields.get("expected_url_count") {
        Some(Fv::Scalar(s)) => s.trim().parse::<i64>().ok().filter(|v| *v >= 1),
        _ => None,
    }
}

/// The latest session id from session_ids (scalar or last list item).
fn latest_plan_session(fields: &Fields) -> Option<String> {
    match fields.get("session_ids") {
        Some(Fv::Scalar(s)) if !s.is_empty() => Some(s.clone()),
        Some(Fv::List(items)) | Some(Fv::BlockList(items)) => items.last().cloned(),
        _ => None,
    }
}

/// Report telemetry failure without changing the durable plan outcome.
fn emit_plan_event(events_path: Option<&Path>, event_type: &str, data: Value) {
    let Some(path) = events_path else { return };
    let mut env = serde_json::Map::new();
    env.insert("ts".to_string(), Value::String(now_stamp()));
    env.insert("type".to_string(), Value::String(event_type.to_string()));
    env.insert("source".to_string(), Value::String("target".to_string()));
    env.insert("data".to_string(), data);
    let line = match serde_json::to_string(&Value::Object(env)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("warning: failed to emit {event_type}: {e}");
            return;
        }
    };
    if let Err(e) = crate::event_store::append_envelope(path, &line, None) {
        eprintln!("warning: failed to emit {event_type}: {e}");
    }
}

/// Stamp a plan with shipping metadata (the `stamp` verb's body, lock included).
pub fn cmd_stamp(
    plan_path: &Path,
    session_id: &str,
    urls: &[String],
    expected_url_count: Option<u32>,
    dry_run: bool,
    events_path: Option<&Path>,
) -> OpResult {
    if let Some(n) = expected_url_count {
        if n < 1 {
            return OpResult::fail(
                2,
                format!("error: --expected-url-count must be >= 1 (got {n})"),
            );
        }
    }
    // Serialize the full read-modify-write against a concurrent progress append.
    let guard = match PlanDocLock::acquire(plan_path, std::time::Duration::from_secs(2)) {
        Ok(g) => g,
        Err(e) => return OpResult::fail(1, e),
    };
    let result = do_stamp(
        plan_path,
        session_id,
        urls,
        expected_url_count,
        dry_run,
        events_path,
    );
    drop(guard);
    result
}

/// The read-modify-write core of stamp, called under the lock.
fn do_stamp(
    plan_path: &Path,
    session_id: &str,
    urls: &[String],
    expected_url_count: Option<u32>,
    dry_run: bool,
    events_path: Option<&Path>,
) -> OpResult {
    let (target, mut fields, rest) = match codec::read_plan_file(plan_path) {
        Ok(ok) => ok,
        Err(e) => return OpResult::fail(1, format!("error: {e}")),
    };
    let now_utc = now_stamp();
    let status_from: Option<String> = match fields.get("status") {
        Some(Fv::Scalar(s)) => Some(s.clone()),
        _ => None,
    };
    // Idempotency: check if this (session_id, url) pair is already present.
    // A list read as block form keeps block form on rewrite: the BlockList
    // marker exists so the vault's formatter and this writer never oscillate.
    let urls_block = matches!(fields.get("urls"), Some(Fv::BlockList(_)));
    let sids_block = matches!(fields.get("session_ids"), Some(Fv::BlockList(_)));
    let mut existing_urls = plan_urls(&fields);
    let mut existing_sids: Vec<String> = match fields.get("session_ids") {
        Some(Fv::Scalar(s)) => {
            if s.is_empty() {
                Vec::new()
            } else {
                vec![s.clone()]
            }
        }
        Some(Fv::List(items)) | Some(Fv::BlockList(items)) => items.clone(),
        _ => Vec::new(),
    };
    let all_urls_present = urls.iter().all(|u| existing_urls.contains(u));
    let sid_present = existing_sids.iter().any(|s| s == session_id);

    // Fully idempotent - no-op.
    if all_urls_present && sid_present {
        if !dry_run {
            emit_plan_event(
                events_path,
                "plan_stamped",
                stamp_event_data(
                    &target,
                    session_id,
                    "idempotent_noop",
                    &fields,
                    status_from.clone(),
                    Some("stamp already contains the session and URL"),
                ),
            );
        }
        return OpResult::ok();
    }

    // Not a full duplicate: merge new data in.
    let shipped_at_present = match fields.get("shipped_at") {
        Some(Fv::Scalar(s)) => !s.is_empty(),
        _ => false,
    };
    if !shipped_at_present {
        fields.insert("shipped_at", Fv::Scalar(now_utc.clone()));
    }
    for url in urls {
        if !existing_urls.contains(url) {
            existing_urls.push(url.clone());
        }
    }
    if urls_block {
        fields.insert("urls", Fv::BlockList(existing_urls));
    } else {
        fields.insert("urls", Fv::List(existing_urls));
    }
    if !existing_sids.iter().any(|s| s == session_id) {
        existing_sids.push(session_id.to_string());
    }
    if sids_block {
        fields.insert("session_ids", Fv::BlockList(existing_sids));
    } else {
        fields.insert("session_ids", Fv::List(existing_sids));
    }
    // Status: always in_review on stamp (graduate upgrades to done), compared
    // canonically so a retired spelling keeps its bytes.
    let canonical = canonical_status(status_from.as_deref());
    if canonical != "in_review" && canonical != "done" {
        fields.insert("status", Fv::Scalar("in_review".to_string()));
    }
    // First-writer-wins expected_url_count; a malformed value self-heals.
    if expected_url_count.is_some() && !valid_count(fields.get("expected_url_count")) {
        fields.insert(
            "expected_url_count",
            Fv::Scalar(expected_url_count.unwrap().to_string()),
        );
    }
    if let Err(e) = codec::write_plan_file(&target, &fields, &rest) {
        return OpResult::fail(1, format!("error: {e}"));
    }
    if !dry_run {
        emit_plan_event(
            events_path,
            "plan_stamped",
            stamp_event_data(&target, session_id, "stamped", &fields, status_from, None),
        );
    }
    OpResult::ok()
}

/// Build the `data` payload for plan_stamped/plan_graduated, matching the
/// fields `fno.events.plan_stamped`/`plan_graduated` emit. Optional fields
/// are omitted when None, exactly as the Python builders do.
fn stamp_event_data(
    target: &Path,
    session_id: &str,
    outcome: &str,
    fields: &Fields,
    status_from: Option<String>,
    reason: Option<&str>,
) -> Value {
    let mut data = serde_json::Map::new();
    data.insert(
        "plan_path".into(),
        Value::String(target.to_string_lossy().into_owned()),
    );
    data.insert("session_id".into(), Value::String(session_id.to_string()));
    data.insert("outcome".into(), Value::String(outcome.to_string()));
    if let Some(nid) = plan_node_id(fields) {
        data.insert("node_id".into(), Value::String(nid));
    }
    if let Some(from) = status_from {
        data.insert("status_from".into(), Value::String(from));
    }
    let status_to = match fields.get("status") {
        Some(Fv::Scalar(s)) => Some(s.clone()),
        _ => None,
    };
    if let Some(to) = status_to {
        data.insert("status_to".into(), Value::String(to));
    }
    let urls = plan_urls(fields);
    if !urls.is_empty() {
        data.insert(
            "urls".into(),
            Value::Array(urls.into_iter().map(Value::String).collect()),
        );
    }
    if let Some(count) = plan_expected_url_count(fields) {
        data.insert("expected_url_count".into(), Value::Number(count.into()));
    }
    if let Some(reason) = reason {
        data.insert("reason".into(), Value::String(reason.to_string()));
    }
    Value::Object(data)
}

/// Flip status: in_review -> done when enough URLs have accumulated.
pub fn cmd_graduate(plan_path: &Path, dry_run: bool, events_path: Option<&Path>) -> OpResult {
    let guard = match PlanDocLock::acquire(plan_path, std::time::Duration::from_secs(2)) {
        Ok(g) => g,
        Err(e) => return OpResult::fail(1, e),
    };
    let result = do_graduate(plan_path, dry_run, events_path);
    drop(guard);
    result
}

fn do_graduate(plan_path: &Path, dry_run: bool, events_path: Option<&Path>) -> OpResult {
    let (target, mut fields, rest) = match codec::read_plan_file(plan_path) {
        Ok(ok) => ok,
        Err(e) => return OpResult::fail(1, format!("error: {e}")),
    };
    let status = match fields.get("status") {
        Some(Fv::Scalar(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };
    let status_from = status.clone();
    let session_id = latest_plan_session(&fields);
    let urls = plan_urls(&fields);
    let _expected_count = plan_expected_url_count(&fields);
    if canonical_status(status.as_deref()) != "in_review" {
        if !dry_run {
            let mut data = stamp_event_data(
                &target,
                session_id.as_deref().unwrap_or(""),
                "idempotent_noop",
                &fields,
                status_from.clone(),
                Some(&format!(
                    "status is {}",
                    status.as_deref().unwrap_or("(unset)")
                )),
            );
            // The graduate no-op omits session_id when the doc carries none.
            if session_id.is_none() {
                if let Some(obj) = data.as_object_mut() {
                    obj.remove("session_id");
                }
            }
            emit_plan_event(events_path, "plan_graduated", data);
        }
        return OpResult::ok();
    }

    let expected_raw = match fields.get("expected_url_count") {
        Some(Fv::Scalar(s)) => s.clone(),
        _ => "1".to_string(),
    };
    let expected: i64 = match expected_raw.trim().parse::<i64>() {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "warning: expected_url_count={expected_raw:?} is not an integer; \
                 defaulting to 1. Cross-project plans may graduate early if this \
                 was set by an earlier writer."
            );
            1
        }
    };

    if (urls.len() as i64) >= expected {
        fields.insert("status", Fv::Scalar("done".to_string()));
        let done_at_present = match fields.get("done_at") {
            Some(Fv::Scalar(s)) => !s.is_empty(),
            _ => false,
        };
        if !done_at_present {
            fields.insert("done_at", Fv::Scalar(now_stamp()));
        }
        if let Err(e) = codec::write_plan_file(&target, &fields, &rest) {
            return OpResult::fail(1, format!("error: {e}"));
        }
        if !dry_run {
            let mut data = stamp_event_data(
                &target,
                session_id.as_deref().unwrap_or(""),
                "graduated",
                &fields,
                status_from,
                None,
            );
            if let Some(obj) = data.as_object_mut() {
                obj.insert("expected_url_count".into(), Value::Number(expected.into()));
            }
            if session_id.is_none() {
                if let Some(obj) = data.as_object_mut() {
                    obj.remove("session_id");
                }
            }
            emit_plan_event(events_path, "plan_graduated", data);
        }
    } else if !dry_run {
        let mut data = stamp_event_data(
            &target,
            session_id.as_deref().unwrap_or(""),
            "not_met",
            &fields,
            status_from,
            Some(&format!(
                "{} URL(s) present; {} required",
                urls.len(),
                expected
            )),
        );
        if let Some(obj) = data.as_object_mut() {
            obj.insert("expected_url_count".into(), Value::Number(expected.into()));
        }
        if session_id.is_none() {
            if let Some(obj) = data.as_object_mut() {
                obj.remove("session_id");
            }
        }
        emit_plan_event(events_path, "plan_graduated", data);
    }

    OpResult::ok()
}

/// Authoritatively write expected_url_count. Unlike stamp's first-writer-wins,
/// this OVERWRITES: decompose is the authority on the group count. Exit 3 on a
/// missing doc (benign for decompose); exit 2 on count < 1; no event.
pub fn cmd_set_expected(plan_path: &Path, count: u32, dry_run: bool) -> OpResult {
    if count < 1 {
        return OpResult::fail(2, format!("error: --count must be >= 1 (got {count})"));
    }
    let (target, mut fields, rest) = match codec::read_plan_file(plan_path) {
        Ok(ok) => ok,
        Err(ReadError::NotFound(e)) => return OpResult::fail(3, format!("error: {e}")),
        Err(e) => return OpResult::fail(1, format!("error: {e}")),
    };
    fields.insert("expected_url_count", Fv::Scalar(count.to_string()));
    if dry_run {
        return OpResult::ok();
    }
    match codec::write_plan_file(&target, &fields, &rest) {
        Ok(()) => OpResult::ok(),
        Err(e) => OpResult::fail(1, format!("error: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fno-plan-doc-stamp-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fixture() -> &'static str {
        "---\ntitle: Char test\ncreated: 2026-05-01\nscope: single-project\nproject: fno\nexpected_url_count: 1\nkill_criteria:\n  - name: iteration_ceiling\n    predicate: iteration > 10\n    reason: too many\n---\n\n# Body content\n"
    }

    #[test]
    fn stamp_then_graduate_contract() {
        let dir = tmp_dir("contract");
        let doc = dir.join("plan.md");
        std::fs::write(&doc, fixture()).unwrap();
        let r = cmd_stamp(
            &doc,
            "SID-X",
            &["https://example.com/pull/7".to_string()],
            None,
            false,
            None,
        );
        assert_eq!(r.exit, 0, "{}", r.message);
        let text = std::fs::read_to_string(&doc).unwrap();
        assert!(text.contains("status: in_review"));
        assert!(text.contains("shipped_at:"));
        assert!(text.contains("https://example.com/pull/7"));
        assert!(text.contains("SID-X"));
        assert!(text.contains("iteration_ceiling"));

        let r = cmd_graduate(&doc, false, None);
        assert_eq!(r.exit, 0, "{}", r.message);
        let text = std::fs::read_to_string(&doc).unwrap();
        assert!(text.contains("status: done"));
        assert!(text.contains("iteration_ceiling"));
    }

    #[test]
    fn set_expected_contract_and_guards() {
        let dir = tmp_dir("setexp");
        let doc = dir.join("design.md");
        std::fs::write(&doc, "---\ntitle: Epic\nstatus: draft\n---\n# body\n").unwrap();
        let r = cmd_set_expected(&doc, 3, false);
        assert_eq!(r.exit, 0, "{}", r.message);
        assert!(std::fs::read_to_string(&doc)
            .unwrap()
            .contains("expected_url_count: 3"));

        let original = "---\ntitle: Epic\nstatus: draft\n---\n# body\n";
        std::fs::write(&doc, original).unwrap();
        let r = cmd_set_expected(&doc, 0, false);
        assert_ne!(r.exit, 0);
        assert_eq!(std::fs::read_to_string(&doc).unwrap(), original);

        // Malformed frontmatter: non-zero exit, doc unchanged.
        let original = "---\nstatus: draft\n  stray: nested\n---\n# body\n";
        std::fs::write(&doc, original).unwrap();
        let r = cmd_set_expected(&doc, 3, false);
        assert_ne!(r.exit, 0);
        assert_eq!(std::fs::read_to_string(&doc).unwrap(), original);
    }

    #[test]
    fn stamp_rejects_expected_url_count_below_one() {
        let dir = tmp_dir("belowone");
        let doc = dir.join("design.md");
        let original = "---\nstatus: draft\n---\n# body\n";
        std::fs::write(&doc, original).unwrap();
        let r = cmd_stamp(
            &doc,
            "sid",
            &["https://x/pr/1".to_string()],
            Some(0),
            false,
            None,
        );
        assert_eq!(r.exit, 2);
        assert_eq!(std::fs::read_to_string(&doc).unwrap(), original);
    }

    #[test]
    fn stamp_leaves_a_retired_spelling_in_place() {
        let dir = tmp_dir("retired");
        let doc = dir.join("plan.md");
        std::fs::write(
            &doc,
            fixture().replace(
                "scope: single-project",
                "scope: single-project\nstatus: shipped",
            ),
        )
        .unwrap();
        let r = cmd_stamp(
            &doc,
            "SID-Y",
            &["https://example.com/pull/8".to_string()],
            None,
            false,
            None,
        );
        assert_eq!(r.exit, 0, "{}", r.message);
        let text = std::fs::read_to_string(&doc).unwrap();
        assert!(text.contains("status: shipped"));
        assert!(!text.contains("status: in_review"));
        assert!(text.contains("SID-Y"));
        let r = cmd_graduate(&doc, false, None);
        assert_eq!(r.exit, 0);
        assert!(std::fs::read_to_string(&doc)
            .unwrap()
            .contains("status: done"));
    }

    #[test]
    fn first_writer_wins_expected_url_count() {
        let dir = tmp_dir("fww");
        let doc = dir.join("design.md");
        std::fs::write(
            &doc,
            "---\nstatus: draft\nexpected_url_count: 3\n---\n# body\n",
        )
        .unwrap();
        let r = cmd_stamp(
            &doc,
            "sid-1",
            &["https://x/pr/1".to_string()],
            Some(1),
            false,
            None,
        );
        assert_eq!(r.exit, 0, "{}", r.message);
        let text = std::fs::read_to_string(&doc).unwrap();
        assert!(text.contains("expected_url_count: 3"));
        assert!(!text.contains("expected_url_count: 1"));
    }

    #[test]
    fn stamp_sets_expected_when_absent_and_overwrites_malformed() {
        let dir = tmp_dir("absent");
        let doc = dir.join("design.md");
        std::fs::write(&doc, "---\nstatus: draft\n---\n# body\n").unwrap();
        let r = cmd_stamp(
            &doc,
            "sid-1",
            &["https://x/pr/1".to_string()],
            Some(2),
            false,
            None,
        );
        assert_eq!(r.exit, 0);
        assert!(std::fs::read_to_string(&doc)
            .unwrap()
            .contains("expected_url_count: 2"));

        std::fs::write(
            &doc,
            "---\nstatus: draft\nexpected_url_count: not-a-number\n---\n# body\n",
        )
        .unwrap();
        let r = cmd_stamp(
            &doc,
            "sid-1",
            &["https://x/pr/1".to_string()],
            Some(2),
            false,
            None,
        );
        assert_eq!(r.exit, 0);
        let text = std::fs::read_to_string(&doc).unwrap();
        assert!(text.contains("expected_url_count: 2"));
        assert!(!text.contains("not-a-number"));
    }

    #[test]
    fn graduate_group_fragment_and_missing_docs() {
        let dir = tmp_dir("group");
        let doc = dir.join("design.md");
        std::fs::write(
            &doc,
            "---\nstatus: shipped\nurls: [https://x/1]\nexpected_url_count: 1\n---\n# body\n",
        )
        .unwrap();
        let fragment = PathBuf::from(format!("{}#group-backend", doc.display()));
        let r = cmd_graduate(&fragment, false, None);
        assert_eq!(r.exit, 0, "{}", r.message);
        assert!(std::fs::read_to_string(&doc)
            .unwrap()
            .contains("status: done"));

        let missing = dir.join("nope.md");
        let r = cmd_set_expected(&missing, 3, false);
        assert_eq!(r.exit, 3);
        assert!(!r.message.is_empty());
    }

    #[test]
    fn stamp_accumulates_urls_and_session_ids() {
        let dir = tmp_dir("accum");
        let doc = dir.join("plan.md");
        std::fs::write(&doc, "---\nstatus: draft\n---\n# body\n").unwrap();
        let r = cmd_stamp(
            &doc,
            "s1",
            &["https://x/pr/1".to_string()],
            None,
            false,
            None,
        );
        assert_eq!(r.exit, 0);
        let r = cmd_stamp(
            &doc,
            "s2",
            &["https://x/pr/2".to_string()],
            None,
            false,
            None,
        );
        assert_eq!(r.exit, 0);
        let text = std::fs::read_to_string(&doc).unwrap();
        assert!(text.contains("urls: [https://x/pr/1, https://x/pr/2]"));
        assert!(text.contains("session_ids: [s1, s2]"));
    }

    #[test]
    fn stamp_idempotent_repeat_is_a_noop() {
        let dir = tmp_dir("idem");
        let doc = dir.join("plan.md");
        std::fs::write(&doc, "---\nstatus: draft\n---\n# body\n").unwrap();
        let r = cmd_stamp(
            &doc,
            "s1",
            &["https://x/pr/1".to_string()],
            None,
            false,
            None,
        );
        assert_eq!(r.exit, 0);
        let before = std::fs::read_to_string(&doc).unwrap();
        let r = cmd_stamp(
            &doc,
            "s1",
            &["https://x/pr/1".to_string()],
            None,
            false,
            None,
        );
        assert_eq!(r.exit, 0);
        assert_eq!(std::fs::read_to_string(&doc).unwrap(), before);
    }

    #[test]
    fn stamp_preserves_block_form_of_existing_lists() {
        let dir = tmp_dir("keepblock");
        let doc = dir.join("plan.md");
        std::fs::write(
            &doc,
            "---\nstatus: draft\nurls:\n  - https://x/1\nsession_ids:\n  - a\n---\n# body\n",
        )
        .unwrap();
        let r = cmd_stamp(&doc, "b", &["https://x/2".to_string()], None, false, None);
        assert_eq!(r.exit, 0, "{}", r.message);
        let text = std::fs::read_to_string(&doc).unwrap();
        assert!(
            text.contains("urls:\n  - https://x/1\n  - https://x/2"),
            "{text}"
        );
        assert!(text.contains("session_ids:\n  - a\n  - b"), "{text}");
    }

    #[test]
    fn set_expected_overwrites_existing_and_creates_frontmatter() {
        let dir = tmp_dir("overwr");
        let doc = dir.join("design.md");
        std::fs::write(
            &doc,
            "---\nstatus: draft\nexpected_url_count: 2\n---\n# body\n",
        )
        .unwrap();
        let r = cmd_set_expected(&doc, 4, false);
        assert_eq!(r.exit, 0);
        let text = std::fs::read_to_string(&doc).unwrap();
        assert!(text.contains("expected_url_count: 4"));
        assert!(!text.contains("expected_url_count: 2"));

        let doc2 = dir.join("plain.md");
        std::fs::write(&doc2, "# Just a body\n\nsome prose\n").unwrap();
        let r = cmd_set_expected(&doc2, 2, false);
        assert_eq!(r.exit, 0);
        let text = std::fs::read_to_string(&doc2).unwrap();
        assert!(text.starts_with("---\n"));
        assert!(text.contains("expected_url_count: 2"));
        assert!(text.contains("# Just a body"));
    }
}
