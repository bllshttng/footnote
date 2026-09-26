//! The reply ladder: did an answered attention item reach its asker?
//!
//! The mux records one durable `attention_answer` row and returns; this
//! ladder is the delivery half. One beat step per item, a pure
//! [`step`] decision over stored per-item state, and every effect (the
//! clear, the resume, the crown mail) behind an injected runner, so tests
//! stage the world. Rungs: the clear's mail leg, a resume of an idle asker
//! confirmed by content in its transcript, a wrapped mail to the crown over
//! the asker's node, and `none` when no crown covers it.

use crate::attention::AttentionItem;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

/// How long the ladder waits for the resume to show up in the asker's
/// transcript before it escalates to the crown.
pub const CONFIRM_WINDOW_S: u64 = 300;

/// The bound for one resume, mail or crown mail shellout.
const RUN_TIMEOUT_S: u64 = 30;

/// Per-item ladder state, one JSON file beside the settle state.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ReplyState {
    pub item_id: String,
    pub answer: String,
    #[serde(default)]
    pub sink: String,
    /// How many times the clear failed here.
    #[serde(default)]
    pub clear_retries: u32,
    /// The clear ran (or needed no clear: a note, or an item closed
    /// elsewhere).
    #[serde(default)]
    pub cleared: bool,
    /// The clear's posture line (the last stderr line it printed).
    #[serde(default)]
    pub receipt: String,
    /// The note path's one explicit answer mail went out.
    #[serde(default)]
    pub note_mail_sent: bool,
    /// When the resume ran, and the transcript byte offset saved before it.
    #[serde(default)]
    pub resumed_at: Option<u64>,
    #[serde(default)]
    pub offset: Option<u64>,
    /// The resume's exit code, when it ran.
    #[serde(default)]
    pub resume_exit: Option<i32>,
    #[serde(default)]
    pub rung: String,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub holder: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub updated_at: u64,
}

impl ReplyState {
    /// A terminal state stops the ladder: one delivery row per item.
    fn terminal(&self) -> bool {
        !self.outcome.is_empty()
    }
}

/// One delivery outcome the ladder commits: the `attention_delivery` row.
pub struct Delivery {
    pub rung: &'static str,
    pub outcome: &'static str,
    pub evidence: String,
    pub holder: Option<String>,
    pub session_id: Option<String>,
}

/// The next thing the ladder wants. Every effect stays with the caller.
pub enum Step {
    /// Terminal: commit the row.
    Deliver(Delivery),
    /// Resume the asker with the answer.
    Resume,
    /// A note item has no clear, so the mail rung is this ladder's own send.
    MailNote,
    /// Escalate: resolve the crown over the asker's node, mail it wrapped.
    CrownNeeded,
    /// No crown covered the node: undelivered.
    Fail { evidence: String },
    /// Nothing to do this beat.
    Idle,
}

/// The facts one step decides over. Everything derived from the world this
/// beat: the asker's session facts, whether the transcript check confirmed
/// the answer landed, and whether the clear's mail leg already delivered.
pub struct Facts<'a> {
    pub mail_landed: bool,
    pub is_note: bool,
    pub session: Option<(&'a str, &'a str)>,
    pub confirmed: bool,
    pub now: u64,
}

/// The pure decision. Total over all inputs; never shells out.
pub fn step(state: &ReplyState, facts: &Facts) -> Step {
    if facts.mail_landed {
        return Step::Deliver(Delivery {
            rung: "mail",
            outcome: "landed",
            evidence: state.receipt.clone(),
            holder: None,
            session_id: facts.session.map(|(sid, _)| sid.to_string()),
        });
    }
    if facts.is_note && !state.note_mail_sent {
        return Step::MailNote;
    }
    if state.resumed_at.is_none() {
        return match facts.session {
            Some((sid, _)) if !sid.is_empty() => Step::Resume,
            _ => Step::CrownNeeded,
        };
    }
    // Resume phase.
    if state.resume_exit != Some(0) {
        return Step::CrownNeeded;
    }
    if facts.confirmed {
        return Step::Deliver(Delivery {
            rung: "resume",
            outcome: "confirmed",
            evidence: format!("found {} in the asker's transcript", state.item_id),
            holder: None,
            session_id: facts.session.map(|(sid, _)| sid.to_string()),
        });
    }
    if state
        .resumed_at
        .is_some_and(|at| facts.now.saturating_sub(at) > CONFIRM_WINDOW_S)
    {
        return Step::CrownNeeded;
    }
    Step::Idle
}

/// The arm's mux pass: every unsuperseded `attention_answer` row, whatever
/// its sink, drives one ladder. The clear runs while the item is still open;
/// the ladder advances across beats from the persisted state. Returns the
/// acted count and the detail lines the tick row carries.
pub fn tick_answers(
    items: &[AttentionItem],
    cwd: &Path,
    state_dir: &Path,
    deadline: std::time::Instant,
    io: &mut dyn crate::attention_arm::SinkIo,
    runner: &dyn Fn(&[String]) -> (i32, String, String),
) -> (u64, Vec<String>) {
    let now = now_secs();
    let (answers, delivered) = fold_answer_rows(items);
    // A delivery row is the ladder's durable terminal marker: answers that
    // already carry one never re-enter the ladder, and their terminal states
    // are pruned so replies.json stays bounded.
    let mut states: HashMap<String, ReplyState> = load_states(state_dir);
    states.retain(|id, _| !delivered.contains(id));
    let mut acted: u64 = 0;
    let mut detail: Vec<String> = Vec::new();
    for (item_id, sink, answer) in &answers {
        if std::time::Instant::now() >= deadline {
            detail.push(format!("ladder: budget spent, {} deferred", answers.len()));
            break;
        }
        let state = states.entry(item_id.clone()).or_insert_with(|| ReplyState {
            item_id: item_id.clone(),
            answer: answer.clone(),
            sink: sink.clone(),
            ..Default::default()
        });
        if state.terminal() {
            continue;
        }
        // The clear phase: while the item is open, the arm owns the clear
        // (same retry cap as the file lane). A closed item, a withdrawn one
        // or a note needs no clear - the ladder starts from the answer row.
        if !state.cleared {
            let item = items.iter().find(|i| i.id == *item_id);
            let open = item.is_some_and(|i| i.state == "open");
            if open && !item_id.starts_with("note-") {
                match io.clear(item_id, answer) {
                    Ok(posture) => {
                        state.cleared = true;
                        state.receipt = posture;
                        acted += 1;
                    }
                    Err(e) => {
                        state.clear_retries += 1;
                        detail.push(format!("ladder {item_id}: clear failed: {e}"));
                        if state.clear_retries >= crate::attention_arm::CLEAR_RETRY_CAP {
                            state.rung = "none".into();
                            state.outcome = "failed".into();
                            state.evidence = format!("the clear kept failing: {e}");
                        }
                    }
                }
            } else if item_id.starts_with("note-") {
                // A note: no door closes it, so the note mail rung follows.
                state.cleared = true;
            } else {
                // Closed by another lane (the endpoint, a terminal, or the
                // file lane): that clear's own mail leg addressed the asker,
                // so the ladder ends here instead of waking anyone twice.
                state.cleared = true;
                state.rung = "mail".into();
                state.outcome = "landed".into();
                state.evidence =
                    "question closed by another lane; its clear mailed the asker".into();
                acted += 1;
                emit_delivery(state);
            }
        }
        // The ladder phase: effects through the injected runner, one delivery
        // row per item.
        let step = step_of(state, items, now);
        match step {
            Step::Idle => {}
            Step::Fail { evidence } => {
                state.rung = "none".into();
                state.outcome = "failed".into();
                state.evidence = evidence;
                acted += 1;
                emit_delivery(state);
            }
            Step::Deliver(d) => {
                state.rung = d.rung.into();
                state.outcome = d.outcome.into();
                state.evidence = d.evidence.clone();
                state.holder = d.holder.clone();
                state.session_id = d.session_id.clone();
                acted += 1;
                emit_delivery(state);
            }
            Step::Resume => {
                let item = items.iter().find(|i| i.id == *item_id);
                if let Some(item) = item {
                    run_resume(state, item, now, runner);
                }
            }
            Step::MailNote => {
                let asker = items
                    .iter()
                    .find(|i| i.id == *item_id)
                    .and_then(|i| i.asker.as_ref().map(|a| a.handle.clone()));
                mail_note(state, asker, runner);
            }
            Step::CrownNeeded => {
                let item = items.iter().find(|i| i.id == *item_id);
                match item.and_then(|i| crown_holder(i, cwd)) {
                    Some(holder) => {
                        mail_crown(state, &holder, runner);
                    }
                    None => {
                        state.rung = "none".into();
                        state.outcome = "failed".into();
                        state.evidence = format!(
                            "no live crown over {item_id} and the mail did not confirm landing"
                        );
                        emit_delivery(state);
                    }
                }
            }
        }
    }
    save_states(state_dir, &states);
    (acted, detail)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build the facts one step decides over: the asker's session, the clear's
/// mail verdict, and (resume phase) the transcript confirm. `confirmed` is
/// only computed in the resume phase - a transcript scan per beat per item
/// is the one cost the ladder refuses to pay before it must.
fn step_of(state: &ReplyState, items: &[AttentionItem], now: u64) -> Step {
    let item = items.iter().find(|i| i.id == state.item_id);
    let session: Option<(String, String)> = item.and_then(|i| {
        i.asker.as_ref().and_then(|a| {
            a.session_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|sid| (sid.to_string(), a.harness.clone().unwrap_or_default()))
        })
    });
    let confirmed = if state.resumed_at.is_some() && state.resume_exit == Some(0) {
        session.as_ref().is_some_and(|(sid, harness)| {
            transcript_confirm(sid, harness, state.item_id.as_str(), state.offset)
        })
    } else {
        // No session facts (or a failed resume) to confirm against: not
        // confirmed this beat.
        false
    };
    let facts = Facts {
        mail_landed: state.receipt.contains("delivered (hosted)"),
        is_note: state.item_id.starts_with("note-"),
        session: session.as_ref().map(|(s, h)| (s.as_str(), h.as_str())),
        confirmed,
        now,
    };
    step(state, &facts)
}

/// The resume effect: run the door once, save the offset and the exit code.
/// The asker facts come from this beat's projection item; the session id is
/// remembered in state so a later beat can confirm the transcript.
fn run_resume(
    state: &mut ReplyState,
    item: &AttentionItem,
    now: u64,
    runner: &dyn Fn(&[String]) -> (i32, String, String),
) {
    let Some(sid) = item
        .asker
        .as_ref()
        .and_then(|a| a.session_id.clone())
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    state.session_id = Some(sid.clone());
    let harness = item
        .asker
        .as_ref()
        .and_then(|a| a.harness.clone())
        .unwrap_or_default();
    let argv = vec![
        "fno".into(),
        "agents".into(),
        "resume".into(),
        sid.clone(),
        "--message".into(),
        resume_message(state),
    ];
    let (code, _stdout, _stderr) = runner(&argv);
    if let Some(path) = transcript_for(&sid, &harness) {
        state.offset = Some(transcript_len(&path));
    }
    state.resumed_at = Some(now);
    state.resume_exit = Some(code);
}

/// The resume message: the answer plus the question id, so the asker can
/// dedupe a double delivery and the ladder can confirm the landing.
fn resume_message(state: &ReplyState) -> String {
    format!(
        "Answer to your question {}: {}. If you already have this answer, ignore this copy.",
        state.item_id, state.answer
    )
}

/// The note path's one explicit answer mail (a note has no clear, so no mail
/// leg runs for it). Sent once; the receipt judged like any other mail rung.
fn mail_note(
    state: &mut ReplyState,
    asker: Option<String>,
    runner: &dyn Fn(&[String]) -> (i32, String, String),
) {
    let Some(asker) = asker else {
        return;
    };
    let argv = vec![
        "fno".into(),
        "agents".into(),
        "mail".into(),
        "send".into(),
        asker.clone(),
        resume_message(state),
        "--style-exception".into(),
        "operator answer verbatim: quoted decision text, not authored prose".into(),
    ];
    let (code, stdout, _stderr) = runner(&argv);
    state.note_mail_sent = true;
    if code == 0 && crate::mail_inject::mail_send_receipt(&stdout).contains("delivered (hosted)") {
        state.rung = "mail".into();
        state.outcome = "landed".into();
        state.evidence = crate::mail_inject::mail_send_receipt(&stdout).to_string();
        state.session_id = Some(asker);
        emit_delivery(state);
    }
}

/// The crown effect: one wrapped mail to the crown holder over the asker's
/// node; the row records `crown/sent` on exit 0, `none/failed` otherwise.
fn mail_crown(
    state: &mut ReplyState,
    holder: &str,
    runner: &dyn Fn(&[String]) -> (i32, String, String),
) {
    let argv = vec![
        "fno".into(),
        "agents".into(),
        "mail".into(),
        "send".into(),
        holder.to_string(),
        resume_message(state),
        "--style-exception".into(),
        "operator answer verbatim: quoted decision text, not authored prose".into(),
    ];
    let (code, _stdout, _stderr) = runner(&argv);
    state.session_id = state.session_id.clone().or(None);
    if code == 0 {
        state.rung = "crown".into();
        state.outcome = "sent".into();
        state.evidence = format!("mailed the crown holder {holder}");
        state.holder = Some(holder.to_string());
    } else {
        state.rung = "none".into();
        state.outcome = "failed".into();
        state.evidence = format!("the crown mail to {holder} failed");
    }
    emit_delivery(state);
}

/// The crown resolver: the live crown whose compiled territory names the
/// asker's node (or its first block). Reuses the one resolver the drain
/// and the court read: territory's node_owners over live_crowns.
fn crown_holder(item: &AttentionItem, cwd: &Path) -> Option<String> {
    // A literal `none` node is the projection's "no node"; fall through to
    // the blocks, which still name what the answer unblocks.
    let node = item
        .node
        .as_deref()
        .filter(|n| !n.is_empty() && *n != "none")
        .or_else(|| item.blocks.first().map(String::as_str));
    let crowns =
        crate::territory::live_crowns(&crate::paths::AgentsHome::from_env().registry_json())
            .ok()?;
    let entries = crate::territory::graph_entries(cwd).ok()?;
    let projects = Ok(crate::territory::workspace_paths(cwd));
    let (owners, _failures) = crate::territory::node_owners(&crowns, &entries, &projects);
    node.and_then(|n| owners.get(n))
        .and_then(|scope| crowns.iter().find(|c| c.scope == *scope))
        .map(|c| c.holder.clone())
}

/// The asker's transcript, by harness: claude resolves by session uuid, codex
/// by thread id over the rollout store. Other harnesses have no transcript
/// finder today, so their answers go to the crown (the plan's rung b table).
fn transcript_for(sid: &str, harness: &str) -> Option<PathBuf> {
    match harness {
        "claude" => crate::claude_drive::find_transcript(sid),
        "codex" => codex_rollout(sid),
        _ => None,
    }
}

/// Whether the question id shows up in the asker's transcript after the byte
/// offset the resume saved: the content-confirm the mail lane lacks.
fn transcript_confirm(sid: &str, harness: &str, marker: &str, offset: Option<u64>) -> bool {
    let Some(path) = transcript_for(sid, harness) else {
        return false;
    };
    crate::mail_inject::confirm_content_after(&path, marker, offset.unwrap_or(0)).unwrap_or(false)
}

/// The transcript's byte length, read once: the resume's before-offset.
fn transcript_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Fold the unsuperseded `attention_answer` rows: id -> (sink, answer text).
/// The answer text maps the row's option onto the item's option text when the
/// projection has the item; words and done stand alone.
fn fold_answer_rows(
    items: &[AttentionItem],
) -> (Vec<(String, String, String)>, std::collections::HashSet<String>) {
    let home = crate::paths::AgentsHome::from_env();
    let path = crate::provider_cap::questions_path(&home);
    let mut out: Vec<(String, String, String)> = Vec::new();
    let mut delivered: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut won: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in crate::event_store::journal_text(&path, &[]).lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("attention_answer") => {}
            Some("attention_delivery") => {
                let Some(did) = v.get("data").and_then(|d| d.get("item_id")).and_then(Value::as_str)
                else {
                    continue;
                };
                delivered.insert(did.to_string());
                continue;
            }
            _ => continue,
        }
        let data = v.get("data").cloned().unwrap_or(Value::Null);
        let Some(id) = data.get("item_id").and_then(Value::as_str) else {
            continue;
        };
        let superseded = data.get("superseded").and_then(Value::as_bool) == Some(true);
        if superseded || won.contains(id) {
            continue;
        }
        won.insert(id.to_string());
        let sink = data
            .get("sink")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let field = |k: &str| {
            data.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let text = match (
            data.get("option").and_then(Value::as_u64),
            field("words"),
            data.get("done").and_then(Value::as_bool),
        ) {
            (Some(n), _, _) => items
                .iter()
                .find(|i| i.id == id)
                .and_then(|i| i.options.iter().find(|o| o.n as u64 == n))
                .map(|o| o.text.clone())
                .unwrap_or_else(|| format!("option {n}")),
            (_, w, _) if !w.is_empty() => w,
            (_, _, Some(true)) => "done".to_string(),
            _ => String::new(),
        };
        out.push((id.to_string(), sink, text));
    }
    (out, delivered)
}

/// Load the ladder states from `replies.json` beside the settle state.
fn load_states(state_dir: &Path) -> HashMap<String, ReplyState> {
    let path = state_dir.join("replies.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Save the ladder states atomically (tmp + rename).
fn save_states(state_dir: &Path, states: &HashMap<String, ReplyState>) {
    let path = state_dir.join("replies.json");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if serde_json::to_string(states)
        .map(|s| std::fs::write(&tmp, s))
        .unwrap_or(Err(std::io::Error::other("serialize")))
        .is_ok()
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Commit one terminal ladder: the `attention_delivery` row in
/// `questions.jsonl`, beside the answer row it delivers.
fn emit_delivery(state: &ReplyState) {
    let answered_at = chrono::Utc::now().to_rfc3339();
    let row = serde_json::json!({
        "ts": answered_at,
        "type": "attention_delivery",
        "source": "daemon",
        "data": {
            "item_id": state.item_id,
            "rung": state.rung,
            "outcome": state.outcome,
            "evidence": state.evidence,
            "session_id": state.session_id,
            "holder": state.holder,
            "at": answered_at,
        }
    });
    let home = crate::paths::AgentsHome::from_env();
    let path = crate::provider_cap::questions_path(&home);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{row}");
    }
}

/// The bounded `fno` shellout the effects run through: stdin closed, 30 s
/// bound, the (code, stdout, stderr) triple the ladder reads.
pub fn real_runner_pub(argv: &[String]) -> (i32, String, String) {
    let mut cmd = crate::loop_dispatch::fno_cmd(&argv[0]);
    cmd.args(&argv[1..]).stdin(std::process::Stdio::null());
    match crate::bounded_cmd::output_with_timeout_result(cmd, RUN_TIMEOUT_S) {
        Ok(out) => (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        ),
        Err(e) => (-1, String::new(), e.to_string()),
    }
}

/// The codex thread's rollout transcript, over the store the gc inventory
/// walks. Other harnesses have no finder today.
fn codex_rollout(thread_id: &str) -> Option<PathBuf> {
    let sessions = crate::gc_inventory::codex_store_sessions().ok()?;
    sessions
        .get(&thread_id.to_ascii_lowercase())
        .and_then(|paths| paths.first().cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::AttentionItem;

    /// Four staged worlds, driven through tick_answers with a fake SinkIo.
    struct FakeIo {
        posture: String,
        clears: usize,
    }

    impl crate::attention_arm::SinkIo for FakeIo {
        fn read(&mut self, _p: &Path) -> std::io::Result<String> {
            Ok(String::new())
        }
        fn write_atomic(&mut self, _p: &Path, _c: &str) -> std::io::Result<()> {
            Ok(())
        }
        fn create_new(&mut self, _p: &Path, _c: &str) -> std::io::Result<bool> {
            Ok(false)
        }
        fn rename(&mut self, _a: &Path, _b: &Path) -> std::io::Result<()> {
            Ok(())
        }
        fn list_md(&mut self, _d: &Path) -> Vec<PathBuf> {
            vec![]
        }
        fn path_exists(&mut self, _p: &Path) -> bool {
            false
        }
        fn route(&mut self, _i: &AttentionItem) -> Result<crate::attention_route::Routing, String> {
            Ok(Default::default())
        }
        fn record(
            &mut self,
            _i: &AttentionItem,
            _s: &str,
            _a: &crate::attention_file::FileAnswer,
        ) -> Result<String, String> {
            Ok(String::new())
        }
        fn clear(&mut self, _id: &str, _answer: &str) -> Result<String, String> {
            self.clears += 1;
            Ok(self.posture.clone())
        }
        fn notify(&mut self, _t: &str, _b: &str) {}
    }

    fn ready_item(id: &str, harness: Option<&str>, sid: Option<&str>) -> AttentionItem {
        let mut item = AttentionItem {
            id: id.into(),
            kind: "question".into(),
            title: "Which?".into(),
            body: None,
            project: String::new(),
            priority: "high".into(),
            created_at: String::new(),
            deadline: None,
            on_silence: None,
            class: None,
            blocks: vec!["x-1".into()],
            subject: None,
            asker: Some(crate::attention::Asker {
                handle: "w1".into(),
                session_id: sid.map(String::from),
                harness: harness.map(String::from),
                rank: None,
                live: None,
                reach: None,
            }),
            node: Some("x-1".into()),
            blocked_because: None,
            options_rationale: None,
            recommendation: None,
            options: vec![],
            unknowns: None,
            reversible: None,
            cost_if_wrong: None,
            meanwhile: None,
            ready: true,
            missing: vec![],
            state: "open".into(),
        };
        item
    }

    /// Append one answer row to the declared questions.jsonl.
    fn record_answer(item_id: &str) {
        crate::attention_arm::append_answer_row(
            item_id,
            "mux",
            &crate::attention_file::FileAnswer::Option(1),
            "sink",
            "mux",
        )
        .unwrap();
    }

    /// The declared root's questions.jsonl, read back.
    fn journal() -> String {
        let home = crate::paths::AgentsHome::from_env();
        crate::event_store::journal_text(&crate::provider_cap::questions_path(&home), &[])
    }

    #[test]
    fn ac2_hp_a_hosted_mail_receipt_ends_the_ladder_at_the_mail_rung() {
        let _root = crate::paths::DeclaredRoot::declare("reply_hp_mail_lande");
        let items = vec![ready_item("q-hp", None, Some("s1"))];
        record_answer("q-hp");
        let mut io = FakeIo {
            posture: "outstanding: q-hp answered; mail to w1: delivered (hosted)".into(),
            clears: 0,
        };
        let state_dir = tempfile::tempdir().unwrap();
        let (acted, _detail) = tick_answers(
            &items,
            Path::new("."),
            state_dir.path(),
            std::time::Instant::now() + std::time::Duration::from_secs(60),
            &mut io,
            &|_argv: &[String]| (0, String::new(), String::new()),
        );
        assert_eq!(acted, 2, "clear + delivery: {acted} journal={}", journal());
        assert_eq!(io.clears, 1, "one clear");
        let j = journal();
        assert!(j.contains("attention_delivery"), "{j}");
        assert!(j.contains("\"rung\":\"mail\""), "{j}");
        assert!(j.contains("\"outcome\":\"landed\""), "{j}");
        assert!(j.contains("\"sink\":\"mux\""), "the answer row: {j}");
    }

    #[test]
    fn ac2_edge_a_durable_receipt_resumes_and_confirms_by_transcript() {
        let _root = crate::paths::DeclaredRoot::declare("reply_edge_resumes_");
        let base = std::env::temp_dir().join(format!("reply-transcripts-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let sid = "01234567-89ab-cdef-0123-456789abcdef";
        let proj = base.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join(format!("{sid}.jsonl")),
            "{\"text\":\"unrelated turn before the resume\"}\n",
        )
        .unwrap();
        std::env::set_var(crate::claude_drive::PROJECTS_DIR_ENV, &base);
        let items = vec![ready_item("q-edge", Some("claude"), Some(sid))];
        record_answer("q-edge");
        let mut io = FakeIo {
            posture: "outstanding: q-edge answered; mail to w1: queued (durable)".into(),
            clears: 0,
        };
        let resumed = std::sync::atomic::AtomicUsize::new(0);
        let resumed_ref = &resumed;
        let staged = move |argv: &[String]| -> (i32, String, String) {
            if argv.get(2).map(String::as_str) == Some("resume") {
                resumed_ref.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            (0, String::new(), String::new())
        };
        let _ = &staged;
        let state_dir = tempfile::tempdir().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let (acted1, _d1) = tick_answers(
            &items,
            Path::new("."),
            state_dir.path(),
            deadline,
            &mut io,
            &staged,
        );
        assert_eq!(
            resumed.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one resume"
        );
        assert_eq!(acted1, 1, "the clear acted; the resume is pending");
        // The second beat: the resumed session lands its turn, appending the
        // answer (with the question id) after the offset the resume saved.
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(proj.join(format!("{sid}.jsonl")))
                .unwrap();
            f.write_all(b"{\"text\":\"Answer to your question q-edge: yes\"}\n")
                .unwrap();
        }
        let (acted2, _d2) = tick_answers(
            &items,
            Path::new("."),
            state_dir.path(),
            deadline,
            &mut io,
            &staged,
        );
        assert_eq!(
            resumed.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no second resume"
        );
        assert_eq!(acted2, 1, "the confirm lands the delivery row");
        let j = journal();
        assert!(j.contains("\"rung\":\"resume\""), "{j}");
        assert!(j.contains("\"outcome\":\"confirmed\""), "{j}");
    }

    #[test]
    fn ac2_err_a_reassigned_resume_with_no_crown_reads_undelivered() {
        let _root = crate::paths::DeclaredRoot::declare("reply_err_no_crown");
        let items = vec![ready_item("q-err", Some("claude"), Some("s-err"))];
        record_answer("q-err");
        let mut io = FakeIo {
            posture: "outstanding: q-err answered; mail to w1: queued (durable)".into(),
            clears: 0,
        };
        let staged =
            |_argv: &[String]| -> (i32, String, String) { (17, String::new(), String::new()) };
        let state_dir = tempfile::tempdir().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let (_a1, _d1) = tick_answers(
            &items,
            Path::new("."),
            state_dir.path(),
            deadline,
            &mut io,
            &staged,
        );
        let (_a2, _d2) = tick_answers(
            &items,
            Path::new("."),
            state_dir.path(),
            deadline,
            &mut io,
            &staged,
        );
        let j = journal();
        assert!(j.contains("\"rung\":\"none\""), "{j}");
        assert!(j.contains("\"outcome\":\"failed\""), "{j}");
    }
}
