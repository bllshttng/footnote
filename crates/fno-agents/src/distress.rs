//! The `<help>` distress side channel (x-77a0): parse the tag from the
//! stopping message, append the deduped `blocked` event row, and push it to
//! the parent spawn lineage. Deliberately separate from the stop DECISION -
//! a help tag never changes the verdict; it tells the parent the session is
//! stuck without stopping it - and named by the one question it answers, so
//! the transcript readers it needs live beside it instead of growing
//! loopcheck further (x-6aca).

use serde_json::Value;
use std::path::Path;

use crate::claims::append_event_line;
use crate::loopcheck::{bounded_read, log_bounded_read_error, now_rfc3339_utc, parse_xml_attr};

/// A `<help reason="..." evidence="...">` distress tag parsed from the
/// stopping message. Deliberately NOT an `Intent` variant: a help tag never
/// changes the stop decision; it fires a side channel that tells the parent
/// spawn lineage the session is stuck without stopping it.
#[derive(Debug, PartialEq)]
pub(crate) struct HelpDistress {
    reason: String,
    evidence: Option<String>,
}

/// First `<help ...>` opening tag whose name is exactly `help` (a raw
/// `find("<help")` would also match `<helper>`). An empty or missing reason
/// still parses: the events schema permits a reason-less blocked row, and the
/// distress itself is the signal.
pub(crate) fn extract_help_distress(text: &str) -> Option<HelpDistress> {
    let mut from = 0usize;
    while let Some(rel) = text[from..].find("<help") {
        let start = from + rel;
        let after = &text[start + "<help".len()..];
        let boundary = after
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if boundary {
            let tag_end = after.find('>')?;
            let tag = &text[start..start + "<help".len() + tag_end + 1];
            return Some(HelpDistress {
                reason: parse_xml_attr(tag, "reason").unwrap_or_default(),
                evidence: parse_xml_attr(tag, "evidence"),
            });
        }
        from = start + "<help".len();
    }
    None
}

/// The NEWEST assistant entry's text from a transcript, read through the one
/// transcript reader that speaks both harness shapes (`fno agents
/// newest-assistant-text`, peek's parsers). The distress parse's
/// transcript-only fallback (agy / opencode / codex stop hooks carry no
/// `last_assistant_message` payload). The in-process parser this replaced
/// resolved the speaker via /message/role and a top-level role alone, so a
/// codex rollout (payload.role, content[].output_text) read as
/// assistant-free on every line and no codex distress ever reached a parent
/// (x-6aca); routing through the reader deletes that second parser instead
/// of teaching it the codex shape. Newest-entry-only mirrors the intent
/// read's newest-entry rule for `watching`: an older entry's distress was
/// handled at its own stop. Fail-quiet None on a missing fno, a timeout, or
/// an empty answer - the same degrade an unreadable transcript always had.
/// `fno_bin` comes from the caller (`loopcheck_fno_bin()`) so the read is
/// hermetically testable with a stub script.
pub(crate) fn newest_assistant_text_via_reader(
    fno_bin: &str,
    transcript_path: &Path,
    cwd: &Path,
) -> Option<String> {
    let args = [
        "agents",
        "newest-assistant-text",
        "--transcript",
        transcript_path.to_str()?,
    ];
    let out = bounded_read(
        std::ffi::OsStr::new(fno_bin),
        &args,
        cwd,
        "newest_assistant_text",
        std::time::Duration::from_secs(10),
    )
    .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// True when the project log already carries a `blocked` row for this run
/// with this reason. The stop hook re-fires on every turn end, so without
/// this a session whose newest message still carries the same distress would
/// re-mail the parent on each fire.
fn blocked_distress_already_emitted(project_events: &Path, run: &str, reason: &str) -> bool {
    let Ok(file) = std::fs::File::open(project_events) else {
        return false;
    };
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if v.get("type").and_then(|t| t.as_str()) == Some("blocked")
                && v.get("run").and_then(|r| r.as_str()) == Some(run)
                && v.pointer("/data/reason").and_then(|r| r.as_str()) == Some(reason)
            {
                return true;
            }
        }
        line.clear();
    }
    false
}

/// Free-text cap shared with the emit CLI (`_PROTOCOL_DATA_STR_CAP`) and
/// finalize's run_summary cap, so a Rust-emitted reason can never outgrow the
/// rows every other producer writes.
const BLOCKED_DATA_STR_CAP: usize = 500;

/// Emit the `blocked` x-dbaf event natively (x-77a0) and push it to the
/// parent handle. The push leg and the emit-CLI auto-push shipped with zero
/// emitters (the advisory `--emit-boundary blocked` instruction demonstrably
/// never fires, so a king got swept on a timeout instead of being told). The
/// stop hook is the one surface that mechanically reads every session's
/// message, which makes it the emitter that cannot be skipped. Envelope mirrors
/// finalize's run_summary; the push shells the same Python resolver finalize
/// shells, so lineage resolution lives in one place. Best-effort throughout: a
/// write or push failure logs one stderr note and never changes the verdict.
pub(crate) fn emit_help_distress_blocked(
    project_events: &Path,
    global_events: &Path,
    cwd: &Path,
    run: &str,
    node: Option<&str>,
    distress: &HelpDistress,
) {
    if !append_blocked_event(project_events, global_events, run, node, distress) {
        return;
    }
    push_blocked_to_parent(cwd, run, node, &distress.reason);
}

/// Append the deduped `blocked` envelope to both logs. Returns whether a row
/// was written (false = duplicate, or the durable append failed and no push
/// may ride it - AC1-FR ordering, same as the emit CLI: a push with no
/// durable record is the receipt-can-lie shape).
fn append_blocked_event(
    project_events: &Path,
    global_events: &Path,
    run: &str,
    node: Option<&str>,
    distress: &HelpDistress,
) -> bool {
    let cap = |s: &str| -> String { s.chars().take(BLOCKED_DATA_STR_CAP).collect() };
    let reason = cap(&distress.reason);
    // Dedup on the CAPPED reason (codex round on PR 1282): the stored row
    // carries the capped value, so comparing the raw one missed on every
    // >500-char distress and re-emitted on each stop.
    if blocked_distress_already_emitted(project_events, run, &reason) {
        return false;
    }
    let mut data = serde_json::json!({"reason": reason});
    if let Some(ev) = distress.evidence.as_deref() {
        data["evidence"] = serde_json::json!(cap(ev));
    }
    let mut env = serde_json::json!({
        "ts": now_rfc3339_utc(),
        "v": 1,
        "type": "blocked",
        "source": "target",
        "run": run,
        "data": data,
    });
    if let Some(n) = node {
        env["node"] = serde_json::json!(n);
    }
    if let Err(error) = append_event_line(project_events, &env, std::time::Duration::from_secs(2)) {
        eprintln!(
            "loop-check: blocked write to {} failed (non-fatal): {error}",
            project_events.display()
        );
        return false;
    }
    if project_events != global_events {
        if let Err(error) =
            append_event_line(global_events, &env, std::time::Duration::from_secs(2))
        {
            eprintln!(
                "loop-check: blocked mirror to {} failed (non-fatal): {error}",
                global_events.display()
            );
        }
    }
    true
}

/// Push leg, mirroring finalize's `push_run_summary_to_parent` (a missing
/// `fno` / no spawn lineage is a silent skip; the events.jsonl row already
/// landed independently). Routed through `bounded_read` - the one bounded
/// transport every `fno` child reachable from the stop decision uses - so a
/// wedged `fno` cannot hang the fire (self-review finding: an unbounded
/// `.output()` here had exactly that shape).
fn push_blocked_to_parent(cwd: &Path, run: &str, node: Option<&str>, reason: &str) {
    let mut args: Vec<&str> = vec![
        "doctor",
        "event",
        "push-parent",
        "--type",
        "blocked",
        "--run",
        run,
        "--reason",
        reason,
    ];
    if let Some(n) = node {
        args.push("--node");
        args.push(n);
    }
    match bounded_read(
        std::ffi::OsStr::new("fno"),
        &args,
        cwd,
        "blocked_parent_push",
        std::time::Duration::from_secs(15),
    ) {
        // The push's own exit status is not load-bearing (the resolver exits
        // 0 on a no-lineage skip), and a timeout means the push MAY have
        // fired - either way the durable row already landed, which is the
        // record every reader joins on.
        Ok(_) => {}
        Err(error) => log_bounded_read_error("blocked parent push skipped (non-fatal)", &error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Mirrors the loopcheck test helper: a shell script on disk, executable.
    fn write_exec(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    #[test]
    fn extract_help_distress_attrs_and_shapes() {
        // The documented shape: both attributes.
        let d = extract_help_distress(
            r#"stuck <help reason="missing dependency" evidence="plan 4.2">detail</help>"#,
        )
        .expect("documented shape must parse");
        assert_eq!(d.reason, "missing dependency");
        assert_eq!(d.evidence.as_deref(), Some("plan 4.2"));
        // Reason-less and evidence-less tags still parse (schema permits).
        let bare = extract_help_distress("<help>").expect("bare tag parses");
        assert_eq!(bare.reason, "");
        assert_eq!(bare.evidence, None);
        let reason_only =
            extract_help_distress(r#"<help reason="x">"#).expect("reason-only parses");
        assert_eq!(reason_only.reason, "x");
        // A longer tag name sharing the prefix must NOT match.
        assert_eq!(extract_help_distress("use <helper> here"), None);
        // Absence stays absence.
        assert_eq!(extract_help_distress("no distress at all"), None);
    }

    #[test]
    fn blocked_distress_dedup_scopes_to_run_and_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let mk = |run: &str, reason: &str| {
            serde_json::to_string(&serde_json::json!({
                "type": "blocked", "run": run,
                "data": {"reason": reason}
            }))
            .unwrap()
                + "\n"
        };
        std::fs::write(&path, mk("run-a", "missing dependency")).unwrap();
        // Same run + reason -> already emitted.
        assert!(blocked_distress_already_emitted(
            &path,
            "run-a",
            "missing dependency"
        ));
        // Different reason on the same run -> not a duplicate.
        assert!(!blocked_distress_already_emitted(
            &path,
            "run-a",
            "other wall"
        ));
        // Same reason on a different run -> not a duplicate.
        assert!(!blocked_distress_already_emitted(
            &path,
            "run-b",
            "missing dependency"
        ));
        // A missing log is an honest no, not a corrupted yes.
        let absent = tmp.path().join("absent.jsonl");
        assert!(!blocked_distress_already_emitted(
            &absent,
            "run-a",
            "missing dependency"
        ));
    }

    #[test]
    fn emit_help_distress_blocked_appends_envelope_and_dedups() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("events.jsonl");
        let global = tmp.path().join("global.jsonl");
        let d = HelpDistress {
            reason: "missing dependency".to_string(),
            evidence: Some("plan 4.2".to_string()),
        };
        append_blocked_event(&project, &global, "run-a", Some("x-77a0"), &d);
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(&project)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 1, "exactly one row on first distress");
        let row = &rows[0];
        // x-dbaf extended envelope, same family finalize's run_summary writes.
        assert_eq!(row["type"], "blocked");
        assert_eq!(row["source"], "target");
        assert_eq!(row["run"], "run-a");
        assert_eq!(row["node"], "x-77a0");
        assert_eq!(row["data"]["reason"], "missing dependency");
        assert_eq!(row["data"]["evidence"], "plan 4.2");
        assert_eq!(
            std::fs::read_to_string(&global).unwrap().trim(),
            serde_json::to_string(&row).unwrap(),
            "the global mirror carries the identical row"
        );
        // A second fire with the same (run, reason) appends nothing anywhere.
        assert!(!append_blocked_event(
            &project,
            &global,
            "run-a",
            Some("x-77a0"),
            &d
        ));
        assert_eq!(
            std::fs::read_to_string(&project).unwrap().lines().count(),
            1,
            "identical distress must not append a second row"
        );
        // A different reason is a new distress: it appends.
        let d2 = HelpDistress {
            reason: "second wall".to_string(),
            evidence: None,
        };
        assert!(append_blocked_event(&project, &global, "run-a", None, &d2));
        assert_eq!(
            std::fs::read_to_string(&project).unwrap().lines().count(),
            2
        );
    }

    #[test]
    fn emit_help_distress_blocked_caps_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("events.jsonl");
        let global = tmp.path().join("global2.jsonl");
        let long: String = "x".repeat(BLOCKED_DATA_STR_CAP + 50);
        let d = HelpDistress {
            reason: long.clone(),
            evidence: None,
        };
        assert!(append_blocked_event(&project, &global, "run-a", None, &d));
        let row: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(&project)
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        let capped: String = "x".repeat(BLOCKED_DATA_STR_CAP);
        assert_eq!(row["data"]["reason"], serde_json::json!(capped));
        // Dedup joins on the CAPPED value (codex round on PR 1282): the
        // stored row carries the capped reason, so comparing the raw one
        // missed on every re-fire of the same oversized distress.
        assert!(!append_blocked_event(&project, &global, "run-a", None, &d));
        assert_eq!(
            std::fs::read_to_string(&project).unwrap().lines().count(),
            1
        );
    }

    #[test]
    fn distress_flows_from_reader_output_to_a_blocked_row() {
        // x-6aca regression, Rust half: the transcript fallback must feed
        // extract_help_distress and land a blocked row. The stub pins the
        // seam contract: `agents newest-assistant-text --transcript <path>`
        // with the newest assistant text on stdout. The codex record SHAPE
        // itself is pinned on the Python side, in the reader that now owns
        // this parse (cli/tests/agents/test_peek.py); a rollout whose
        // payload.role is assistant and whose last message carries the tag
        // produces exactly this stdout.
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("rollout-2026-09-06T00-00-00-cx-1.jsonl");
        std::fs::write(&transcript, "rollout bytes the stub vouches for\n").unwrap();
        let stub = write_exec(
            tmp.path(),
            "fno",
            "#!/bin/sh\n[ \"$1\" = agents ] && [ \"$2\" = newest-assistant-text ] && [ \"$3\" = --transcript ] && [ -f \"$4\" ] || exit 42\nprintf '%s' '<help reason=\"worktree-init-blocked\" evidence=\"Unable to create .git/refs/heads lock: Operation not permitted\">'\n",
        );
        let text =
            newest_assistant_text_via_reader(stub.to_str().unwrap(), &transcript, tmp.path());
        let distress = text.as_deref().and_then(extract_help_distress);
        assert_eq!(
            distress.as_ref().map(|d| d.reason.as_str()),
            Some("worktree-init-blocked"),
            "the reader's answer must reach the distress parse"
        );
        // The row the parent actually reads (the same emit the stop fire makes).
        let project = tmp.path().join("events.jsonl");
        let global = tmp.path().join("global.jsonl");
        assert!(append_blocked_event(
            &project,
            &global,
            "cx-run",
            Some("x-6aca"),
            &distress.unwrap()
        ));
        let row: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&project).unwrap()).unwrap();
        assert_eq!(row["type"], serde_json::json!("blocked"));
        assert_eq!(
            row["data"]["reason"],
            serde_json::json!("worktree-init-blocked")
        );
        assert_eq!(row["node"], serde_json::json!("x-6aca"));
        // No reader answer (missing binary, empty stdout): fail-quiet None,
        // the same degrade an unreadable transcript always had.
        let text = newest_assistant_text_via_reader(
            tmp.path().join("no-such-fno").to_str().unwrap(),
            &transcript,
            tmp.path(),
        );
        assert_eq!(text, None);
    }
}
