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

/// Why the session is stuck, from the `<help class="...">` attribute. The
/// router (help_router.rs) routes on this class, so the parse is total:
/// a missing or unknown class reads Unclassified and still gets a route
/// (question at the next rung), never a silent skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HelpClass {
    StalePlan,
    MissingPrereq,
    CiRed,
    Stuck,
    Held,
    Wait,
    EnvDenied,
    GateDeadlock,
    GateUnsatisfiable,
    Question,
    Budget,
    Unclassified,
}

impl HelpClass {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            HelpClass::StalePlan => "stale-plan",
            HelpClass::MissingPrereq => "missing-prereq",
            HelpClass::CiRed => "ci-red",
            HelpClass::Stuck => "stuck",
            HelpClass::Held => "held",
            HelpClass::Wait => "wait",
            HelpClass::EnvDenied => "env-denied",
            HelpClass::GateDeadlock => "gate-deadlock",
            HelpClass::GateUnsatisfiable => "gate-unsatisfiable",
            HelpClass::Question => "question",
            HelpClass::Budget => "budget",
            HelpClass::Unclassified => "unclassified",
        }
    }

    /// The inverse of `as_str`; anything unknown reads Unclassified so the
    /// router's exhaustive match stays total over a growing vocabulary.
    pub(crate) fn parse(s: &str) -> HelpClass {
        match s {
            "stale-plan" => HelpClass::StalePlan,
            "missing-prereq" => HelpClass::MissingPrereq,
            "ci-red" => HelpClass::CiRed,
            "stuck" => HelpClass::Stuck,
            "held" => HelpClass::Held,
            "wait" => HelpClass::Wait,
            "env-denied" => HelpClass::EnvDenied,
            "gate-deadlock" => HelpClass::GateDeadlock,
            "gate-unsatisfiable" => HelpClass::GateUnsatisfiable,
            "question" => HelpClass::Question,
            "budget" => HelpClass::Budget,
            _ => HelpClass::Unclassified,
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
    class: HelpClass,
}

/// The one constructor for callers outside this module: the stuck row the
/// self-cancel path writes. The fields stay private to the emitter, so a
/// distress keeps its one shape at the door it is parsed at.
pub(crate) fn help_distress_stuck(reason: String, evidence: Option<String>) -> HelpDistress {
    HelpDistress {
        reason,
        evidence,
        kind: DistressKind::Help,
        class: HelpClass::Stuck,
    }
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
                class: parse_xml_attr(tag, "class")
                    .map(|c| HelpClass::parse(&c))
                    .unwrap_or(HelpClass::Unclassified),
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
                class: HelpClass::Unclassified,
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
        let closer = if trimmed.starts_with("```") {
            "```"
        } else if trimmed == "<result>" {
            "</result>"
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
                    class: HelpClass::Unclassified,
                });
            }
        }
        i = j + 1;
    }
    None
}

/// The stopping turn, read natively from the transcript tail: the assistant
/// entries since the newest user entry, their texts, and a turn key for the
/// dedup. This replaces the `newest-assistant-text` shell-out on the
/// distress path, so a help in an EARLIER assistant entry of the same turn
/// is found (the newest-entry-only read lost it), and no transcript reader
/// subprocess rides a stop fire. The claude shape (/message/role, content
/// blocks, top-level uuid) and the codex rollout shape (payload.role,
/// payload.content[].text, turn_id in the record metadata) both parse.
pub(crate) struct TurnRead {
    /// codex turn_id from the payload record, else the uuid of the newest
    /// assistant entry, else a hash of the turn text: the dedup key that
    /// keeps a re-fire on the SAME turn from writing a second row while a
    /// NEW turn with the same class climbs.
    pub turn_key: String,
    /// The newest assistant text in the turn that carries a distress, if
    /// any. Newest-first: the newest tag wins, mirroring the intent read.
    pub distress_text: Option<String>,
}

/// One transcript record's speaker and text, both harness shapes.
fn turn_role_and_text(val: &Value) -> (&str, String) {
    if let Some(role) = val.pointer("/payload/role").and_then(Value::as_str) {
        let text = val
            .pointer("/payload/content")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        return (role, text);
    }
    let role = val
        .pointer("/message/role")
        .or_else(|| val.get("role"))
        .and_then(Value::as_str)
        .unwrap_or("");
    (role, crate::loopcheck::extract_assistant_text(val))
}

/// True when the window already holds a user entry, so growing it further
/// cannot extend the turn.
fn turn_boundary_in_window(lines: &[String]) -> bool {
    lines.iter().any(|line| {
        serde_json::from_str::<Value>(line.trim())
            .map(|val| turn_role_and_text(&val).0 == "user")
            .unwrap_or(false)
    })
}

/// The turn reader's ceiling: a real stopping turn fits far under this, and
/// a transcript with no user entry inside 8MB has no turn boundary worth
/// reading for distress - the stop hook p90 the router exists on top of
/// must not pay a 64MB tail for it.
const TURN_READ_MAX_WINDOW: u64 = 8 << 20;

/// Read the stopping turn. None when the transcript holds no parseable
/// lines (absent, empty, or unreadable): the caller keys its dedup on the
/// payload text alone, the best key a fire without a transcript has.
pub(crate) fn read_stopping_turn(transcript_path: &Path) -> Option<TurnRead> {
    // Growth loop: start at 1MB and grow only while the window has not yet
    // reached the user boundary, because a session transcript reaches
    // 135MB and the whole file is churn a stop fire cannot afford.
    let mut window_bytes: u64 = 1 << 20;
    let window = loop {
        let candidate = crate::loopcheck::read_tail_lines(transcript_path, window_bytes);
        if (candidate.len() >= 2 && turn_boundary_in_window(&candidate))
            || window_bytes >= TURN_READ_MAX_WINDOW
        {
            break candidate;
        }
        window_bytes *= 4;
    };
    if window.is_empty() {
        return None;
    }
    let mut texts_rev: Vec<String> = Vec::new();
    let mut turn_id: Option<String> = None;
    let mut entry_key: Option<String> = None;
    for line in window.iter().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if turn_id.is_none() {
            let tid = val
                .pointer("/payload/internal_chat_message_metadata_passthrough/turn_id")
                .or_else(|| val.pointer("/payload/turn_id"))
                .or_else(|| val.get("turn_id"))
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty());
            if let Some(tid) = tid {
                turn_id = Some(tid.to_string());
            }
        }
        match turn_role_and_text(&val) {
            ("assistant", text) if !text.is_empty() => {
                if entry_key.is_none() {
                    entry_key = val
                        .get("uuid")
                        .and_then(Value::as_str)
                        .filter(|u| !u.is_empty())
                        .map(str::to_string)
                        .or_else(|| {
                            val.pointer("/payload/id")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        });
                }
                texts_rev.push(text);
            }
            ("user", _) => break,
            _ => {}
        }
    }
    let distress_text = texts_rev.iter().find_map(|t| {
        extract_help_distress(t)
            .or_else(|| extract_result_blocked(t))
            .map(|_| t.clone())
    });
    let turn_key = turn_id.or(entry_key).unwrap_or_else(|| {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for t in texts_rev.iter().rev() {
            t.hash(&mut h);
        }
        format!("hash-{:016x}", h.finish())
    });
    Some(TurnRead {
        turn_key,
        distress_text,
    })
}

/// True when the project log already carries a `blocked` row for this run
/// with this class on this turn. The stop hook re-fires on every turn end,
/// so without this a session whose stopping turn still carries the same
/// distress would re-write a row on each fire; a NEW turn with the same
/// class appends and climbs (the rung count), because the run moved and the
/// class recurred.
fn blocked_distress_already_emitted(
    project_events: &Path,
    run: &str,
    class: HelpClass,
    turn_key: &str,
) -> bool {
    blocked_rows_for(project_events, run, |v| {
        v.pointer("/data/class").and_then(|x| x.as_str()) == Some(class.as_str())
            && v.pointer("/data/turn").and_then(|x| x.as_str()) == Some(turn_key)
    })
    .into_iter()
    .next()
    .is_some()
}

/// Every committed `blocked` row for this run, narrowed by a predicate on
/// the parsed envelope. Committed rows, not journal bytes: the store commit
/// is the write boundary, so the dedup reads what a reader would see.
/// Import first: pre-cutover bytes beside the journal are part of the
/// history the dedup must see, and a read on an absent store is an honest
/// no.
fn blocked_rows_for(project_events: &Path, run: &str, pred: impl Fn(&Value) -> bool) -> Vec<Value> {
    let _ = crate::event_store::import_all(project_events);
    let Ok(rows) = crate::event_store::query_events(
        project_events,
        &crate::event_store::EventQuery {
            types: vec!["blocked".to_string()],
            ..Default::default()
        },
    ) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|r| serde_json::from_str::<Value>(&r.line).ok())
        .filter(|v| v.get("run").and_then(|x| x.as_str()) == Some(run))
        .filter(|v| pred(v))
        .collect()
}

/// Free-text cap shared with the emit CLI (`_PROTOCOL_DATA_STR_CAP`) and
/// finalize's run_summary cap, so a Rust-emitted reason can never outgrow the
/// rows every other producer writes.
const BLOCKED_DATA_STR_CAP: usize = 500;

/// Read the stopping message for a `<help>` tag and, on a hit, append the
/// blocked row - the read+emit chain shared by loop_check's manifest-bearing
/// stop, the stop hook's every-fire scan, and a pre-manifest
/// visitor-allowed exit, so there is one implementation and no second copy
/// to drift. `last_assistant_message` still wins for the distress TEXT when
/// present, but the turn read ALSO runs: it supplies the turn key and finds
/// a help in an earlier assistant entry of the same turn that the
/// newest-entry-only read lost. Returns the class and rung when a row was
/// written.
pub(crate) fn scan_and_emit(
    project_events: &Path,
    global_events: &Path,
    cwd: &Path,
    run: &str,
    node: Option<&str>,
    harness: Option<&str>,
    transcript_path: &Path,
    last_assistant_message: Option<&str>,
) -> Option<(HelpClass, u64)> {
    // The payload parse leads; the turn read always runs for the key and
    // covers the payload miss. A fire with no transcript (loopcheck payloads
    // without transcript_path) keys on the payload text hash, the best key
    // it has, and still dedups an identical re-fire.
    let turn = read_stopping_turn(transcript_path);
    let payload_distress = last_assistant_message
        .and_then(|t| extract_help_distress(t).or_else(|| extract_result_blocked(t)));
    let distress = payload_distress.or_else(|| {
        turn.as_ref()
            .and_then(|t| t.distress_text.as_deref())
            .and_then(|t| extract_help_distress(t).or_else(|| extract_result_blocked(t)))
    });
    let Some(distress) = distress else {
        return None;
    };
    let turn_key = turn.map(|t| t.turn_key).unwrap_or_else(|| {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        last_assistant_message.unwrap_or_default().hash(&mut h);
        format!("hash-{:016x}", h.finish())
    });
    let rung = emit_help_distress_blocked(
        project_events,
        global_events,
        cwd,
        run,
        node,
        harness,
        &distress,
        &turn_key,
    )?;
    Some((distress.class, rung))
}

/// Emit the `blocked` event natively, route it, and push it to the
/// parent handle. The push leg and the emit-CLI auto-push shipped with zero
/// emitters (the advisory `--emit-boundary blocked` instruction demonstrably
/// never fires, so a lead got swept on a timeout instead of being told). The
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
    turn_key: &str,
) -> Option<u64> {
    let Some(rung) = append_blocked_event(
        project_events,
        global_events,
        run,
        node,
        harness,
        distress,
        turn_key,
    ) else {
        return None;
    };
    push_blocked_to_parent(cwd, run, node, &distress.reason);
    crate::help_router::route_emitted_distress(
        cwd,
        run,
        node,
        distress.class,
        &distress.reason,
        distress.evidence.as_deref(),
        rung,
        turn_key,
    );
    Some(rung)
}

/// Append the deduped `blocked` envelope to both logs. Returns the rung the
/// row was written at (None = duplicate, or the durable append failed and
/// no push may ride it - AC1-FR ordering, same as the emit CLI: a push with
/// no durable record is the receipt-can-lie shape).
fn append_blocked_event(
    project_events: &Path,
    global_events: &Path,
    run: &str,
    node: Option<&str>,
    harness: Option<&str>,
    distress: &HelpDistress,
    turn_key: &str,
) -> Option<u64> {
    let cap = |s: &str| -> String { s.chars().take(BLOCKED_DATA_STR_CAP).collect() };
    let reason = cap(&distress.reason);
    // Dedup key is run + class + turn (F4): a re-fire on the same turn
    // writes nothing; a new turn with the same class writes a row and the
    // rung count climbs. The stored row carries the CAPPED reason and the
    // capped turn key, so the comparison joins on stored values.
    if blocked_distress_already_emitted(project_events, run, distress.class, turn_key) {
        return None;
    }
    let rung = blocked_rows_for(project_events, run, |v| {
        v.pointer("/data/class").and_then(|x| x.as_str()) == Some(distress.class.as_str())
    })
    .len() as u64;
    let mut data = serde_json::json!({
        "reason": reason,
        "kind": distress.kind.as_str(),
        "class": distress.class.as_str(),
        "turn": cap(turn_key),
        "rung": rung,
    });
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
        return None;
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
    Some(rung)
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
    let turn = read_stopping_turn(&transcript);
    let Some(distress) = turn
        .as_ref()
        .and_then(|t| t.distress_text.as_deref())
        .and_then(|t| extract_help_distress(t).or_else(|| extract_result_blocked(t)))
    else {
        println!("distress: none");
        return 0;
    };
    let project_events = events_path.unwrap_or_else(|| crate::paths::events_path(&cwd));
    let global_events =
        global_events_path.unwrap_or_else(crate::loopcheck::default_global_events_path);
    // Already parsed above: call the emitter directly instead of
    // scan_and_emit, which would parse the same text for a <help> tag again.
    let reason = distress.reason.clone();
    let turn_key = turn.map(|t| t.turn_key).unwrap_or_default();
    let wrote = emit_help_distress_blocked(
        &project_events,
        &global_events,
        &cwd,
        &run,
        node.as_deref(),
        harness.as_deref(),
        &distress,
        &turn_key,
    );
    if wrote.is_some() {
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
/// F6: does the newest user entry of the transcript carry the cancel verb?
/// The user-typed `/fno:cancel-target` (claude) or `$fno:cancel-target`
/// (codex) is the authoritative cancel; the session's own assistant write
/// is not. The newest user entry ends the walk; anything newer is assistant
/// text written after the user typed.
pub(crate) fn newest_user_entry_carries_cancel(transcript_path: &Path) -> bool {
    let mut window_bytes: u64 = 1 << 20;
    let window = loop {
        let candidate = crate::loopcheck::read_tail_lines(transcript_path, window_bytes);
        if (candidate.len() >= 2 && turn_boundary_in_window(&candidate))
            || window_bytes >= (64u64 << 20)
        {
            break candidate;
        }
        window_bytes *= 4;
    };
    for line in window.iter().rev() {
        if let Ok(val) = serde_json::from_str::<Value>(line) {
            let (role, text) = turn_role_and_text(&val);
            if role == "user" {
                let trimmed = text.trim_start();
                return trimmed.starts_with("/fno:cancel-target")
                    || trimmed.starts_with("$fno:cancel-target");
            }
        }
    }
    false
}

#[cfg(test)]
pub(crate) fn fno_bin_env_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write_exec_stub as write_exec;

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
    fn blocked_distress_dedup_scopes_to_run_class_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let mk = |run: &str, class: &str, turn: &str| {
            serde_json::to_string(&serde_json::json!({
                "ts": "2026-01-01T00:00:00Z", "source": "test",
                "type": "blocked", "run": run,
                "data": {"reason": "r", "class": class, "turn": turn}
            }))
            .unwrap()
                + "\n"
        };
        std::fs::write(&path, format!("{}\n", mk("run-a", "stuck", "turn-1"))).unwrap();
        // Same run + class + turn -> already emitted.
        assert!(blocked_distress_already_emitted(
            &path,
            "run-a",
            HelpClass::Stuck,
            "turn-1"
        ));
        // A different turn key on the same run -> not a duplicate (the run
        // moved on; the class recurred and climbs).
        assert!(!blocked_distress_already_emitted(
            &path,
            "run-a",
            HelpClass::Stuck,
            "turn-2"
        ));
        // A different class on the same turn -> not a duplicate.
        assert!(!blocked_distress_already_emitted(
            &path,
            "run-a",
            HelpClass::CiRed,
            "turn-1"
        ));
        // Same class + turn on a different run -> not a duplicate.
        assert!(!blocked_distress_already_emitted(
            &path,
            "run-b",
            HelpClass::Stuck,
            "turn-1"
        ));
        // A missing log is an honest no, not a corrupted yes.
        let absent = tmp.path().join("absent.jsonl");
        assert!(!blocked_distress_already_emitted(
            &absent,
            "run-a",
            HelpClass::Stuck,
            "turn-1"
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
            class: HelpClass::Stuck,
        };
        assert!(append_blocked_event(
            &project,
            &global,
            "run-a",
            Some("x-aaaa"),
            Some("codex"),
            &d,
            "turn-1"
        )
        .is_some());
        let rows: Vec<serde_json::Value> = crate::events::committed_journal_text(&project)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 1, "exactly one row on first distress");
        let row = &rows[0];
        assert_eq!(row["type"], "blocked");
        assert_eq!(row["source"], "target");
        assert_eq!(row["run"], "run-a");
        assert_eq!(row["node"], "x-aaaa");
        assert_eq!(row["harness"], "codex");
        assert_eq!(row["data"]["reason"], "missing dependency");
        assert_eq!(row["data"]["evidence"], "plan 4.2");
        assert_eq!(row["data"]["class"], "stuck");
        assert_eq!(row["data"]["turn"], "turn-1");
        assert_eq!(row["data"]["rung"], 0);
        assert_eq!(
            crate::events::committed_journal_text(&global).trim(),
            serde_json::to_string(&row).unwrap(),
            "the global mirror carries the identical row"
        );
        assert!(append_blocked_event(
            &project,
            &global,
            "run-a",
            Some("x-aaaa"),
            Some("codex"),
            &d,
            "turn-1"
        )
        .is_none());
        assert_eq!(
            crate::events::committed_journal_text(&project)
                .lines()
                .count(),
            1,
            "same turn re-fire writes nothing"
        );
        assert!(append_blocked_event(
            &project,
            &global,
            "run-a",
            Some("x-aaaa"),
            Some("codex"),
            &d,
            "turn-2"
        )
        .is_some());
        assert_eq!(
            crate::events::committed_journal_text(&project)
                .lines()
                .count(),
            2,
            "new turn same class climbs"
        );
        let row2: serde_json::Value = serde_json::from_str(
            crate::events::committed_journal_text(&project)
                .lines()
                .last()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(row2["data"]["rung"], 1);
        let d2 = HelpDistress {
            reason: "second wall".to_string(),
            evidence: None,
            kind: DistressKind::Help,
            class: HelpClass::Unclassified,
        };
        assert!(
            append_blocked_event(&project, &global, "run-a", None, None, &d2, "turn-1").is_some()
        );
        let rows: Vec<serde_json::Value> = crate::events::committed_journal_text(&project)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 3);
        assert!(
            rows[2].get("harness").is_none(),
            "no harness known must omit the key"
        );
        assert_eq!(rows[2]["data"]["class"], "unclassified");
    }

    #[test]
    fn emit_help_distress_blocked_caps_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("events.jsonl");
        let global = tmp.path().join("global2.jsonl");
        let long: String = "x".repeat(BLOCKED_DATA_STR_CAP + 50);
        let d = HelpDistress {
            class: HelpClass::Question,
            reason: long.clone(),
            evidence: None,
            kind: DistressKind::Help,
        };
        assert!(
            append_blocked_event(&project, &global, "run-a", None, None, &d, "turn-1").is_some()
        );
        let row: serde_json::Value = serde_json::from_str(
            crate::events::committed_journal_text(&project)
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        let capped: String = "x".repeat(BLOCKED_DATA_STR_CAP);
        assert_eq!(row["data"]["reason"], serde_json::json!(capped));
        assert!(
            append_blocked_event(&project, &global, "run-a", None, None, &d, "turn-1").is_none()
        );
        assert_eq!(
            crate::events::committed_journal_text(&project)
                .lines()
                .count(),
            1
        );
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
        assert!(wrote.is_none());
        assert!(!project.exists());
        assert!(!global.exists());
    }

    #[test]
    fn scan_and_emit_takes_the_transcript_fallback_and_stamps_harness() {
        // AC1-HP / AC3-HP through the same entry both stop paths call: no
        // last_assistant_message (the agy/opencode/codex shape), so the
        // native turn read supplies the tag; the caller harness lands on
        // the envelope.
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("rollout-2026-09-06T00-00-00-cx-1.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"payload":{"role":"user","content":[{"type":"input_text","text":"go"}]}}"#, "\n",
                r#"{"payload":{"role":"assistant","content":[{"type":"output_text","text":"<help reason=\"worktree-init-blocked\" evidence=\"Operation not permitted\">detail</help>"}]}}"#, "\n",
            ),
        )
        .unwrap();
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
        assert!(wrote.is_some());
        let row: serde_json::Value =
            serde_json::from_str(&crate::events::committed_journal_text(&project)).unwrap();
        assert_eq!(row["data"]["reason"], "worktree-init-blocked");
        assert_eq!(row["data"]["evidence"], "Operation not permitted");
        assert_eq!(row["node"], "x-bbbb");
        assert_eq!(row["harness"], "codex");
        assert_eq!(row["data"]["class"], "unclassified");
    }

    /// The `agents newest-assistant-text --transcript <path>` contract this
    /// crate cannot itself parse (that reader lives in Python, `peek.py`):
    /// read the record's `payload.content[0].text`, the same field the real
    /// reader returns for a codex rollout line.
    // The `agents newest-assistant-text` reader stub this file once needed
    // is gone with the native turn read: fixtures parse in-process.

    #[test]
    fn run_distress_scan_reads_the_checked_in_codex_fixture() {
        // AC10-HP / AC11-EDGE through the actual verb entry point
        // (run_distress_scan), against the real rollout line checked in at
        // tests/fixtures/rollout-codex-help.jsonl, parsed natively (no
        // reader stub). The tag-free rerun is the positive control for the
        // dedup: it must add no second row.
        let tmp = tempfile::tempdir().unwrap();
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/rollout-codex-help.jsonl");
        let project = tmp.path().join("events.jsonl");
        let global = tmp.path().join("global.jsonl");
        let args = |path: &std::path::Path, run: &str| -> Vec<String> {
            [
                "distress-scan",
                "--transcript",
                path.to_str().unwrap(),
                "--run",
                run,
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
            .collect()
        };
        let code = run_distress_scan(&args(&fixture, "fixture-run"));
        // The tag-free copy of the same line: same run, new turn key would
        // differ, so write it under its own run to prove the parse misses.
        let text = std::fs::read_to_string(&fixture).unwrap();
        let mut rec: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        rec["payload"]["content"][0]["text"] = serde_json::json!("all clear, nothing stuck here");
        let no_help_fixture = tmp.path().join("no-help.jsonl");
        std::fs::write(&no_help_fixture, serde_json::to_string(&rec).unwrap()).unwrap();
        let code2 = run_distress_scan(&args(&no_help_fixture, "fixture-run-2"));
        assert_eq!(code, 0);
        assert_eq!(code2, 0);
        let rows: Vec<serde_json::Value> = crate::events::committed_journal_text(&project)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "the tag-free rerun must add no second row: {rows:?}"
        );
        assert_eq!(rows[0]["harness"], "codex");
        assert_eq!(rows[0]["data"]["class"], "unclassified");
        assert_eq!(
            rows[0]["data"]["turn"], "01a077be-60f6-7432-bdc2-0018b8f20840",
            "the codex turn_id is the dedup key"
        );
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
        // The preferred JSON object, fenced. The fence opens its own line,
        // the contract shape: prose glued to the opener is not a fence.
        let fenced = concat!(
            "work so far committed.\n",
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
        assert!(wrote.is_some());
        let row: serde_json::Value =
            serde_json::from_str(&crate::events::committed_journal_text(&project)).unwrap();
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
        assert!(wrote2.is_some());
        let row2: serde_json::Value =
            serde_json::from_str(&crate::events::committed_journal_text(&project2)).unwrap();
        assert_eq!(row2["data"]["kind"], "result_blocked");
        assert_eq!(row2["data"]["reason"], "gate refused");
        // AC3-EDGE: a second stop on the same message appends nothing - the
        // existing (run, capped reason) dedup covers the new path.
        assert!(scan_and_emit(
            &project2,
            &global2,
            tmp2.path(),
            "run-b",
            None,
            None,
            &transcript2,
            Some(json_msg)
        )
        .is_none());
        assert_eq!(
            crate::events::committed_journal_text(&project2)
                .lines()
                .count(),
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
        assert!(wrote.is_some());
        let row: serde_json::Value =
            serde_json::from_str(&crate::events::committed_journal_text(&project)).unwrap();
        assert_eq!(row["data"]["kind"], "help");
        assert_eq!(row["data"]["reason"], "missing dependency");
        assert_eq!(
            crate::events::committed_journal_text(&project)
                .lines()
                .count(),
            1,
            "exactly one row when both vocabularies appear"
        );
    }

    #[test]
    fn run_distress_scan_covers_the_result_blocked_path() {
        // The CLI verb's own parse (the shape the pre-deploy done_probe
        // exercises): the transcript carries a RESULT: BLOCKED return and
        // the scan emits one result_blocked row, keyed by the entry uuid.
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("rollout-result-blocked.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"payload":{"role":"user","content":[{"type":"input_text","text":"go"}]}}"#, "\n",
                r#"{"payload":{"role":"assistant","content":[{"type":"output_text","text":"RESULT: BLOCKED\nREASON: probe reason"}]},"id":"msg_1"}"#, "\n",
            ),
        ).unwrap();
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
        assert_eq!(code, 0);
        let row: serde_json::Value =
            serde_json::from_str(&crate::events::committed_journal_text(&project)).unwrap();
        assert_eq!(row["type"], "blocked");
        assert_eq!(row["data"]["kind"], "result_blocked");
        assert_eq!(row["data"]["reason"], "probe reason");
        assert_eq!(row["data"]["turn"], "msg_1");
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
        )
        .is_some());
        let row: serde_json::Value =
            serde_json::from_str(&crate::events::committed_journal_text(&project)).unwrap();
        assert_eq!(row["data"]["kind"], "help");
    }

    #[test]
    fn an_undeclared_loopcheck_fno_bin_is_refused_under_a_unit_test() {
        // AC2/AC4 for the loopcheck seam: a declared stub passes through
        // unchanged; unset under cfg!(test) the resolver answers a path that
        // cannot exec and whose text names the remedy.
        let _env_guard = fno_bin_env_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let var = "FNO_LOOPCHECK_FNO_BIN";
        let prior = std::env::var(var).ok();
        std::env::remove_var(var);
        let resolved = loopcheck_fno_bin();
        let tmp = tempfile::tempdir().unwrap();
        let stub = write_exec(tmp.path(), "fno", "#!/bin/sh\nexit 0\n");
        std::env::set_var(var, stub.to_str().unwrap());
        let declared = loopcheck_fno_bin();
        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
        assert_ne!(resolved, "fno");
        assert!(!Path::new(&resolved).exists());
        assert!(resolved.contains("FNO_BIN"));
        assert_eq!(declared, stub.to_str().unwrap());
    }

    #[test]
    fn a_blocked_push_with_no_declared_fno_execs_nothing() {
        // AC1: with a declared recording stub the push fires and the stub
        // receives the argv (positive control); with the var removed the row
        // still lands in events.jsonl and the stub log gains nothing - the
        // durable row never depended on the push.
        let _env_guard = fno_bin_env_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let var = "FNO_LOOPCHECK_FNO_BIN";
        let prior = std::env::var(var).ok();
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls.log");
        let body = format!(
            "#!/bin/sh\nprintf '%s\\t%s\\n' \"$PWD\" \"$*\" >> {}\nexit 1\n",
            log.to_string_lossy()
        );
        let stub = write_exec(tmp.path(), "fno", &body);
        std::env::set_var(var, stub.to_str().unwrap());
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
        assert!(wrote.is_some());
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(
            calls.contains(
                "doctor event push-parent --type blocked --run run-a --reason probe reason"
            ),
            "the declared stub must receive the push argv, got: {calls}"
        );
        std::env::remove_var(var);
        let project2 = tmp.path().join("events2.jsonl");
        let global2 = tmp.path().join("global2.jsonl");
        let wrote2 = scan_and_emit(
            &project2,
            &global2,
            tmp.path(),
            "run-c",
            None,
            None,
            &transcript,
            Some("RESULT: BLOCKED\nREASON: probe reason"),
        );
        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
        assert!(wrote2.is_some());
        let row: serde_json::Value =
            serde_json::from_str(&crate::events::committed_journal_text(&project2)).unwrap();
        assert_eq!(row["data"]["reason"], "probe reason");
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            calls,
            "no exec once the fno is undeclared"
        );
    }

    #[test]
    fn help_class_parses_kebab_and_unknown_reads_unclassified() {
        // AC1/AC2: the class attribute parses kebab-case; missing or
        // unknown reads Unclassified, so the router stays total.
        assert_eq!(HelpClass::parse("ci-red"), HelpClass::CiRed);
        assert_eq!(HelpClass::parse("stale-plan"), HelpClass::StalePlan);
        assert_eq!(HelpClass::parse("budget"), HelpClass::Budget);
        assert_eq!(HelpClass::parse("bogus"), HelpClass::Unclassified);
        let tag = extract_help_distress(r#"<help class="ci-red" reason="x">"#).unwrap();
        assert_eq!(tag.class, HelpClass::CiRed);
        let bare = extract_help_distress("<help>").unwrap();
        assert_eq!(bare.class, HelpClass::Unclassified);
        let blocked = extract_result_blocked("RESULT: BLOCKED\nREASON: r");
        assert_eq!(blocked.unwrap().class, HelpClass::Unclassified);
    }

    #[test]
    fn turn_read_finds_a_help_in_an_earlier_entry_of_the_same_turn() {
        // F2/AC3: the newest-entry-only read lost a help carried by an
        // earlier assistant entry of the same turn; the turn read finds it.
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("claude-turn.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"message":{"role":"user","content":"go"}}"#, "\n",
                r#"{"message":{"role":"assistant","content":[{"type":"text","text":"<help class=\"stuck\" reason=\"early\">ev</help>"}]},"uuid":"ua-1"}"#, "\n",
                r#"{"message":{"role":"assistant","content":[{"type":"text","text":"still going, plain text"}]},"uuid":"ua-2"}"#, "\n",
            ),
        )
        .unwrap();
        let turn = read_stopping_turn(&transcript).unwrap();
        let distress = turn
            .distress_text
            .as_deref()
            .and_then(extract_help_distress)
            .expect("the earlier-entry help must be found");
        assert_eq!(distress.class, HelpClass::Stuck);
        assert_eq!(turn.turn_key, "ua-2", "key is the newest assistant uuid");
        // A turn with no help at all reads distress-free.
        let transcript2 = tmp.path().join("plain-turn.jsonl");
        std::fs::write(
            &transcript2,
            concat!(
                r#"{"message":{"role":"user","content":"go"}}"#, "\n",
                r#"{"message":{"role":"assistant","content":[{"type":"text","text":"plain"}]},"uuid":"ub-1"}"#, "\n",
            ),
        )
        .unwrap();
        let turn2 = read_stopping_turn(&transcript2).unwrap();
        assert_eq!(turn2.distress_text, None);
    }

    #[test]
    fn classed_fixture_turns_write_classed_rows() {
        // AC4/AC9: the checked-in claude and codex fixtures each write one
        // blocked row whose data.class matches, keyed on their turn id.
        let claude_fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/distress/claude-classed-help.jsonl");
        let codex_fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/distress/codex-classed-help.jsonl");
        for (fixture, expect_class, expect_turn) in [
            (&claude_fixture, "stuck", "u-assistant-late"),
            (&codex_fixture, "stale-plan", "01jad4c-turn-codex"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let project = tmp.path().join("events.jsonl");
            let global = tmp.path().join("global.jsonl");
            let wrote = scan_and_emit(
                &project,
                &global,
                tmp.path(),
                &format!("run-{expect_class}"),
                Some("x-fix"),
                None,
                fixture,
                None,
            );
            assert!(wrote.is_some(), "fixture {fixture:?} must write a row");
            let row: serde_json::Value =
                serde_json::from_str(&crate::events::committed_journal_text(&project)).unwrap();
            assert_eq!(row["data"]["class"], expect_class);
            assert_eq!(row["data"]["turn"], expect_turn);
            assert_eq!(row["data"]["rung"], 0);
        }
        // AC10: the tag-free rerun of a fixture writes nothing.
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("events.jsonl");
        let global = tmp.path().join("global.jsonl");
        let wrote = scan_and_emit(
            &project,
            &global,
            tmp.path(),
            "run-plain",
            None,
            None,
            &claude_fixture.join("no-such-file.jsonl"),
            Some("all clear, nothing stuck here"),
        );
        assert!(wrote.is_none());
        assert!(!project.exists());
    }

    #[test]
    fn a_second_turn_with_the_same_class_climbs_the_rung() {
        // AC5/AC6: two stop fires on the same turn with reworded reasons
        // write one row; a new turn with the same class writes rung 1.
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("events.jsonl");
        let global = tmp.path().join("global.jsonl");
        let mk = |reason: &str| HelpDistress {
            reason: reason.to_string(),
            evidence: None,
            kind: DistressKind::Help,
            class: HelpClass::StalePlan,
        };
        assert!(append_blocked_event(
            &project,
            &global,
            "run-a",
            None,
            None,
            &mk("wall a"),
            "turn-1"
        )
        .is_some());
        assert!(append_blocked_event(
            &project,
            &global,
            "run-a",
            None,
            None,
            &mk("wall a, reworded"),
            "turn-1"
        )
        .is_none());
        assert!(append_blocked_event(
            &project,
            &global,
            "run-a",
            None,
            None,
            &mk("wall b"),
            "turn-2"
        )
        .is_some());
        let rows: Vec<serde_json::Value> = crate::events::committed_journal_text(&project)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["data"]["rung"], 1);
    }

    #[test]
    fn every_help_class_has_a_route_and_texts_agree() {
        // AC23 groundwork: every class routes somewhere; the in-session
        // classes carry their plan texts verbatim; the wait backoffs stay
        // under the 15-minute watcher cap.
        let all = [
            HelpClass::StalePlan,
            HelpClass::MissingPrereq,
            HelpClass::CiRed,
            HelpClass::Stuck,
            HelpClass::Held,
            HelpClass::Wait,
            HelpClass::EnvDenied,
            HelpClass::GateDeadlock,
            HelpClass::GateUnsatisfiable,
            HelpClass::Question,
            HelpClass::Budget,
            HelpClass::Unclassified,
        ];
        for class in all {
            for rung in [0u64, 1, 2, 5] {
                match crate::help_router::route(class, rung) {
                    crate::help_router::Route::InSession(text) => assert!(!text.is_empty()),
                    crate::help_router::Route::OffSession { text, .. } => {
                        assert!(!text.is_empty())
                    }
                    crate::help_router::Route::Timer { backoff_secs } => {
                        assert!(backoff_secs <= 900, "watcher cap is 15m");
                    }
                }
            }
        }
        for (class, text) in [
            (HelpClass::StalePlan, crate::help_router::STALE_PLAN_TEXT),
            (
                HelpClass::MissingPrereq,
                crate::help_router::MISSING_PREREQ_TEXT,
            ),
            (HelpClass::CiRed, crate::help_router::CI_RED_TEXT),
            (HelpClass::Stuck, crate::help_router::STUCK_TEXT),
        ] {
            match crate::help_router::route(class, 0) {
                crate::help_router::Route::InSession(t) => assert_eq!(t, text),
                other => panic!("class {class:?} must be in-session at rung 0: {other:?}"),
            }
        }
    }
}
