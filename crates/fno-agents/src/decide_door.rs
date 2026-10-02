//! The decide door: the `fno inbox decide` / `fno backlog decide`
//! record verb, its provenance lanes, and the decide-reindex recovery
//! verb, ported from the deleted Python decide family. Shared law-door
//! helpers stay in `law_match`; this module reaches them through
//! `crate::law_match`.

use serde_json::{json, Value};

use crate::law_match::{
    all_decision_rows, attended_terminal, evidence_repo_root, is_retraction_row, mint_decision_id,
    now_iso, project_events_journal, text_cap, WAIVER_SUBJECT_PREFIX,
};

// ---------------------------------------------------------------------------
// The decide door: the `decide` mode carrying the `fno inbox decide` /
// `fno backlog decide` argv. The port of `_record` + `record_decision`
// (cli/src/fno/decide/cli.py, cli/src/fno/decide/__init__.py, deleted): the
// same gate order, the same refusal texts, the same receipt lines, and the
// same exit contract (0 recorded, 1 recorded-but-index-failed or a failed
// write, 2 a bad --authority or graduation, 3 refused). The law door above
// is the chat_attested/operator slice of this door; the decide door adds the
// agent, crown, and beastmode lanes with their origin floor, relayed_by
// stamp, coord closure key, and waiver guard.
// ---------------------------------------------------------------------------

/// The closed mail-origin vocabulary (`MAIL_ORIGINS`). Origin is evidence
/// about the channel, never an authority claim.
pub(crate) const MAIL_ORIGINS: &[&str] = &["operator", "peer", "scheduler", "recovery"];

/// The highest authority an origin may carry (`MAX_AUTHORITY_BY_ORIGIN`).
fn max_authority_by_origin(origin: &str) -> &'static str {
    match origin {
        "operator" => "operator",
        _ => "agent",
    }
}

/// The authority lanes the decide CLI accepts (`AUTHORITY_SOURCES`);
/// `chat_attested` is the law door's lane and never a decide flag value.
const AUTHORITY_SOURCES: &[&str] = &["operator", "crown", "agent", "beastmode"];

/// The caller identity a decide run resolves under: `Ambient` reads the
/// process truth (the ancestry prover); `Forced` injects a handle (`Some`)
/// or a scrubbed identity (`None`), the seam the tests drive where the law
/// door injects a caller.
#[derive(Clone, Copy)]
pub(crate) enum DecideIdentity {
    Ambient,
    #[cfg_attr(not(test), expect(dead_code))]
    Forced(Option<&'static str>),
}

impl DecideIdentity {
    fn agent(self) -> Option<String> {
        match self {
            Self::Ambient => {
                let get = |name: &str| std::env::var(name).ok();
                let ident = crate::spawn_context::resolve_self_identity(
                    &get,
                    None,
                    None,
                    &crate::paths::AgentsHome::from_env(),
                );
                let session = ident.session_id.as_deref().map(str::trim).unwrap_or("");
                let harness = ident.harness.as_deref().map(str::trim).unwrap_or("");
                if session.is_empty() || harness.is_empty() {
                    None
                } else {
                    Some(crate::identity::canonical_handle(session))
                }
            }
            Self::Forced(None) => None,
            Self::Forced(Some(h)) => Some((*h).to_string()),
        }
    }
}

/// The authority lanes the origin floor can cap (`enforce_origin_floor`): an
/// ambient agent identity cannot declare an origin above peer.
pub(crate) fn enforce_origin_floor(origin: Option<&str>, id: DecideIdentity) -> Option<String> {
    let origin = origin?;
    if origin == "peer" {
        return Some(origin.to_string());
    }
    if id.agent().is_some() {
        return Some("peer".to_string());
    }
    Some(origin.to_string())
}

/// Who recorded a ruling, and how much of that a reader may trust
/// (`Provenance`, a named struct in Python for the same reason).
pub(crate) struct DecideProvenance {
    pub(crate) decided_by: String,
    pub(crate) authority_source: Option<String>,
    pub(crate) attested_by: Option<String>,
    pub(crate) relayed_by: Option<String>,
}

/// The provenance refusals, each carrying the exact Python error text so the
/// CLI's remedy wording stays one law.
pub(crate) enum DecideRefusal {
    UnknownOrigin(String),
    RefusedAuthority {
        agent: String,
        origin: Option<String>,
    },
    UnattributedAuthority,
}

/// The decide door's parsed argv.
pub(crate) struct DecideDoor {
    subject: Option<String>,
    decision: Option<String>,
    question_id: Option<String>,
    rationale: Option<String>,
    options: Vec<String>,
    supersedes: Option<String>,
    decided_by: Option<String>,
    authority: Option<String>,
    graduation: Option<String>,
    graduation_ref: Option<String>,
    reads: Vec<String>,
    origin: Option<String>,
}

const DECIDE_USAGE: &str = "usage: fno inbox decide <subject> <decision> [--question-id q] [--rationale s] [--option s]... [--supersedes d-x] [--decided-by name] [--authority operator|crown|agent|beastmode] [--graduation k] [--graduation-ref r] [--read cmd]... [--origin o]";

pub(crate) fn parse_decide_door(args: &[String]) -> Result<DecideDoor, String> {
    let mut door = DecideDoor {
        subject: None,
        decision: None,
        question_id: None,
        rationale: None,
        options: Vec::new(),
        supersedes: None,
        decided_by: None,
        authority: None,
        graduation: None,
        graduation_ref: None,
        reads: Vec::new(),
        origin: None,
    };
    let mut positional = 0usize;
    let mut i = 0usize;
    while i < args.len() {
        let (flag, inline) = match args[i].split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (args[i].clone(), None),
        };
        let take = |i: &mut usize| -> Result<String, String> {
            if let Some(v) = &inline {
                return Ok(v.clone());
            }
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value\n{DECIDE_USAGE}"))
        };
        match flag.as_str() {
            "--subject" => {
                let v = take(&mut i)?;
                if door.subject.is_some() {
                    return Err("pass either <subject> or --subject (deprecated), not both".into());
                }
                eprintln!(
                    "warning: --subject is deprecated; use <subject> instead. \
The alias will be removed in a future release."
                );
                door.subject = Some(v);
            }
            "--decision" => {
                let v = take(&mut i)?;
                if door.decision.is_some() {
                    return Err(
                        "pass either <decision> or --decision (deprecated), not both".into(),
                    );
                }
                eprintln!(
                    "warning: --decision is deprecated; use <decision> instead. \
The alias will be removed in a future release."
                );
                door.decision = Some(v);
            }
            "--question-id" => door.question_id = Some(take(&mut i)?),
            "--rationale" => door.rationale = Some(take(&mut i)?),
            "--option" => door.options.push(take(&mut i)?),
            "--supersedes" => door.supersedes = Some(take(&mut i)?),
            "--decided-by" => door.decided_by = Some(take(&mut i)?),
            "--authority" => door.authority = Some(take(&mut i)?),
            "--graduation" => door.graduation = Some(take(&mut i)?),
            "--graduation-ref" => door.graduation_ref = Some(take(&mut i)?),
            "--read" => door.reads.push(take(&mut i)?),
            "--origin" => door.origin = Some(take(&mut i)?),
            f if f.starts_with('-') && f != "-" => {
                return Err(format!("no such option: {f}\n{DECIDE_USAGE}"));
            }
            _ => {
                positional += 1;
                if positional == 1 {
                    door.subject = Some(args[i].clone());
                } else if positional == 2 {
                    door.decision = Some(args[i].clone());
                } else {
                    return Err(format!("too many positional arguments\n{DECIDE_USAGE}"));
                }
            }
        }
        i += 1;
    }
    Ok(door)
}

/// The stored-provenance to lane map (`_decision_lane`).
pub(crate) fn decision_lane(row: &Value) -> &'static str {
    let authority = row
        .get("authority_source")
        .and_then(Value::as_str)
        .unwrap_or("");
    match authority {
        "agent" | "crown" => "coord",
        "beastmode" => "grant",
        "operator" | "chat_attested" => {
            let ts = row.get("ts").and_then(Value::as_str).unwrap_or("");
            if ts >= crate::decision_index::LAW_LANE_CUTOVER {
                "law"
            } else {
                "unattributed"
            }
        }
        _ => "unattributed",
    }
}

/// The claim context receipt (`_render_claim_receipt`): advisory, and a
/// receipt failure cannot undo a ruling.
fn render_decide_claim_receipt(node_id: &str, decided_by: &str) {
    let key = format!("node:{node_id}");
    let caller_label = if decided_by.is_empty() {
        "unknown caller"
    } else {
        decided_by
    };
    let (state, holder, error) = {
        let (state, record) = crate::claims::status(&key, None);
        (
            state.as_str().to_string(),
            record.map(|r| r.holder),
            None::<String>,
        )
    };
    let mut line = format!("decide: claim {key}; claim state: {state}; caller: {caller_label}");
    if state == "free" {
        eprintln!("{line}");
        return;
    }
    if let Some(h) = &holder {
        line.push_str(&format!("; holder: {h}"));
    }
    let mut comparison = "caller comparison unavailable".to_string();
    if (state == "live" || state == "suspect") && holder.is_some() {
        let holder = holder.as_deref().unwrap_or_default();
        if let Some((prefix, holder_session)) = holder.split_once(':') {
            if prefix == "target-session" && !holder_session.is_empty() {
                let mut caller_session = std::env::var("TARGET_SESSION_ID")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty());
                if caller_session.is_none() {
                    let get = |name: &str| std::env::var(name).ok();
                    let ident = crate::spawn_context::resolve_self_identity(
                        &get,
                        None,
                        None,
                        &crate::paths::AgentsHome::from_env(),
                    );
                    caller_session = ident
                        .session_id
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                }
                if let Some(caller) = caller_session {
                    comparison = if holder_session == caller {
                        "this caller holds the node".to_string()
                    } else {
                        "another session holds the node, not this caller".to_string()
                    };
                }
            }
        }
    }
    if let Some(e) = error {
        line.push_str(&format!("; error: {e}"));
    }
    eprintln!("{line}; {comparison}");
}

/// The machine decision index, beside the ledger (`_decisions_index_path`).
fn decisions_jsonl_path() -> std::path::PathBuf {
    crate::decision_index::default_state_path("decisions.jsonl")
}

/// This repo's project journal, the record door's durability write.
fn decisions_project_journal() -> std::path::PathBuf {
    project_events_journal()
}

/// The machine journal (`paths.global_events_json`), when it resolves.
fn global_events_journal_path() -> Option<std::path::PathBuf> {
    crate::decision_index::default_state_path("events.jsonl")
        .exists()
        .then(|| crate::decision_index::default_state_path("events.jsonl"))
}

/// The row identity `row_key` applies (`decide/__init__.py:814`).
fn decision_index_key(row: &Value) -> (String, String) {
    let etype = row
        .get("_event_type")
        .and_then(Value::as_str)
        .unwrap_or("operator_decision")
        .to_string();
    let id = ["decision_id", "retraction_id", "target_decision_id"]
        .iter()
        .find_map(|k| row.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()))
        .unwrap_or("")
        .to_string();
    (etype, id)
}

/// Compact the raw index: drop damaged lines into a `.corrupt` sibling and
/// rewrite the good ones atomically. Returns the dropped count. The store
/// is the read boundary; this repairs the legacy raw file the Python
/// `_compact_index` repaired.
fn compact_decisions_index() -> usize {
    let path = decisions_jsonl_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return 0;
    };
    let mut good: Vec<&str> = Vec::new();
    let mut dropped: Vec<&str> = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let ok = serde_json::from_str::<Value>(line)
            .map(|v| {
                let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
                let is_type = ty == "operator_decision" || ty == "decision_retracted";
                let data = v.get("data").cloned().unwrap_or(Value::Null);
                let id = if ty == "operator_decision" {
                    data.get("decision_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                } else {
                    data.get("target_decision_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                };
                is_type && !id.is_empty()
            })
            .unwrap_or(false);
        if ok {
            good.push(line);
        } else {
            dropped.push(line);
        }
    }
    if dropped.is_empty() {
        return 0;
    }
    let corrupt = path.with_extension("jsonl.corrupt");
    if let Ok(mut fh) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&corrupt)
    {
        use std::io::Write as _;
        let _ = writeln!(fh, "{}", dropped.join("\n"));
    }
    let tmp = path.with_extension("jsonl.compact");
    if std::fs::write(&tmp, format!("{}\n", good.join("\n"))).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
    dropped.len()
}

/// The decide engine's graduation validator (`validate_graduation` in
/// cli/src/fno/decide/graduation.py, deleted): lowercase-normalized, and
/// the unknown-kind refusal names no value. The law door's twin above
/// carries its own wording; the decide texts are not that law.
fn decide_graduation(kind: Option<&str>, reference: Option<&str>) -> Result<Value, String> {
    let kind = kind.unwrap_or("").trim().to_lowercase();
    let reference = reference.unwrap_or("").trim();
    match kind.as_str() {
        "guidance" => {
            if !reference.is_empty() {
                return Err("guidance takes no graduation reference".to_string());
            }
            Ok(json!({"kind": "guidance"}))
        }
        "enforced" => {
            static ARTIFACT_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            let re = ARTIFACT_RE.get_or_init(|| {
                regex::Regex::new(r"^(file|test|doc|gate|default):\S(?:.*\S)?$").expect("parses")
            });
            if !re.is_match(reference) {
                return Err(
                    "enforced graduation requires file:, test:, doc:, gate:, or default:"
                        .to_string(),
                );
            }
            Ok(json!({"kind": "enforced", "artifact": reference}))
        }
        "should-be-enforced-but-i-did-not" => {
            static FOLLOW_UP_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            let re = FOLLOW_UP_RE.get_or_init(|| {
                regex::Regex::new(r"(?i)^node:[a-z][a-z0-9]*-[0-9a-f]+$").expect("parses")
            });
            if !re.is_match(reference) {
                return Err("should-be-enforced-but-i-did-not requires node:<id>".to_string());
            }
            Ok(json!({
                "kind": "should-be-enforced-but-i-did-not",
                "follow_up": reference.to_ascii_lowercase(),
            }))
        }
        _ => Err(
            "graduation must be enforced, guidance, or should-be-enforced-but-i-did-not"
                .to_string(),
        ),
    }
}

/// The decide wrapper: an omitted declaration defaults to guidance.
fn decide_graduation_or_guidance(
    kind: Option<&str>,
    reference: Option<&str>,
) -> Result<Value, String> {
    match (kind, reference) {
        (None, None) => Ok(json!({"kind": "guidance"})),
        _ => decide_graduation(Some(&kind.unwrap_or("")), reference),
    }
}

/// The decide-reindex verb, ported from `reindex` (cli/src/fno/decide/
/// __init__.py, deleted): compact damaged rows out of the raw index and
/// recover any journaled row the index store lacks, without minting ids.
/// The machine-wide project-roots walk the Python engine paid is not
/// re-derived: the store's migration absorbed the historical journals
/// machine-wide into graph.db, and a repo running its own decide-reindex
/// recovers its own journal-only row. Counts mirror the Python keys.
pub fn run_decide_reindex(_argv: &[String]) -> i32 {
    // A backfill cannot degrade: an unreadable store must not read as done.
    let graph_path = crate::graph_get::default_graph_path();
    if let Err(e) = crate::decision_index::read_store_rows(&graph_path, &decisions_jsonl_path()) {
        eprintln!("backlog decide-reindex: failed: {e}");
        return 1;
    }
    let repaired = compact_decisions_index();
    let (rows, _) = crate::decision_index::read_store_rows(&graph_path, &decisions_jsonl_path())
        .unwrap_or((Vec::new(), 0));
    let mut known: std::collections::BTreeSet<(String, String)> =
        rows.iter().map(decision_index_key).collect();
    let prior_keys = known.clone();
    let mut counted: std::collections::BTreeSet<(String, String)> = Default::default();
    let mut added = 0usize;
    let mut already = 0usize;
    let mut invalid = 0usize;
    let mut unusable = 0usize;
    let mut journals = vec![decisions_project_journal()];
    if let Some(g) = global_events_journal_path() {
        journals.push(g);
    }
    for journal in journals {
        let text = match crate::event_store::journal_text_checked(
            &journal,
            &crate::event_store::EventQuery::of_types(&["operator_decision", "decision_retracted"]),
        ) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for line in text.lines() {
            let Ok(env) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let ty = env.get("type").and_then(Value::as_str).unwrap_or("");
            if ty != "operator_decision" && ty != "decision_retracted" {
                continue;
            }
            let data = env.get("data").cloned().unwrap_or(Value::Null);
            let id = if ty == "operator_decision" {
                data.get("decision_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            } else {
                data.get("target_decision_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            };
            if id.is_empty() {
                unusable += 1;
                continue;
            }
            let key = (ty.to_string(), id.to_string());
            if known.contains(&key) {
                if prior_keys.contains(&key) && !counted.contains(&key) {
                    counted.insert(key.clone());
                    already += 1;
                }
                continue;
            }
            if let Err(_) = crate::event_store::append_envelope(&decisions_jsonl_path(), line, None)
            {
                invalid += 1;
            } else {
                known.insert(key);
                added += 1;
            }
        }
    }
    let counts = json!({
        "added": added,
        "already": already,
        "repaired": repaired,
        "invalid": invalid,
        "unusable": unusable,
        "total": known.len(),
    });
    println!("{counts}");
    0
}

/// The decide door entry: parse, then the gated write. The argv is the
/// `fno inbox decide` / `fno backlog decide` command line minus its verb
/// tokens.
pub fn run_decide_door(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{DECIDE_USAGE}");
        return 0;
    }
    let door = match parse_decide_door(args) {
        Ok(d) => d,
        Err(usage) => {
            eprintln!("fno inbox decide: {usage}");
            return 2;
        }
    };
    decide_door_write(door, DecideIdentity::Ambient)
}

/// The gated decide write: the gate ladder of `_record` + `record_decision`,
/// in the Python order. Exit contract: 0 recorded, 1 failed write or a
/// supersession target the index cannot recover, 2 a bad flag value, 3
/// refused.
pub(crate) fn decide_door_write(door: DecideDoor, id: DecideIdentity) -> i32 {
    // Positional subject and decision are required to record.
    let (subject, decision) = match (&door.subject, &door.decision) {
        (Some(s), Some(d)) if !s.trim().is_empty() && !d.trim().is_empty() => {
            (s.trim().to_string(), d.clone())
        }
        _ => {
            eprintln!("decide: subject and decision are required to record");
            return 1;
        }
    };
    // The authority enum binds where the value is authored; the reader stays
    // permissive (rows on disk carry invented spellings).
    if let Some(a) = &door.authority {
        if !AUTHORITY_SOURCES.contains(&a.as_str()) {
            eprintln!(
                "decide: --authority '{a}' is not one of {}. Nothing was recorded. Use 'crown' \
for a king ruling inside its own scope; omit the flag to resolve it from this session.",
                AUTHORITY_SOURCES.join(", ")
            );
            return 2;
        }
    }
    let graduation = match decide_graduation_or_guidance(
        door.graduation.as_deref(),
        door.graduation_ref.as_deref(),
    ) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("backlog decide: refused. {e}. Nothing was recorded.");
            return 2;
        }
    };
    // The origin floor binds before the provenance resolution, so the event
    // records the gated value (the same order Python applies).
    let origin = enforce_origin_floor(door.origin.as_deref(), id);
    let provenance = match resolve_decider_lanes(
        id,
        door.decided_by.as_deref(),
        door.authority.as_deref(),
        origin.as_deref(),
    ) {
        Ok(p) => p,
        Err(DecideRefusal::UnknownOrigin(o)) => {
            eprintln!(
                "decide: refused. mail origin '{o}' is unknown; use one of {}",
                MAIL_ORIGINS.join(", ")
            );
            return 3;
        }
        Err(DecideRefusal::RefusedAuthority { agent, .. }) => {
            return decide_authority_refusal(&agent);
        }
        Err(DecideRefusal::UnattributedAuthority) => return decide_unattributed_refusal(),
    };
    // The evidence gate: a measured claim carries the read that produced it.
    // Exempt when the RESOLVED authority is operator (an attended terminal;
    // the waiver verb inherits this).
    let mut read_rows: Option<Vec<Value>> = None;
    if provenance.authority_source.as_deref() != Some("operator") {
        let text = format!(
            "{decision}\n{}",
            door.rationale.as_deref().unwrap_or_default()
        );
        let root = evidence_repo_root();
        let mut runner =
            |cmd: &str, root: &std::path::Path| crate::evidence::shell_run(cmd, root, 20);
        match crate::evidence::check_ruling_evidence(&text, &door.reads, &root, &mut runner) {
            Ok(rows) => read_rows = rows,
            Err(gate) => {
                eprintln!("decide: refused. {} Nothing was recorded.", gate.message);
                return 3;
            }
        }
    }
    // A waiver subject is operator-evidence-only; the check reads the
    // RESOLVED authority, never the caller's claim.
    let waiver_hit = subject == WAIVER_SUBJECT_PREFIX
        || subject.starts_with(&format!("{WAIVER_SUBJECT_PREFIX}:"));
    if waiver_hit && provenance.authority_source.as_deref() != Some("operator") {
        return decide_waiver_refusal(&subject);
    }
    // Supersession: an unknown target refuses before any write (a transposed
    // id must not read as a silent no-op), and the lane guards mirror the
    // deleted Python's two RefusedAuthority raises.
    if let Some(sup) = &door.supersedes {
        let (rows, _) = all_decision_rows().unwrap_or((Vec::new(), 0));
        let want = sup.trim().to_lowercase();
        let matched: Vec<&Value> = rows
            .iter()
            .filter(|r| {
                !is_retraction_row(r) && decision_row_str(r, "decision_id").to_lowercase() == want
            })
            .collect();
        if matched.is_empty() {
            eprintln!(
                "decide: failed to record: supersession target {sup} is not recoverable from the \
decision index. Run `fno backlog decide-reindex` before retrying."
            );
            return 1;
        }
        let newest = matched
            .into_iter()
            .max_by_key(|r| {
                (
                    decision_row_str(r, "ts").to_string(),
                    decision_row_str(r, "decision_id").to_string(),
                )
            })
            .cloned()
            .unwrap_or_default();
        let lane = decision_lane(&newest);
        let resolved = provenance.authority_source.as_deref().unwrap_or("");
        if lane == "law" && resolved != "operator" && resolved != "chat_attested" {
            return decide_authority_refusal(&provenance.decided_by);
        }
        let target_authority = decision_row_str(&newest, "authority_source");
        if resolved == "chat_attested" && target_authority == "operator" {
            return decide_authority_refusal(&provenance.decided_by);
        }
    }
    // A coord lane row (agent or crown) carries the closure key the subject
    // proves; an unproven key stays None and reads `unscoped`.
    let mut expiry_ref: Option<Value> = None;
    if matches!(
        provenance.authority_source.as_deref(),
        Some("agent") | Some("crown")
    ) {
        let row = json!({"subject": subject, "expiry_ref": Value::Null});
        let entries = crate::backlog::decisions_cli::read_graph_entries().unwrap_or_default();
        expiry_ref = crate::backlog::decisions_cli::derive_coord_expiry_ref(&row, &entries);
    }
    // Mint and envelope. A key appears only when its value is set; the text
    // caps mirror the Python builder (2000 chars).
    let decision_id = mint_decision_id();
    let ts = now_iso();
    let mut data = json!({
        "decision_id": decision_id,
        "decision": text_cap(&decision, 2000),
        "graduation": graduation,
    });
    data["subject"] = json!(subject);
    if let Some(r) = &expiry_ref {
        data["expiry_ref"] = json!(r);
    }
    if let Some(q) = door
        .question_id
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
    {
        data["question_id"] = json!(q);
    }
    if !door.options.is_empty() {
        data["options"] = json!(door.options);
    }
    data["decided_by"] = json!(provenance.decided_by);
    if let Some(a) = &provenance.attested_by {
        data["attested_by"] = json!(a);
    }
    if let Some(r) = &provenance.relayed_by {
        data["relayed_by"] = json!(r);
    }
    if let Some(o) = &origin {
        data["origin"] = json!(o);
    }
    if let Some(a) = &provenance.authority_source {
        data["authority_source"] = json!(a);
    }
    if let Some(r) = door
        .rationale
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        data["rationale"] = json!(text_cap(r, 2000));
    }
    if let Some(sup) = &door.supersedes {
        data["supersedes"] = json!(sup);
    }
    if let Some(rows) = &read_rows {
        data["reads"] = json!(rows);
    }
    let envelope = json!({"ts": ts, "type": "operator_decision", "source": "target", "data": data});
    // Durability first: the project journal. A failed write here records
    // nothing anywhere (the Python generic handler's exit 1).
    let journal = project_events_journal();
    if let Err(e) = crate::event_store::append_envelope(&journal, &envelope.to_string(), None) {
        eprintln!("decide: failed to record: {e}");
        return 1;
    }
    // Recall second: the machine-wide decision index. The event id names the
    // recovery, because re-running would mint a second id for one ruling.
    let index_path = decisions_jsonl_path();
    if let Err(e) = crate::event_store::append_envelope(&index_path, &envelope.to_string(), None) {
        eprintln!(
            "decide: recorded {decision_id} to the project journal, but the \
recall store write failed: {e}. Run `fno backlog decide-reindex` to recover it. \
Do NOT re-run decide; that records it twice."
        );
        return 1;
    }
    // The graph decisions table is the store `fno backlog decisions` reads
    // first; a refusal degrades to the durable capture, never a lost ruling.
    let graph_path = crate::graph_get::default_graph_path();
    let db_ok = crate::backlog::api::decision_record(
        &crate::backlog::api::Store::new(&graph_path),
        envelope.clone(),
    )
    .is_ok();
    let (node_id, why) = if !db_ok {
        eprintln!("decide: recorded {decision_id}, but the graph store refused the ruling.");
        (None, "the graph store refused the ruling".to_string())
    } else {
        match crate::backlog::decisions_cli::read_graph_entries() {
            Err(e) => (None, format!("the graph could not be read ({e})")),
            Ok(entries) if entries.is_empty() => (None, "the graph read back no nodes".to_string()),
            Ok(entries) => {
                match crate::backlog::decisions_cli::resolved_twice(&entries, &subject) {
                    Some(n) => (Some(n), String::new()),
                    None => (None, format!("subject '{subject}' names no graph node")),
                }
            }
        }
    };
    match &node_id {
        Some(node) => {
            eprintln!(
                "decide: recorded {decision_id} on {node}. Recover with: fno backlog decisions {node}"
            );
            render_decide_claim_receipt(node, &provenance.decided_by);
        }
        None => {
            eprintln!(
                "decide: recorded {decision_id}; no projection was written because {why} \
(the event and the index are the record). Recover with: fno backlog decisions {subject}"
            );
        }
    }
    println!("{decision_id}");
    0
}

/// The decide door's agent-lane refusal: the exact law-door-advice paragraph
/// `backlog_decide` prints for RefusedAuthorityError.
fn decide_authority_refusal(agent: &str) -> i32 {
    eprintln!(
        "backlog decide: refused. This session is agent {agent}, so it cannot record decisions HERE. \
The law door is open to it and the terms are narrow, so read them before you use it: \
`fno inbox law set <subject> <decision> --rationale <why>` (the operator types \
`/fno:law <the ruling>` for the same thing) records a chat_attested row, never an \
operator row, and it cannot supersede the operator's own law. That door is for a \
durable rule the OPERATOR asked for. It is not a way to route your own ruling around \
this refusal. Append agent findings without replacing node details with \
`fno backlog note <node> <text>`."
    );
    3
}

/// The decide door's unattributed refusal (`UnattributedAuthorityError`).
fn decide_unattributed_refusal() -> i32 {
    eprintln!(
        "decide: refused. This process has no session identity and no terminal, so nothing here \
shows the operator ruled. Operator authority is never inherited by silence. Run \
`fno inbox law set <subject> <decision> --rationale <why>` from an attended operator \
terminal, or have the operator type `/fno:law <the ruling>` in chat, which records in \
one step. Append agent findings with `fno backlog note <node> <text>`."
    );
    3
}

/// A string field off a decision row, defaulting empty (`rows_str` in the
/// decisions listing; a private twin here so the door does not reach across
/// the listing module).
fn decision_row_str<'a>(row: &'a Value, key: &str) -> &'a str {
    row.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The waiver-subject refusal (`WaiverAuthorityRefusedError`): the only path
/// is the attended coverage-waive command.
fn decide_waiver_refusal(subject: &str) -> i32 {
    eprintln!(
        "decide: refused. '{subject}' is a review-coverage waiver subject: waiver evidence needs \
superuser authority, and a chat-attested row proves only that a session was addressed, \
not that a person reviewed anything. Waivers are recorded by the attended command \
`fno do pr coverage-waive <pr> --reason \"...\"` at an operator terminal; a session a \
harness identifies records nothing there."
    );
    3
}

/// The full provenance resolver (`_resolve_decider`): three states and the
/// third fails closed. `decided_by` is STAMPED, never stated, whenever a
/// session identity resolves; a caller-supplied name rides `relayed_by`.
pub(crate) fn resolve_decider_lanes(
    id: DecideIdentity,
    decided_by: Option<&str>,
    authority_source: Option<&str>,
    origin: Option<&str>,
) -> Result<DecideProvenance, DecideRefusal> {
    if let Some(o) = origin {
        if !MAIL_ORIGINS.contains(&o) {
            return Err(DecideRefusal::UnknownOrigin(o.to_string()));
        }
    }
    let agent = id.agent();
    if let Some(o) = origin {
        if authority_source == Some("operator") && max_authority_by_origin(o) != "operator" {
            return Err(DecideRefusal::RefusedAuthority {
                agent: agent
                    .clone()
                    .unwrap_or_else(|| "unattributed-caller".to_string()),
                origin: Some(o.to_string()),
            });
        }
    }
    if agent.is_some() && authority_source == Some("operator") {
        return Err(DecideRefusal::RefusedAuthority {
            agent: agent.clone().unwrap(),
            origin: None,
        });
    }
    if let Some(handle) = &agent {
        // State 1. Relayed only when it says something the stamp does not.
        if origin == Some("operator") {
            return Ok(DecideProvenance {
                decided_by: handle.clone(),
                authority_source: Some(authority_source.unwrap_or("agent").to_string()),
                attested_by: None,
                relayed_by: Some(handle.clone()),
            });
        }
        let relayed = decided_by
            .map(str::trim)
            .filter(|d| !d.is_empty() && *d != handle.as_str())
            .map(str::to_string);
        return Ok(DecideProvenance {
            decided_by: handle.clone(),
            authority_source: Some(authority_source.unwrap_or("agent").to_string()),
            attested_by: None,
            relayed_by: relayed,
        });
    }
    if attended_terminal() {
        // State 2. A person is at a terminal; the stated name IS the record.
        // Authority is never defaulted to operator here.
        let decider = decided_by
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .unwrap_or("operator");
        let attested = origin.filter(|o| !o.is_empty()).unwrap_or(decider);
        return Ok(DecideProvenance {
            decided_by: decider.to_string(),
            authority_source: authority_source.map(str::to_string),
            attested_by: Some(attested.to_string()),
            relayed_by: None,
        });
    }
    // State 3, fail closed.
    if authority_source == Some("operator") {
        return Err(DecideRefusal::UnattributedAuthority);
    }
    Ok(DecideProvenance {
        decided_by: "unattributed-caller".to_string(),
        authority_source: authority_source.map(str::to_string),
        attested_by: None,
        relayed_by: decided_by
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::law_match::scope_tests::DoorEnv;

    // -------------------------------------------------------------------
    // The decide door: injected-identity unit tests (the parity goldens
    // pin the identity-independent CLI answers; these pin the lanes).
    // -------------------------------------------------------------------

    /// A parsed decide argv.
    fn decide_argv(parts: &[&str]) -> DecideDoor {
        parse_decide_door(&parts.iter().map(|s| s.to_string()).collect::<Vec<_>>()).expect("parses")
    }

    /// Save one node into the env's graph so a subject can resolve.
    fn save_node(graph: &std::path::Path, id: &str, slug: &str) {
        let mut connection = crate::backlog::open(graph).expect("opens");
        let node = crate::backlog::model::Node::from_json(&serde_json::json!({
            "id": id,
            "slug": slug,
            "title": "Node",
            "type": "feature",
            "status": "ready",
            "priority": "p2",
        }))
        .expect("node json");
        crate::backlog::nodes::save(&mut connection, &node).expect("saves");
    }

    #[test]
    fn decide_agent_lane_stamps_the_handle_and_floors_the_origin() {
        let env = DoorEnv::new();
        crate::paths::pin_test_claims_root(env.0.path());
        let code = decide_door_write(
            decide_argv(&[
                "x-n1",
                "Ship it",
                "--authority",
                "agent",
                "--origin",
                "operator",
            ]),
            DecideIdentity::Forced(Some("cl-test0")),
        );
        assert_eq!(code, 0);
        let (rows, _) = all_decision_rows().expect("reads");
        let row = rows.last().expect("a row landed");
        assert_eq!(row["decided_by"], json!("cl-test0"));
        assert_eq!(row["authority_source"], json!("agent"));
        // An ambient agent cannot claim the operator channel; the floor
        // caps it to peer before the event records it.
        assert_eq!(row["origin"], json!("peer"));
        assert_eq!(row["relayed_by"], Value::Null);
    }

    #[test]
    fn decide_agent_supersede_of_a_law_row_refuses_and_records_nothing() {
        let env = DoorEnv::new();
        crate::paths::pin_test_claims_root(env.0.path());
        // Seed an operator law row through the same stores the door writes.
        let envelope = serde_json::json!({
            "ts": now_iso(),
            "type": "operator_decision",
            "source": "target",
            "data": {
                "decision_id": "d-0aa0aa0a",
                "decision": "The operator owns durable policy.",
                "subject": "file-budget",
                "authority_source": "operator",
                "graduation": {"kind": "guidance"},
            },
        });
        let line = envelope.to_string();
        crate::event_store::append_envelope(&decisions_jsonl_path(), &line, None).expect("index");
        crate::backlog::api::decision_record(
            &crate::backlog::api::Store::new(&crate::graph_get::default_graph_path()),
            envelope,
        )
        .expect("db");
        let before = all_decision_rows().expect("reads").0.len();
        let code = decide_door_write(
            decide_argv(&["x-n1", "Override", "--supersedes", "d-0aa0aa0a"]),
            DecideIdentity::Forced(Some("cl-test1")),
        );
        assert_eq!(code, 3);
        let after = all_decision_rows().expect("reads").0.len();
        assert_eq!(after, before, "nothing recorded on a refused write");
    }

    #[test]
    fn decide_unattributed_operator_refusal_records_nothing() {
        let env = DoorEnv::new();
        crate::paths::pin_test_claims_root(env.0.path());
        let code = decide_door_write(
            decide_argv(&["x-n1", "Law", "--authority", "operator"]),
            DecideIdentity::Forced(None),
        );
        assert_eq!(code, 3);
        let (rows, _) = all_decision_rows().unwrap_or((Vec::new(), 0));
        assert!(rows.is_empty(), "nothing recorded");
    }

    #[test]
    fn decide_unattributed_agent_lane_records_under_the_honest_handle() {
        let env = DoorEnv::new();
        crate::paths::pin_test_claims_root(env.0.path());
        let code = decide_door_write(
            decide_argv(&["x-n1", "Coord note", "--authority", "agent"]),
            DecideIdentity::Forced(None),
        );
        assert_eq!(code, 0);
        let (rows, _) = all_decision_rows().expect("reads");
        let row = rows.last().expect("a row landed");
        assert_eq!(row["decided_by"], json!("unattributed-caller"));
        assert_eq!(row["authority_source"], json!("agent"));
        assert_eq!(row["lifecycle"], Value::Null);
    }

    #[test]
    fn decide_bad_authority_and_graduation_refuse_before_any_write() {
        let env = DoorEnv::new();
        crate::paths::pin_test_claims_root(env.0.path());
        assert_eq!(
            decide_door_write(
                decide_argv(&["x-n1", "do it", "--authority", "king"]),
                DecideIdentity::Forced(None),
            ),
            2
        );
        assert_eq!(
            decide_door_write(
                decide_argv(&["x-n1", "do it", "--graduation", "banana"]),
                DecideIdentity::Forced(None),
            ),
            2
        );
        assert!(all_decision_rows().is_err(), "no store was ever created");
    }

    #[test]
    fn decide_missing_subject_or_decision_refuses() {
        let env = DoorEnv::new();
        crate::paths::pin_test_claims_root(env.0.path());
        assert_eq!(
            decide_door_write(decide_argv(&["x-n1"]), DecideIdentity::Forced(None)),
            1
        );
    }

    #[test]
    fn decide_coord_lane_stamps_the_node_closure_key() {
        let env = DoorEnv::new();
        crate::paths::pin_test_claims_root(env.0.path());
        save_node(
            &crate::graph_get::default_graph_path(),
            "x-node1",
            "node-one",
        );
        let code = decide_door_write(
            decide_argv(&["x-node1", "Ship it", "--authority", "agent"]),
            DecideIdentity::Forced(Some("cl-test2")),
        );
        assert_eq!(code, 0);
        let (rows, _) = all_decision_rows().expect("reads");
        let row = rows.last().expect("a row landed");
        assert_eq!(row["expiry_ref"]["kind"], json!("node"));
        assert_eq!(row["expiry_ref"]["node_id"], json!("x-node1"));
    }

    #[test]
    fn decide_unknown_supersession_target_refuses_with_exit_one() {
        let env = DoorEnv::new();
        crate::paths::pin_test_claims_root(env.0.path());
        let code = decide_door_write(
            decide_argv(&["x-n1", "Override", "--supersedes", "d-ffffffff"]),
            DecideIdentity::Forced(None),
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn decide_waiver_subject_refuses_a_non_operator() {
        let env = DoorEnv::new();
        crate::paths::pin_test_claims_root(env.0.path());
        let code = decide_door_write(
            decide_argv(&["review-coverage-waiver", "waive", "--authority", "agent"]),
            DecideIdentity::Forced(Some("cl-test3")),
        );
        assert_eq!(code, 3);
        let (rows, _) = all_decision_rows().unwrap_or((Vec::new(), 0));
        assert!(rows.is_empty(), "nothing recorded");
    }

    #[test]
    fn decide_relayed_name_rides_relayed_by_not_decided_by() {
        let env = DoorEnv::new();
        crate::paths::pin_test_claims_root(env.0.path());
        let code = decide_door_write(
            decide_argv(&["x-n1", "Relayed", "--decided-by", "J.N. Choi"]),
            DecideIdentity::Forced(Some("cl-test4")),
        );
        assert_eq!(code, 0);
        let (rows, _) = all_decision_rows().expect("reads");
        let row = rows.last().expect("a row landed");
        assert_eq!(row["decided_by"], json!("cl-test4"));
        assert_eq!(row["relayed_by"], json!("J.N. Choi"));
    }
}
