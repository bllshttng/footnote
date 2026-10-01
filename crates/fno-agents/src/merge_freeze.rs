//! The scoped merge freeze: one record with a subject and an allow-list of
//! PRs, written by the team through the `authorized-merge` verb's op
//! transport (`{"op": "freeze-set"|"freeze-clear"|"freeze-check", ...}`).
//!
//! Two readers enforce it, both refusing an off-list PR with a receipt that
//! names the freeze: the merge owner's own gate (the same spot the fleet
//! breaker is read, covering `fno do pr merge` and every path that routes
//! through it) and the pr-watch merge arm's per-PR pre-check in Python,
//! which reads the same file for its early skip.
//!
//! Fail posture mirrors the fleet breaker: an unreadable record refuses
//! (an unreadable freeze must not read as "no freeze"); absence is a real
//! answer. A PR ON the allow-list merges normally - the freeze scopes a
//! release window, it is not a full stop.

use crate::paths::AgentsHome;
use serde_json::{json, Value};
use std::path::PathBuf;

/// The only schema version this reader understands.
pub const STATE_VERSION: u32 = 1;

/// The record's home, beside `fleet-stop.json`.
pub fn merge_freeze_json() -> PathBuf {
    record_path(&AgentsHome::from_env().root())
}

fn record_path(home: &std::path::Path) -> PathBuf {
    home.join("merge-freeze.json")
}

/// One freeze verdict for a PR: `Clear` admits; `Frozen` names the freeze;
/// `Unavailable` is an unreadable record (fail closed).
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Clear,
    Frozen { subject: String, set_by: String },
    Unavailable { detail: String },
}

pub(crate) fn verdict_for_in(home: &std::path::Path, pr: Option<u64>) -> Verdict {
    let path = record_path(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Verdict::Clear,
        Err(e) => {
            return Verdict::Unavailable {
                detail: format!("read {}: {e}", path.display()),
            }
        }
    };
    let v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            return Verdict::Unavailable {
                detail: format!("parse {}: {e}", path.display()),
            }
        }
    };
    if v.get("version").and_then(Value::as_u64) != Some(STATE_VERSION as u64) {
        return Verdict::Unavailable {
            detail: format!(
                "{} is not a version-{STATE_VERSION} freeze record",
                path.display()
            ),
        };
    }
    let subject = v
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or("unnamed freeze")
        .to_string();
    let set_by = v
        .get("set_by")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let allowed = v.get("allow").and_then(Value::as_array);
    match (pr, allowed) {
        (Some(pr), Some(list)) => {
            let on_list = list
                .iter()
                .filter_map(Value::as_i64)
                .any(|n| n == pr as i64);
            if on_list {
                Verdict::Clear
            } else {
                Verdict::Frozen { subject, set_by }
            }
        }
        // No PR resolved, or a record with no allow array: the freeze is
        // total. An allow-less record freezes everything, never nothing.
        _ => Verdict::Frozen { subject, set_by },
    }
}

/// The owner's gate: a merge- or arm-effect ask for an off-list PR refuses,
/// naming the freeze; an unreadable record refuses fail closed. `None`
/// admits.
pub(crate) fn refusal(pr: Option<u64>) -> Option<(i32, String)> {
    refusal_in(&AgentsHome::from_env().root(), pr)
}

fn refusal_in(home: &std::path::Path, pr: Option<u64>) -> Option<(i32, String)> {
    match verdict_for_in(home, pr) {
        Verdict::Clear => None,
        Verdict::Frozen { subject, set_by } => {
            let who = if set_by.is_empty() {
                String::new()
            } else {
                format!(" (set_by {set_by})")
            };
            let listing = pr
                .map(|n| format!("PR {n} is not on its allow-list"))
                .unwrap_or_else(|| "no PR resolved for this ask".to_string());
            Some((
                crate::spawn_gate::EXIT_FLEET_STOP,
                format!(
                    "refused: a merge freeze holds ({subject}{who}); {listing}; \
                     lift it with `{{\"op\": \"freeze-clear\", \"evidence\": ...}}` \
                     on the authorized-merge verb\n"
                ),
            ))
        }
        Verdict::Unavailable { detail } => Some((
            crate::spawn_gate::EXIT_FLEET_STOP_UNAVAILABLE,
            format!(
                "refused: the merge-freeze record is unreadable ({detail}); \
                 the merge fails closed\n"
            ),
        )),
    }
}

/// Run one freeze op from an `authorized-merge` payload. Same receipt
/// contract as the hold ops.
pub fn run(op: &str, payload: &Value) -> String {
    run_in(&AgentsHome::from_env().root(), op, payload)
}

fn run_in(home: &std::path::Path, op: &str, payload: &Value) -> String {
    match op.strip_prefix("freeze-").unwrap_or(op) {
        "set" => set_in(home, payload),
        "clear" => clear_in(home, payload),
        "check" => check_in(home, payload),
        other => receipt("refused", 2, format!("unknown freeze op: {other}")).to_string(),
    }
}

fn receipt(outcome: &str, code: i32, detail: impl Into<String>) -> Value {
    json!({"outcome": outcome, "exit_code": code, "detail": detail.into()})
}

fn payload_str<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

fn set_in(home: &std::path::Path, payload: &Value) -> String {
    let subject = payload_str(payload, "subject").unwrap_or("");
    let set_by = payload_str(payload, "set_by").unwrap_or("");
    for (name, value) in [("subject", subject), ("set-by", set_by)] {
        if value.trim().is_empty() {
            return receipt("refused", 2, format!("set needs a non-blank --{name}")).to_string();
        }
    }
    let allow: Vec<i64> = payload
        .get("allow")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    if allow.is_empty() {
        return receipt(
            "refused",
            2,
            "set needs a non-empty --allow list of PR numbers; a freeze with \
             nothing allowed stops every merge, which the fleet breaker owns",
        )
        .to_string();
    }
    if verdict_for_in(home, None) != Verdict::Clear {
        let (subject, _) = read_subject_in(home);
        return receipt(
            "refused",
            3,
            format!("a merge freeze is already active ({subject}); lift it with a freeze-clear op"),
        )
        .to_string();
    }
    let record = json!({
        "version": STATE_VERSION,
        "subject": subject,
        "set_by": set_by,
        "reason": payload_str(payload, "reason").unwrap_or(""),
        "allow": allow,
    });
    let path = record_path(home);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, format!("{record}\n")) {
        return receipt("error", 2, format!("freeze write failed: {e}")).to_string();
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        return receipt("error", 2, format!("freeze rename failed: {e}")).to_string();
    }
    // Readback: the writer proves the record says what it wrote.
    if verdict_for_in(home, allow.first().copied().map(|n| n as u64)) != Verdict::Clear {
        let _ = std::fs::remove_file(&path);
        return receipt(
            "error",
            1,
            "freeze readback refused its own first allowed PR; the record was removed",
        )
        .to_string();
    }
    let mut out = receipt("frozen", 0, "");
    if let Some(obj) = out.as_object_mut() {
        obj.insert("subject".into(), json!(subject));
        obj.insert("allow".into(), json!(allow));
    }
    out.to_string()
}

fn read_subject_in(home: &std::path::Path) -> (String, String) {
    match std::fs::read_to_string(record_path(home)) {
        Ok(text) => {
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            (
                v.get("subject")
                    .and_then(Value::as_str)
                    .unwrap_or("unnamed")
                    .to_string(),
                v.get("set_by")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            )
        }
        Err(_) => ("unreadable".to_string(), String::new()),
    }
}

fn clear_in(home: &std::path::Path, payload: &Value) -> String {
    let evidence = payload_str(payload, "evidence").unwrap_or("");
    if evidence.trim().is_empty() {
        return receipt("refused", 2, "clear needs a non-blank --evidence").to_string();
    }
    let path = record_path(home);
    if !path.exists() {
        return receipt("refused", 3, "no merge freeze is active; nothing to lift").to_string();
    }
    if let Err(e) = std::fs::remove_file(&path) {
        return receipt("error", 2, format!("freeze clear failed: {e}")).to_string();
    }
    let mut out = receipt("lifted", 0, "");
    if let Some(obj) = out.as_object_mut() {
        obj.insert("evidence".into(), json!(evidence));
    }
    out.to_string()
}

fn check_in(home: &std::path::Path, payload: &Value) -> String {
    let pr = payload.get("pr").and_then(Value::as_u64);
    match verdict_for_in(home, pr) {
        Verdict::Clear => receipt("clear", 0, "").to_string(),
        Verdict::Frozen { subject, set_by } => {
            receipt("frozen", 0, format!("{subject} {set_by}")).to_string()
        }
        Verdict::Unavailable { detail } => receipt("unavailable", 0, detail).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn home(test: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new_in(std::env::temp_dir()).unwrap();
        let root = dir.path().join(test);
        std::fs::create_dir_all(&root).unwrap();
        let clone = root.clone();
        (dir, clone)
    }

    #[test]
    fn freeze_gate_contract() {
        let (_dir, home) = home("freeze-absent");
        assert_eq!(verdict_for_in(&home, Some(42)), Verdict::Clear);
        assert!(refusal_in(&home, Some(42)).is_none());
        set_scope_contract();
        unreadable_fail_closed_contract();
        set_clear_lifecycle_contract();
    }

    fn set_scope_contract() {
        let (_dir, home) = home("freeze-scoped");
        let out = run_in(
            &home,
            "freeze-set",
            &json!({"subject": "rc freeze", "set_by": "team", "allow": [2739, 2740]}),
        );
        let r: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(r["outcome"], "frozen", "{out}");
        assert!(matches!(verdict_for_in(&home, Some(2739)), Verdict::Clear));
        assert!(matches!(
            verdict_for_in(&home, Some(2500)),
            Verdict::Frozen { ref subject, .. } if subject == "rc freeze"
        ));
        let (code, message) = refusal_in(&home, Some(2500)).unwrap();
        assert!(message.contains("rc freeze"), "{message}");
        assert!(message.contains("not on its allow-list"), "{message}");
        assert!(code != 0);
    }

    fn unreadable_fail_closed_contract() {
        let (_dir, home) = home("freeze-unreadable");
        std::fs::write(record_path(&home), "{").unwrap();
        assert!(matches!(
            verdict_for_in(&home, Some(42)),
            Verdict::Unavailable { .. }
        ));
        assert!(refusal_in(&home, Some(42)).is_some());
    }

    fn set_clear_lifecycle_contract() {
        let (_dir, home) = home("freeze-lift");
        let g = |extra: Value| {
            let mut base = json!({"subject": "s", "set_by": "team", "allow": [1]});
            for (k, v) in extra.as_object().unwrap() {
                base[k] = v.clone();
            }
            base
        };
        run_in(&home, "freeze-set", &g(json!({})));
        let second = run_in(&home, "freeze-set", &g(json!({"subject": "again"})));
        assert!(second.contains("already active"), "{second}");
        let no_evidence = run_in(&home, "freeze-clear", &json!({}));
        assert!(no_evidence.contains("--evidence"), "{no_evidence}");
        let lifted = run_in(&home, "freeze-clear", &json!({"evidence": "freeze lifted"}));
        let r: Value = serde_json::from_str(&lifted).unwrap();
        assert_eq!(r["outcome"], "lifted", "{lifted}");
        assert_eq!(verdict_for_in(&home, Some(1)), Verdict::Clear);
        let nothing = run_in(&home, "freeze-clear", &json!({"evidence": "again"}));
        assert!(nothing.contains("no merge freeze is active"), "{nothing}");
    }
}
