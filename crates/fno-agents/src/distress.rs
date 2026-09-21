//! The `<help>` distress side channel : parse the tag from the
//! stopping message, append the deduped `blocked` event row, and push it to
//! the parent spawn lineage. Deliberately separate from the stop DECISION -
//! a help tag never changes the verdict; it tells the parent the session is
//! stuck without stopping it - and named by the one question it answers, so
//! the transcript readers it needs live beside it instead of growing
//! loopcheck further.

use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::claims::append_event_line;
use crate::loopcheck::{
    bounded_read, log_bounded_read_error, loopcheck_fno_bin, now_rfc3339_utc, parse_xml_attr,
    try_flag_value,
};

/// Which distress vocabulary produced the row. `help` for the in-session
/// `<help>` tag, `result_blocked` for a `RESULT: BLOCKED` return-contract
/// line. Lands on the envelope as `data.kind`; the events `type` stays
/// `blocked` for both (the v1 protocol enum is additive-only).
#[derive(Debug, PartialEq)]
pub(crate) enum DistressKind {
    Help,
    ResultBlocked,
}

impl DistressKind {
    fn as_str(&self) -> &'static str {
        match self {
            DistressKind::Help => "help",
            DistressKind::ResultBlocked => "result_blocked",
        }
    }
}

/// A distress parsed from the stopping message: either a
/// `<help reason="..." evidence="...">` tag or a `RESULT: BLOCKED` return.
/// Deliberately NOT an `Intent` variant: a distress never changes the stop
/// decision; it fires a side channel that tells the parent spawn lineage the
/// session is stuck without stopping it.
#[derive(Debug, PartialEq)]
pub(crate) struct HelpDistress {
    reason: String,
    evidence: Option<String>,
    kind: DistressKind,
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
                kind: DistressKind::Help,
            });
        }
        from = start + "<help".len();
    }
    None
}

/// The other half of the same distress vocabulary: a
/// `RESULT: BLOCKED` return, in either return-contract spelling - the plain
/// line grammar (`RESULT: BLOCKED`, reason on a following `REASON:` line)
/// and the preferred JSON object `{"result": "BLOCKED", ...}` in a fenced
/// block or a `<result>` wrapper, reason from its `summary`. A reason-less
/// return still parses; the distress itself is the signal.
pub(crate) fn extract_result_blocked(text: &str) -> Option<HelpDistress> {
    // The token must end the line or be followed by whitespace, so
    // RESULT: BLOCKEDX is prose, not a return.
    let is_blocked_line = |l: &str| -> bool {
        match l.trim_start().strip_prefix("RESULT: BLOCKED") {
            Some(rest) => rest.is_empty() || rest.starts_with(char::is_whitespace),
            None => false,
        }
    };
    for line in text.lines() {
        if is_blocked_line(line) {
            let reason = text
                .lines()
                .skip_while(|l| !is_blocked_line(l))
                .skip(1)
                .find_map(|l| {
                    l.trim_start()
                        .strip_prefix("REASON:")
                        .map(|r| r.trim().to_string())
                })
                .unwrap_or_default();
            return Some(HelpDistress {
                reason,
                evidence: None,
                kind: DistressKind::ResultBlocked,
            });
        }
    }
    extract_result_blocked_json(text)
}

/// The JSON twin: scan fenced ```json blocks and `<result>` wrappers for a
/// `{"result": "BLOCKED", ...}` object. Plain un-fenced `{...}` in prose is
/// deliberately not scanned: it is not a return-contract shape.
fn extract_result_blocked_json(text: &str) -> Option<HelpDistress> {
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        let (opener, closer) = if trimmed.starts_with("```") {
            ("```", "```")
        } else if trimmed == "<result>" {
            ("<result>", "</result>")
        } else {
            i += 1;
            continue;
        };
        let mut body: Vec<&str> = Vec::new();
        let mut j = i + 1;
        while j < lines.len() && lines[j].trim() != closer {
            body.push(lines[j]);
            j += 1;
        }
        let joined = body.join("\n");
        if let Ok(v) = serde_json::from_str::<Value>(joined.trim()) {
            if v.get("result").and_then(Value::as_str) == Some("BLOCKED") {
                return Some(HelpDistress {
                    reason: v
                        .get("summary")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    evidence: None,
                    kind: DistressKind::ResultBlocked,
                });
            }
        }
        i = j + 1;
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
///; routing through the reader deletes that second parser instead
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

/// Read the stopping message for a `<help>` tag and, on a hit, append the
/// blocked row - the read+emit chain shared by loop_check's manifest-bearing
/// stop and a pre-manifest visitor-allowed exit, so there is one
/// implementation and no second copy to drift. `last_assistant_message` wins
/// when present; otherwise the transcript reader supplies the NEWEST entry
/// (mirroring the intent read's newest-entry rule for `watching`, since an
/// older entry's distress was already handled at its own stop) - the
/// fallback the agy, opencode, and codex stop hooks need, as those
/// invocations carry no `last_assistant_message` payload. Returns whether a
/// row was written.
pub(crate) fn scan_and_emit(
    project_events: &Path,
    global_events: &Path,
    cwd: &Path,
    run: &str,
    node: Option<&str>,
    harness: Option<&str>,
    transcript_path: &Path,
    last_assistant_message: Option<&str>,
) -> bool {
    let distress_text: Option<String> = last_assistant_message
        .map(str::to_string)
        .or_else(|| newest_assistant_text_via_reader(&loopcheck_fno_bin(), transcript_path, cwd));
    // A help tag wins when a message somehow carries both: it is the more
    // specific signal and it already has a reason attribute.
    let Some(distress) = distress_text
        .as_deref()
        .and_then(|t| extract_help_distress(t).or_else(|| extract_result_blocked(t)))
    else {
        return false;
    };
    emit_help_distress_blocked(
        project_events,
        global_events,
        cwd,
        run,
        node,
        harness,
        &distress,
    )
}

/// Emit the `blocked` event natively and push it to the
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
    harness: Option<&str>,
    distress: &HelpDistress,
) -> bool {
    if !append_blocked_event(project_events, global_events, run, node, harness, distress) {
        return false;
    }
    push_blocked_to_parent(cwd, run, node, &distress.reason);
    true
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
    harness: Option<&str>,
    distress: &HelpDistress,
) -> bool {
    let cap = |s: &str| -> String { s.chars().take(BLOCKED_DATA_STR_CAP).collect() };
    let reason = cap(&distress.reason);
    // Dedup on the CAPPED reason (codex round on PR 1282): the stored row
    // carries the capped value, so comparing the raw one missed on every
    // >500-char distress and re-emitted on each stop. Keyed on run + reason
    // only: a retry that resolves its harness differently is still the
    // same distress.
    if blocked_distress_already_emitted(project_events, run, &reason) {
        return false;
    }
    let mut data = serde_json::json!({"reason": reason, "kind": distress.kind.as_str()});
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
    if let Some(h) = harness {
        env["harness"] = serde_json::json!(h);
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
        std::ffi::OsStr::new(&loopcheck_fno_bin()),
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

const DISTRESS_SCAN_USAGE: &str = "\
usage: fno-agents distress-scan --transcript <path> --run <id> [--node <id>]
       [--harness <name>] [--cwd <dir>] [--events <p>] [--global-events <p>]

Reads a transcript for a <help> tag or a RESULT: BLOCKED return and, on a
hit, appends a blocked row -
the pre-manifest counterpart of the read loop_check runs inline. Best-effort
throughout: always exits 0. Prints 'distress: emitted <reason>' on a write,
'distress: none' otherwise (no tag, no --run, or a transcript that does not
exist).";

/// `fno-agents distress-scan --transcript <path> --run <id> [--node <id>]
/// [--harness <name>] [--cwd <dir>] [--events <p>] [--global-events <p>].
/// The pre-manifest counterpart of the inline read `loop_check`
/// runs: a stop hook that finds no manifest for the session has no
/// `last_assistant_message` binding to reuse, so it calls this instead of
/// inlining a second copy of the read. Event paths resolve the same way
/// `loop_check` resolves them (project path from cwd, global path under
/// `$HOME/.fno`), overridable for tests. Always exits 0: a failure here must
/// never turn into a stop-hook failure.
pub fn run_distress_scan(args: &[String]) -> i32 {
    let args = if args.first().map(String::as_str) == Some("distress-scan") {
        &args[1..]
    } else {
        args
    };
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{DISTRESS_SCAN_USAGE}");
        return 0;
    }
    let mut transcript: Option<PathBuf> = None;
    let mut run: Option<String> = None;
    let mut node: Option<String> = None;
    let mut harness: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut events_path: Option<PathBuf> = None;
    let mut global_events_path: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        if let Some(val) = try_flag_value(&args[i], "--transcript", args, &mut i) {
            transcript = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(&args[i], "--run", args, &mut i) {
            run = Some(val);
        } else if let Some(val) = try_flag_value(&args[i], "--node", args, &mut i) {
            node = Some(val);
        } else if let Some(val) = try_flag_value(&args[i], "--harness", args, &mut i) {
            harness = Some(val);
        } else if let Some(val) = try_flag_value(&args[i], "--cwd", args, &mut i) {
            cwd = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(&args[i], "--events", args, &mut i) {
            events_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(&args[i], "--global-events", args, &mut i) {
            global_events_path = Some(PathBuf::from(val));
        }
        i += 1;
    }
    let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let (Some(run), Some(transcript)) = (run, transcript) else {
        println!("distress: none");
        return 0;
    };
    if !transcript.exists() {
        println!("distress: none");
        return 0;
    }
    let text = newest_assistant_text_via_reader(&loopcheck_fno_bin(), &transcript, &cwd);
    let Some(distress) = text
        .as_deref()
        .and_then(|t| extract_help_distress(t).or_else(|| extract_result_blocked(t)))
    else {
        println!("distress: none");
        return 0;
    };
    let project_events = events_path.unwrap_or_else(|| crate::paths::events_path(&cwd));
    let global_events =
        global_events_path.unwrap_or_else(crate::loopcheck::default_global_events_path);
    // Already parsed above (line ~351): call the emitter directly instead of
    // scan_and_emit, which would parse the same text for a <help> tag again.
    let reason = distress.reason.clone();
    let wrote = emit_help_distress_blocked(
        &project_events,
        &global_events,
        &cwd,
        &run,
        node.as_deref(),
        harness.as_deref(),
        &distress,
    );
    if wrote {
        println!("distress: emitted {reason}");
    } else {
        println!("distress: none");
    }
    0
}

/// `FNO_LOOPCHECK_FNO_BIN` is process-global; `cargo test` runs unit tests
/// on multiple threads by default, so two tests mutating it concurrently
/// (here and in loopcheck.rs) can hand each other's stub answer to the
/// wrong call. Every test that sets this var holds this lock across the
/// set/run/restore section.
#[cfg(test)]
pub(crate) fn fno_bin_env_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            kind: DistressKind::Help,
        };
        append_blocked_event(
            &project,
            &global,
            "run-a",
            Some("x-aaaa"),
            Some("codex"),
            &d,
        );
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(&project)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 1, "exactly one row on first distress");
        let row = &rows[0];
        // extended envelope, same family finalize's run_summary writes.
        assert_eq!(row["type"], "blocked");
        assert_eq!(row["source"], "target");
        assert_eq!(row["run"], "run-a");
        assert_eq!(row["node"], "x-aaaa");
        assert_eq!(row["harness"], "codex");
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
            Some("x-aaaa"),
            Some("codex"),
            &d
        ));
        assert_eq!(
            std::fs::read_to_string(&project).unwrap().lines().count(),
            1,
            "identical distress must not append a second row"
        );
        // A different reason with no known harness is a new distress: it
        // appends, and carries no harness key at all (not a null one).
        let d2 = HelpDistress {
            reason: "second wall".to_string(),
            evidence: None,
            kind: DistressKind::Help,
        };
        assert!(append_blocked_event(
            &project, &global, "run-a", None, None, &d2
        ));
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(&project)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert!(
            rows[1].get("harness").is_none(),
            "no harness known must omit the key, not write null"
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
            kind: DistressKind::Help,
        };
        assert!(append_blocked_event(
            &project, &global, "run-a", None, None, &d
        ));
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
        assert!(!append_blocked_event(
            &project, &global, "run-a", None, None, &d
        ));
        assert_eq!(
            std::fs::read_to_string(&project).unwrap().lines().count(),
            1
        );
    }

    #[test]
    fn distress_flows_from_reader_output_to_a_blocked_row() {
        // x-bbbb regression, Rust half: the transcript fallback must feed
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
            Some("x-bbbb"),
            None,
            &distress.unwrap()
        ));
        let row: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&project).unwrap()).unwrap();
        assert_eq!(row["type"], serde_json::json!("blocked"));
        assert_eq!(
            row["data"]["reason"],
            serde_json::json!("worktree-init-blocked")
        );
        assert_eq!(row["node"], serde_json::json!("x-bbbb"));
        // No reader answer (missing binary, empty stdout): fail-quiet None,
        // the same degrade an unreadable transcript always had.
        let text = newest_assistant_text_via_reader(
            tmp.path().join("no-such-fno").to_str().unwrap(),
            &transcript,
            tmp.path(),
        );
        assert_eq!(text, None);
    }

    #[test]
    fn scan_and_emit_writes_nothing_without_a_help_tag() {
        // AC2-EDGE: a message with no help tag returns false and appends
        // nothing to either log.
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("events.jsonl");
        let global = tmp.path().join("global.jsonl");
        let transcript = tmp.path().join("t.jsonl");
        let wrote = scan_and_emit(
            &project,
            &global,
            tmp.path(),
            "run-a",
            None,
            None,
            &transcript,
            Some("all clear, nothing stuck here"),
        );
        assert!(!wrote);
        assert!(!project.exists());
        assert!(!global.exists());
    }

    #[test]
    fn scan_and_emit_takes_the_transcript_fallback_and_stamps_harness() {
        // AC1-HP / AC3-HP, exercised through the same entry point both stop
        // paths call: no `last_assistant_message` (the agy/opencode/codex
        // shape), so the transcript reader supplies the tag, and the caller's
        // harness lands on the envelope.
        let _env_guard = fno_bin_env_test_lock().lock().unwrap();
        let var = "FNO_LOOPCHECK_FNO_BIN";
        let prior = std::env::var(var).ok();
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("rollout-2026-09-06T00-00-00-cx-1.jsonl");
        std::fs::write(&transcript, "rollout bytes the stub vouches for\n").unwrap();
        let stub = write_exec(
            tmp.path(),
            "fno",
            "#!/bin/sh\n[ \"$1\" = agents ] && [ \"$2\" = newest-assistant-text ] && [ \"$3\" = --transcript ] && [ -f \"$4\" ] || exit 42\nprintf '%s' '<help reason=\"worktree-init-blocked\" evidence=\"Operation not permitted\">'\n",
        );
        std::env::set_var(var, stub.to_str().unwrap());

        let project = tmp.path().join("events.jsonl");
        let global = tmp.path().join("global.jsonl");
        let wrote = scan_and_emit(
            &project,
            &global,
            tmp.path(),
            "cx-run",
            Some("x-bbbb"),
            Some("codex"),
            &transcript,
            None,
        );

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert!(wrote);
        let row: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&project).unwrap()).unwrap();
        assert_eq!(row["data"]["reason"], "worktree-init-blocked");
        assert_eq!(row["data"]["evidence"], "Operation not permitted");
        assert_eq!(row["node"], "x-bbbb");
        assert_eq!(row["harness"], "codex");
    }

    /// The `agents newest-assistant-text --transcript <path>` contract this
    /// crate cannot itself parse (that reader lives in Python, `peek.py`):
    /// read the record's `payload.content[0].text`, the same field the real
    /// reader returns for a codex rollout line.
    fn write_transcript_reader_stub(dir: &Path) -> PathBuf {
        write_exec(
            dir,
            "fno",
            r#"#!/bin/sh
[ "$1" = agents ] && [ "$2" = newest-assistant-text ] && [ "$3" = --transcript ] || exit 42
python3 -c '
import json, sys
with open(sys.argv[1]) as fh:
    rec = json.loads(fh.readline())
print(rec["payload"]["content"][0]["text"], end="")
' "$4"
"#,
        )
    }

    #[test]
    fn run_distress_scan_reads_the_checked_in_codex_fixture() {
        // AC10-HP / AC11-EDGE, run through the actual verb entry point
        // (run_distress_scan), against the real rollout line checked in at
        // tests/fixtures/rollout-codex-help.jsonl. AC11-EDGE's positive
        // control lives in THIS run: the tag-free variant is asserted
        // against the row the tagged fixture already proved it can write,
        // never as an absence on its own.
        let _env_guard = fno_bin_env_test_lock().lock().unwrap();
        let var = "FNO_LOOPCHECK_FNO_BIN";
        let prior = std::env::var(var).ok();
        let tmp = tempfile::tempdir().unwrap();
        let stub = write_transcript_reader_stub(tmp.path());
        std::env::set_var(var, stub.to_str().unwrap());

        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/rollout-codex-help.jsonl");
        let project = tmp.path().join("events.jsonl");
        let global = tmp.path().join("global.jsonl");
        let args: Vec<String> = [
            "distress-scan",
            "--transcript",
            fixture.to_str().unwrap(),
            "--run",
            "fixture-run",
            "--harness",
            "codex",
            "--cwd",
            tmp.path().to_str().unwrap(),
            "--events",
            project.to_str().unwrap(),
            "--global-events",
            global.to_str().unwrap(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let code = run_distress_scan(&args);

        // AC11-EDGE control: the same wiring against a tag-free copy of the
        // same line must add no second row.
        let text = std::fs::read_to_string(&fixture).unwrap();
        let mut rec: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        rec["payload"]["content"][0]["text"] = serde_json::json!("all clear, nothing stuck here");
        let no_help_fixture = tmp.path().join("no-help.jsonl");
        std::fs::write(&no_help_fixture, serde_json::to_string(&rec).unwrap()).unwrap();
        let args2: Vec<String> = [
            "distress-scan",
            "--transcript",
            no_help_fixture.to_str().unwrap(),
            "--run",
            "fixture-run-2",
            "--harness",
            "codex",
            "--cwd",
            tmp.path().to_str().unwrap(),
            "--events",
            project.to_str().unwrap(),
            "--global-events",
            global.to_str().unwrap(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let code2 = run_distress_scan(&args2);

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert_eq!(code, 0);
        assert_eq!(code2, 0);
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(&project)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "the tag-free rerun must add no second row: {rows:?}"
        );
        assert_eq!(rows[0]["harness"], "codex");
        assert!(
            rows[0]["data"]["evidence"]
                .as_str()
                .unwrap()
                .contains("Operation not permitted"),
            "got: {:?}",
            rows[0]["data"]["evidence"]
        );
    }

    #[test]
    fn extract_result_blocked_line_grammar_and_json_forms() {
        // AC3-HP shape: the plain line grammar with a REASON: line.
        let d = extract_result_blocked("RESULT: BLOCKED\nREASON: missing dependency")
            .expect("line grammar parses");
        assert_eq!(d.kind, DistressKind::ResultBlocked);
        assert_eq!(d.reason, "missing dependency");
        assert_eq!(d.evidence, None);
        // A longer token sharing the prefix is prose, not a return.
        assert_eq!(extract_result_blocked("RESULT: BLOCKEDX"), None);
        // An indented REASON line still supplies the reason.
        let di = extract_result_blocked("RESULT: BLOCKED\n  REASON: dep missing")
            .expect("indented REASON parses");
        assert_eq!(di.reason, "dep missing");
        // The preferred JSON object, fenced.
        let fenced = concat!(
            "work so far committed. ",
            "```json\n",
            r#"{"result": "BLOCKED", "task": "2.1", "summary": "dep missing"}"#,
            "\n```\n"
        );
        let dj = extract_result_blocked(fenced).expect("fenced JSON parses");
        assert_eq!(dj.kind, DistressKind::ResultBlocked);
        assert_eq!(dj.reason, "dep missing");
        // ...and in a <result> wrapper.
        let wrapped = concat!(
            "<result>\n",
            r#"{"result": "BLOCKED", "task": "2.1", "summary": "no claim held"}"#,
            "\n</result>\n"
        );
        let dw = extract_result_blocked(wrapped).expect("wrapped JSON parses");
        assert_eq!(dw.kind, DistressKind::ResultBlocked);
        assert_eq!(dw.reason, "no claim held");
        // A JSON object that is NOT a BLOCKED return is not a distress.
        let success = concat!(
            "```json\n",
            r#"{"result": "SUCCESS", "task": "2.1", "summary": "fine"}"#,
            "\n```\n"
        );
        assert_eq!(extract_result_blocked(success), None);
        // Absence stays absence.
        assert_eq!(
            extract_result_blocked("all clear, nothing stuck here"),
            None
        );
    }

    #[test]
    fn scan_and_emit_covers_the_result_blocked_path() {
        // AC3-HP: a stopping message carrying RESULT: BLOCKED with a REASON
        // appends one blocked row with data.kind result_blocked.
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("events.jsonl");
        let global = tmp.path().join("global.jsonl");
        let transcript = tmp.path().join("t.jsonl");
        let wrote = scan_and_emit(
            &project,
            &global,
            tmp.path(),
            "run-a",
            None,
            None,
            &transcript,
            Some("RESULT: BLOCKED\nREASON: probe reason"),
        );
        assert!(wrote);
        let row: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&project).unwrap()).unwrap();
        assert_eq!(row["type"], "blocked");
        assert_eq!(row["data"]["kind"], "result_blocked");
        assert_eq!(row["data"]["reason"], "probe reason");
        // AC3-EDGE: the JSON return form appends the same row shape.
        let tmp2 = tempfile::tempdir().unwrap();
        let project2 = tmp2.path().join("events.jsonl");
        let global2 = tmp2.path().join("global.jsonl");
        let transcript2 = tmp2.path().join("t2.jsonl");
        let json_msg = concat!(
            "```json\n",
            r#"{"result": "BLOCKED", "task": "2.1", "summary": "gate refused"}"#,
            "\n```"
        );
        let wrote2 = scan_and_emit(
            &project2,
            &global2,
            tmp2.path(),
            "run-b",
            None,
            None,
            &transcript2,
            Some(json_msg),
        );
        assert!(wrote2);
        let row2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&project2).unwrap()).unwrap();
        assert_eq!(row2["data"]["kind"], "result_blocked");
        assert_eq!(row2["data"]["reason"], "gate refused");
        // AC3-EDGE: a second stop on the same message appends nothing - the
        // existing (run, capped reason) dedup covers the new path.
        assert!(!scan_and_emit(
            &project2,
            &global2,
            tmp2.path(),
            "run-b",
            None,
            None,
            &transcript2,
            Some(json_msg)
        ));
        assert_eq!(
            std::fs::read_to_string(&project2).unwrap().lines().count(),
            1
        );
    }

    #[test]
    fn scan_and_emit_help_tag_wins_over_result_blocked() {
        // AC3-ERR: a message carrying BOTH appends exactly one row, kind help.
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("events.jsonl");
        let global = tmp.path().join("global.jsonl");
        let transcript = tmp.path().join("t.jsonl");
        let both = concat!(
            r#"<help reason="missing dependency">plan 4.2</help>"#,
            "\nRESULT: BLOCKED\nREASON: also blocked"
        );
        let wrote = scan_and_emit(
            &project,
            &global,
            tmp.path(),
            "run-a",
            None,
            None,
            &transcript,
            Some(both),
        );
        assert!(wrote);
        let row: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&project).unwrap()).unwrap();
        assert_eq!(row["data"]["kind"], "help");
        assert_eq!(row["data"]["reason"], "missing dependency");
        assert_eq!(
            std::fs::read_to_string(&project).unwrap().lines().count(),
            1,
            "exactly one row when both vocabularies appear"
        );
    }

    #[test]
    fn run_distress_scan_covers_the_result_blocked_path() {
        // The CLI verb's own parse (the shape the pre-deploy done_probe
        // exercises): the transcript reader supplies a RESULT: BLOCKED
        // return and the scan emits one result_blocked row.
        let _env_guard = fno_bin_env_test_lock().lock().unwrap();
        let var = "FNO_LOOPCHECK_FNO_BIN";
        let prior = std::env::var(var).ok();
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("rollout-result-blocked.jsonl");
        std::fs::write(&transcript, "rollout bytes the stub vouches for\n").unwrap();
        let stub = write_exec(
            tmp.path(),
            "fno",
            "#!/bin/sh\n[ \"$1\" = agents ] && [ \"$2\" = newest-assistant-text ] && [ \"$3\" = --transcript ] && [ -f \"$4\" ] || exit 42\nprintf 'RESULT: BLOCKED\\nREASON: probe reason'\n",
        );
        std::env::set_var(var, stub.to_str().unwrap());
        let project = tmp.path().join("events.jsonl");
        let global = tmp.path().join("global.jsonl");
        let args: Vec<String> = [
            "distress-scan",
            "--transcript",
            transcript.to_str().unwrap(),
            "--run",
            "rb-run",
            "--cwd",
            tmp.path().to_str().unwrap(),
            "--events",
            project.to_str().unwrap(),
            "--global-events",
            global.to_str().unwrap(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let code = run_distress_scan(&args);
        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
        assert_eq!(code, 0);
        let row: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&project).unwrap()).unwrap();
        assert_eq!(row["type"], "blocked");
        assert_eq!(row["data"]["kind"], "result_blocked");
        assert_eq!(row["data"]["reason"], "probe reason");
    }

    #[test]
    fn help_rows_also_carry_data_kind() {
        // data.kind is additive on the EXISTING help path too: every blocked
        // row names which vocabulary produced it.
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("events.jsonl");
        let global = tmp.path().join("global.jsonl");
        let transcript = tmp.path().join("t.jsonl");
        assert!(scan_and_emit(
            &project,
            &global,
            tmp.path(),
            "run-a",
            None,
            None,
            &transcript,
            Some(r#"<help reason="stuck">ev</help>"#)
        ));
        let row: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&project).unwrap()).unwrap();
        assert_eq!(row["data"]["kind"], "help");
    }
}
