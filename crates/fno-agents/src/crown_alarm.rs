//! The empty-crown alarm: a crowned scope whose board still holds ready work
//! and which no live session holds is a silent stall. One finding folded into
//! the `arm_watch` tick, so dead arms and an empty crown land in ONE page.
//! The alarm notifies; it never gates dispatch and never takes a crown.

use crate::stuck_work::Finding;
use serde_json::Value;
use std::path::Path;

/// A crown mid-handoff is briefly empty by design, so the alarm waits this
/// long from the first tick that saw the scope empty. The number is not
/// invented: it is the floor `stuck_work` already applies to a hung verb
/// (its `DEFAULT_HUNG_FLOOR_S`), the `notify.arm_failing_after_s` default the
/// confirmed send already pages at, two king_wake beats, and six arm_watch
/// beats. One control plane, one meaning of "long enough".
// ponytail: the first-seen stamp lives in the notify signal store, so a
// restart delays the page by one grace and never suppresses it; a durable
// stamp needs an event the crown writes to mark the turn to empty. Build
// that only when a restart-correlated miss is measured.
pub const CROWN_EMPTY_GRACE_S: u64 = 1800;

/// The court read's wall budget. The live read measured 5.80s over a
/// 2518-node scope, and a busy fleet stretches past 30s of wall clock.
const COURT_READ_BUDGET_S: u64 = 30;

fn s_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// One read of the court payload: the findings whose grace has run, plus a
/// short note naming the states a reader should still see (a scope inside
/// its grace). An unreadable instrument is `Err`, never an empty list: a
/// blind read must not read as "nothing is wrong".
pub fn evaluate(
    payload: &Value,
    store: &Path,
    now_unix: u64,
) -> Result<(Vec<Finding>, String), String> {
    if payload.get("registry_readable").and_then(Value::as_bool) != Some(true) {
        let reason = payload
            .pointer("/summary/reason")
            .and_then(Value::as_str)
            .unwrap_or("the registry could not be read");
        return Err(format!("registry unreadable: {reason}"));
    }
    if payload
        .pointer("/summary/sweep_ran")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err(
            "the manifest-only sweep did not run, so a zero answer is an absence".to_string(),
        );
    }
    let crowns = payload
        .get("crowns")
        .and_then(Value::as_array)
        .ok_or_else(|| "the court payload carries no crowns list".to_string())?;
    let mut findings = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for crown in crowns {
        let Some(scope) = s_str(crown, "scope") else {
            continue;
        };
        // The fold is the instrument for the board half. One unreadable
        // scope fails the whole read, never a clean empty beside it.
        let fold = crown
            .get("scope_nodes")
            .ok_or_else(|| format!("scope {scope} carries no fold"))?;
        if s_str(fold, "status") != Some("ok") {
            let reason = s_str(fold, "reason").unwrap_or("the fold did not run");
            return Err(format!("scope {scope} fold unread: {reason}"));
        }
        let unclaimed: Vec<String> = fold
            .pointer("/stuck/unclaimed")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let key = format!("crown_empty:{scope}");
        seen.insert(key.clone());
        let empty = s_str(crown, "status") == Some("manifest-only");
        if empty && !unclaimed.is_empty() {
            let age_s = match crate::operator_notice::first_seen_age_s(store, &key, now_unix) {
                Some(age_s) => age_s,
                None => {
                    crate::operator_notice::mark_once(store, &key, "empty");
                    0
                }
            };
            if age_s >= CROWN_EMPTY_GRACE_S {
                let threshold_min = fold
                    .pointer("/stuck/threshold_minutes")
                    .and_then(Value::as_u64)
                    .unwrap_or(60);
                let first_seen = now_unix.saturating_sub(age_s);
                findings.push(empty_crown_finding(
                    crown,
                    scope,
                    age_s,
                    threshold_min,
                    first_seen,
                    &unclaimed,
                ));
            } else {
                notes.push(format!(
                    "crown {scope} empty {age_s}s into a {CROWN_EMPTY_GRACE_S}s grace: {}s more",
                    CROWN_EMPTY_GRACE_S - age_s
                ));
            }
        } else {
            // The scope stopped reading empty: recovery is the designed
            // quiet, and the next episode starts a fresh clock.
            crate::operator_notice::forget_at(store, &key);
        }
    }
    // A readable payload is the authority on which scopes exist: a clock
    // whose scope left it is stale and would page a fresh handoff instantly.
    // The Err paths above return before this, so a blind read erases
    // nothing.
    for key in crate::operator_notice::keys_with_prefix(store, "crown_empty:") {
        if !seen.contains(&key) {
            crate::operator_notice::forget_at(store, &key);
        }
    }
    Ok((findings, notes.join("; ")))
}

fn empty_crown_finding(
    crown: &Value,
    scope: &str,
    age_s: u64,
    threshold_min: u64,
    first_seen: u64,
    unclaimed: &[String],
) -> Finding {
    let holder = s_str(crown, "holder").unwrap_or("unknown holder");
    let manifest = match s_str(crown, "manifest_path") {
        Some(path) => format!("crown on manifest {path}"),
        None => "crown on the manifest".to_string(),
    };
    Finding {
        kind: "empty_crown",
        key: format!("crown_empty:{scope}@{first_seen}"),
        line: format!(
            "no king on scope {scope}: {manifest}, holder {holder}, 0 live rows; \
             empty {}m against a {}m grace; {} ready over {threshold_min}m with no \
             worker ({}); respawn: fno agents spawn --crown {scope} --succeed; \
             this notifies only, it gates no dispatch and takes no crown",
            age_s / 60,
            CROWN_EMPTY_GRACE_S / 60,
            unclaimed.len(),
            crate::court_fold::named_ids(unclaimed),
        ),
        root: None,
        holder: Some(holder.to_string()),
        claim_key: None,
    }
}

/// The daemon-facing read: shell the Python court verb, then judge its
/// payload. The Python verb is the one decider for the held list; a native
/// second builder of the held list would be the dual implementation the
/// port law refuses.
pub fn collect(config_cwd: &Path) -> Result<(Vec<Finding>, String), String> {
    let payload = read_court_payload(config_cwd);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    collect_from(
        &payload,
        &crate::operator_notice::notify_signals_path(),
        now,
    )
}

/// The judge over an already-read payload: the same verdict `collect`
/// answers, with the read handed in, so one court read feeds both the
/// crown alarm and the settle pass.
pub(crate) fn collect_from(
    payload: &Result<Value, String>,
    store: &Path,
    now_unix: u64,
) -> Result<(Vec<Finding>, String), String> {
    match payload {
        Err(reason) => Err(reason.clone()),
        Ok(payload) => evaluate(payload, store, now_unix),
    }
}

pub(crate) fn read_court_payload(config_cwd: &Path) -> Result<Value, String> {
    let fno = std::env::var_os("FNO_BIN").unwrap_or_else(|| std::ffi::OsString::from("fno"));
    let mut cmd = std::process::Command::new(&fno);
    cmd.args(["agents", "court", "--nodes"])
        .current_dir(config_cwd)
        .stdin(std::process::Stdio::null());
    let out = crate::bounded_cmd::output_with_timeout(cmd, COURT_READ_BUDGET_S)
        .ok_or_else(|| format!("the court read timed out after {COURT_READ_BUDGET_S}s"))?;
    court_payload_from(out.status.success(), &out.stdout, &out.stderr)
}

/// The read's verdict over one child's output: the parsed payload, or the
/// fault that says why the board is unknown - never an empty, clear board.
fn court_payload_from(success: bool, stdout: &[u8], stderr: &[u8]) -> Result<Value, String> {
    if !success {
        let stderr = String::from_utf8_lossy(stderr);
        return Err(format!(
            "the court read failed: {}",
            crate::king_checkin::stderr_cause(&stderr)
        ));
    }
    serde_json::from_slice(stdout).map_err(|e| format!("the court payload did not parse: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    const NOW: u64 = 1_788_523_200; // 2026-09-04T12:00 to the second

    fn stuck(unclaimed: &[&str]) -> Value {
        json!({"unclaimed": unclaimed, "blocked": [], "unproven_claim": [],
               "in_review": [], "blind": [], "threshold_minutes": 60})
    }

    fn fold(unclaimed: &[&str]) -> Value {
        json!({"status": "ok", "crowns": [], "nodes": [], "stuck": stuck(unclaimed)})
    }

    fn manifest_crown(scope: &str, fold: Value) -> Value {
        json!({
            "holder": "sess-uuid-1", "level": 1, "scope": scope,
            "grantor": "human", "status": "manifest-only", "agree": Value::Null,
            "crown_source": "manifest", "manifest_path": "/spaces/w/manifest",
            "scope_nodes": fold,
        })
    }

    fn payload(crowns: Value) -> Value {
        json!({
            "crowns": crowns, "conflicts": [], "registry_readable": true,
            "graph_readable": true,
            "summary": {"sweep_ran": true, "total": 1, "manifest_only": 1},
        })
    }

    fn temp_store(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("fno-crown-alarm-{name}-{}", std::process::id()))
    }

    fn seed(path: &Path, scope: &str, ts: &str) {
        std::fs::write(
            path,
            format!(r#"{{"crown_empty:{scope}": {{"token": "empty", "ts": "{ts}"}}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn an_empty_crown_over_ready_work_raises_past_the_grace() {
        let store = temp_store("raise");
        seed(&store, "alpha", "2026-09-04T11:30:00Z");
        let p = payload(json!([manifest_crown("alpha", fold(&["x-1"]))]));
        let (findings, note) = evaluate(&p, &store, NOW).unwrap();
        assert_eq!(note, "");
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.kind, "empty_crown");
        assert!(f.key.starts_with("crown_empty:alpha@"));
        let line = &f.line;
        // The four contract clauses, as a conjunction of open markers: no
        // terminator is pinned, so a correctly added clause cannot fail this.
        assert!(line.contains("no king on scope alpha"), "{line}");
        assert!(
            line.contains("crown on manifest /spaces/w/manifest"),
            "{line}"
        );
        assert!(line.contains("holder sess-uuid-1"), "{line}");
        assert!(line.contains("0 live rows"), "{line}");
        assert!(line.contains("empty 30m against a 30m grace"), "{line}");
        assert!(line.contains("ready over 60m with no worker"), "{line}");
        assert!(line.contains("x-1"), "{line}");
        assert!(
            line.contains("respawn: fno agents spawn --crown alpha --succeed"),
            "{line}"
        );
        assert!(line.contains("gates no dispatch"), "{line}");
        std::fs::remove_file(&store).ok();
    }

    #[test]
    fn a_crown_that_is_not_manifest_only_never_raises() {
        let store = temp_store("live");
        seed(&store, "alpha", "2026-09-04T11:30:00Z");
        let mut crown = manifest_crown("alpha", fold(&["x-1"]));
        crown["status"] = json!("live");
        let (findings, _) = evaluate(&payload(json!([crown])), &store, NOW).unwrap();
        assert!(findings.is_empty());
        // The scope stopped reading empty: the clock is gone.
        let text = std::fs::read_to_string(&store).unwrap();
        assert!(!text.contains("crown_empty:alpha"), "{text}");
        std::fs::remove_file(&store).ok();
    }

    #[test]
    fn an_empty_crown_with_no_stuck_work_raises_nothing() {
        let store = temp_store("quiet");
        seed(&store, "alpha", "2026-09-04T11:30:00Z");
        let p = payload(json!([manifest_crown("alpha", fold(&[]))]));
        let (findings, _) = evaluate(&p, &store, NOW).unwrap();
        assert!(findings.is_empty());
        let text = std::fs::read_to_string(&store).unwrap();
        assert!(!text.contains("crown_empty:alpha"), "{text}");
        std::fs::remove_file(&store).ok();
    }

    #[test]
    fn inside_the_grace_it_stays_quiet_and_names_the_wait() {
        let store = temp_store("grace");
        seed(&store, "alpha", "2026-09-04T11:55:00Z");
        let p = payload(json!([manifest_crown("alpha", fold(&["x-1"]))]));
        let (findings, note) = evaluate(&p, &store, NOW).unwrap();
        assert!(findings.is_empty());
        assert!(note.contains("1500s more"), "{note}");
        std::fs::remove_file(&store).ok();
    }

    #[test]
    fn a_sweep_that_did_not_run_reads_unreadable_never_clear() {
        let store = temp_store("nosweep");
        let mut p = payload(json!([]));
        p["summary"]["sweep_ran"] = json!(false);
        let err = evaluate(&p, &store, NOW).unwrap_err();
        assert!(err.contains("sweep"), "{err}");
        std::fs::remove_file(&store).ok();
    }

    #[test]
    fn an_unreadable_registry_reads_unreadable_never_clear() {
        let store = temp_store("noreg");
        let p = json!({
            "crowns": Value::Null, "registry_readable": false,
            "graph_readable": Value::Null,
            "summary": {"reason": "registry unreadable: boom"},
        });
        let err = evaluate(&p, &store, NOW).unwrap_err();
        assert!(err.contains("registry unreadable"), "{err}");
        std::fs::remove_file(&store).ok();
    }

    #[test]
    fn an_unresolved_fold_reads_unreadable_never_clear() {
        let store = temp_store("blind");
        let bad = json!({"status": "unresolved", "reason": "the fold timed out"});
        let p = payload(json!([manifest_crown("alpha", bad)]));
        let err = evaluate(&p, &store, NOW).unwrap_err();
        assert!(
            err.contains("alpha") && err.contains("the fold timed out"),
            "{err}"
        );
        std::fs::remove_file(&store).ok();
    }

    #[test]
    fn a_failed_court_read_is_an_error_never_a_clear_board() {
        let err = court_payload_from(false, b"", b"boom").unwrap_err();
        assert!(err.contains("the court read failed"), "{err}");
        let err = court_payload_from(true, b"not json", b"").unwrap_err();
        assert!(err.contains("did not parse"), "{err}");
    }

    #[test]
    fn a_failed_court_read_names_the_last_non_config_line() {
        let err = court_payload_from(
            false,
            b"",
            b"fno config: x is not modeled\nError: court store locked",
        )
        .unwrap_err();
        assert_eq!(err, "the court read failed: Error: court store locked");
    }

    #[test]
    fn a_scope_that_leaves_a_readable_payload_drops_its_clock() {
        let store = temp_store("vanish");
        seed(&store, "alpha", "2026-09-04T11:30:00Z");
        // The sweep ran and named no crowns: alpha's clock is stale.
        let p = payload(json!([]));
        let (findings, _) = evaluate(&p, &store, NOW).unwrap();
        assert!(findings.is_empty());
        let text = std::fs::read_to_string(&store).unwrap();
        assert!(!text.contains("crown_empty:alpha"), "{text}");
        std::fs::remove_file(&store).ok();
    }

    #[test]
    fn a_blind_read_never_erases_a_clock() {
        let store = temp_store("blind-keep");
        seed(&store, "alpha", "2026-09-04T11:30:00Z");
        let p = json!({
            "crowns": Value::Null, "registry_readable": false,
            "graph_readable": Value::Null,
            "summary": {"reason": "registry unreadable: boom"},
        });
        assert!(evaluate(&p, &store, NOW).is_err());
        let text = std::fs::read_to_string(&store).unwrap();
        assert!(text.contains("crown_empty:alpha"), "{text}");
        std::fs::remove_file(&store).ok();
    }

    #[test]
    fn recovery_forgets_and_a_second_episode_pages_again() {
        let store = temp_store("recover");
        // Episode one: raises, the clock seeded past the grace.
        seed(&store, "alpha", "2026-09-04T11:30:00Z");
        let empty = payload(json!([manifest_crown("alpha", fold(&["x-1"]))]));
        let (findings, _) = evaluate(&empty, &store, NOW).unwrap();
        assert_eq!(findings.len(), 1);
        // A live holder returns: the key is dropped, nothing sends.
        let mut live = manifest_crown("alpha", fold(&[]));
        live["status"] = json!("live");
        evaluate(&payload(json!([live])), &store, NOW).unwrap();
        let text = std::fs::read_to_string(&store).unwrap();
        assert!(!text.contains("crown_empty:alpha"), "{text}");
        // Empty again: quiet through the fresh clock, then it raises. The
        // page is not deduped forever.
        let (findings, _) = evaluate(&empty, &store, NOW + 1).unwrap();
        assert!(findings.is_empty());
        std::fs::write(
            &store,
            r#"{"crown_empty:alpha": {"token": "empty", "ts": "2026-09-04T11:29:58Z"}}"#,
        )
        .unwrap();
        let (findings, _) = evaluate(&empty, &store, NOW + 3).unwrap();
        assert_eq!(findings.len(), 1);
        std::fs::remove_file(&store).ok();
    }
}
