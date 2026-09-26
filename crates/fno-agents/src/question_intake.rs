//! The Rust question intake behind `fno inbox outstanding ask`.
//!
//! Transport-only `question-intake` arm: one JSON request on stdin (what the
//! Python shim resolved: text, flags, identity, live law rows, storage root),
//! one JSON answer on stdout, and the exit code carries the verdict. It
//! registers no client verb - the shrink law (d-fe66560a) allows no new
//! action - and callers reach it through `verb_call`, like the patch door.
//!
//! Owns: the law refusal (`law_match`), the context parse
//! (`escalation::parse` with the question-file sections), the node-pointer
//! rule (one line plus a node pointer, law d-59af3235), the dedup on
//! subject plus node, the 2000-character cut, the id mint, the dual write
//! (project journal fatal, index best-effort) and the render-position
//! receipt. The exit CODE lives in the answer (the transport always exits 0
//! once the request parsed, the question-intake shape); refusal MESSAGES stay
//! in the shim, which owns the user's name.

use crate::paths::AgentsHome;
use crate::provider_cap::questions_path;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

/// `cli/src/fno/events/__init__.py: QUESTION_CAP`.
pub const QUESTION_CAP: usize = 2000;

#[derive(Deserialize)]
pub struct IntakeRequest {
    pub question: String,
    #[serde(default)]
    pub ask: Option<String>,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub blocks: Vec<String>,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub asker: Option<String>,
    #[serde(default)]
    pub laws: Vec<crate::law_match::LawRow>,
    pub storage_root: PathBuf,
    /// The machine index, resolved by the Python side (`paths.questions_jsonl()`)
    /// so the caller's sandbox stays the single path authority. Falls back to
    /// `questions_path(home)` when absent.
    #[serde(default)]
    pub index_path: Option<PathBuf>,
    /// The project journal (`paths.project_log("events.jsonl")`), same
    /// authority rule as `index_path`. Falls back to the legacy
    /// `<storage_root>/.fno/events.jsonl`.
    #[serde(default)]
    pub journal_path: Option<PathBuf>,
    /// The user's stated name, interpolated into the law-refusal line.
    #[serde(default)]
    pub display_name: Option<String>,
    /// `fno inbox outstanding`'s render cap, for the does-not-render line.
    #[serde(default)]
    pub render_cap: Option<usize>,
}

#[derive(Serialize)]
pub struct IntakeAnswer {
    pub exit_code: i32,
    /// The stderr lines, in print order: the shim echoes them verbatim.
    pub lines: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qid: Option<String>,
    /// `law` | `node_pointer` | `context` | `decide_yourself` | `dedup` | `write` | `index`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_id: Option<String>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
}

impl IntakeAnswer {
    fn exit(code: i32) -> Self {
        IntakeAnswer {
            exit_code: code,
            lines: Vec::new(),
            qid: None,
            refusal: None,
            open_id: None,
            truncated: false,
            position: None,
            total: None,
        }
    }
}

/// The binary transport: request on stdin, answer on stdout. Exits 0
/// whenever an answer was computed; the verdict rides `exit_code` in it.
pub fn run_question_intake() -> i32 {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("fno-agents question-intake: could not read stdin");
        return 2;
    }
    let req: IntakeRequest = match serde_json::from_str(&input) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fno-agents question-intake: bad request: {e}");
            return 2;
        }
    };
    let home = AgentsHome::from_env();
    let answer = run_intake(&req, &home);
    match serde_json::to_string(&answer) {
        Ok(s) => println!("{s}"),
        Err(e) => {
            eprintln!("fno-agents question-intake: could not serialize answer: {e}");
            return 1;
        }
    }
    0
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get("data")
        .and_then(|d| d.get(key))
        .or_else(|| v.get(key))
        .and_then(Value::as_str)
}

/// One open question seen in the index, for dedup and the receipt.
#[derive(Debug)]
struct OpenRow {
    id: String,
    ts: String,
    blocks: usize,
    subject: Option<String>,
    node: Option<String>,
}

/// Latest ask per id minus closes, folded from the machine index. A
/// malformed line is skipped; the receipt is advisory and dedup is best
/// effort against the rows that do parse.
pub fn question_rows(index: &Path) -> (Map<String, Value>, HashSet<String>) {
    // Store rows first: Python commits closes to questions.db without
    // touching the raw journal, so a raw read marks closed questions open.
    let raw =
        crate::event_store::journal_text(index, &["operator_question", "operator_question_closed"]);
    let mut asked: Map<String, Value> = Map::new();
    let mut closed = HashSet::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let qid = match str_field(&v, "question_id") {
            Some(q) => q.to_string(),
            None => continue,
        };
        match v.get("type").and_then(Value::as_str) {
            Some("operator_question") => {
                asked.insert(qid, v);
            }
            Some("operator_question_closed") => {
                closed.insert(qid);
            }
            _ => {}
        }
    }
    (asked, closed)
}

fn open_rows(index: &Path) -> Vec<OpenRow> {
    let (asked, closed) = question_rows(index);
    let mut out: Vec<OpenRow> = asked
        .into_iter()
        .filter(|(id, _)| !closed.contains(id))
        .map(|(id, v)| OpenRow {
            blocks: v
                .get("data")
                .and_then(|d| d.get("blocks"))
                .or_else(|| v.get("blocks"))
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0),
            ts: v
                .get("ts")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            subject: str_field(&v, "subject").map(str::to_string),
            node: str_field(&v, "node").map(str::to_string),
            id,
        })
        .collect();
    // Newest first, then more blocks, then id (mirrors read_open_questions
    // with a zero liveness budget: one lane, no probes).
    out.sort_by(|a, b| {
        b.ts.cmp(&a.ts)
            .then_with(|| b.blocks.cmp(&a.blocks))
            .then_with(|| b.id.cmp(&a.id))
    });
    out
}

/// True when the text carries any question-file section (so the title line
/// becomes the question), per the template in docs/architecture/
/// attention-items.md.
fn is_question_file(text: &str) -> bool {
    const SECTIONS: [&str; 9] = [
        "## Options",
        "## Recommendation",
        "## Blocked because",
        "## Why these options",
        "## Downside",
        "## Not thought through",
        "## Reversible",
        "## Cost if wrong",
        "## Meanwhile",
    ];
    SECTIONS.iter().any(|s| text.contains(s))
}

fn title_of(text: &str, parsed_title: &str) -> String {
    if !parsed_title.trim().is_empty() {
        return parsed_title.trim().to_string();
    }
    let mut lines = text.lines().map(str::trim).peekable();
    if lines.peek() == Some(&"---") {
        // Skip a leading frontmatter block; the title is the first prose line.
        lines.next();
        for line in lines.by_ref() {
            if line == "---" {
                break;
            }
        }
    }
    lines
        .find(|l| !l.is_empty() && !l.starts_with("---"))
        .unwrap_or("")
        .to_string()
}

/// `q-` + 8 hex from the OS entropy pool; a time+pid hash when it cannot be
/// read, so a mint never fails the ask.
fn mint_id() -> String {
    let mut bytes = [0u8; 4];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok();
    if !ok {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mix = nanos as u64 ^ ((std::process::id() as u64) << 32);
        bytes.copy_from_slice(&mix.to_le_bytes()[..4]);
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("q-{hex}")
}

/// True when the question carries retract intent and names one of `ids`.
/// The id citation is what keeps the exemption narrow - "revoke the
/// credentials" alone never qualifies - and mirrors the nearby tier's own
/// remedy ("name every id above and ask again").
fn cites_retraction(question: &str, ids: &[String]) -> bool {
    let lower = question.to_lowercase();
    let intent = ["retract", "repeal", "rescind", "revoke", "overturn"]
        .iter()
        .any(|w| lower.contains(w));
    intent && ids.iter().any(|id| lower.contains(&id.to_lowercase()))
}

/// True when the question's closing action is the retraction of a live law
/// whose retraction only the operator may record. The lane verdict is the
/// retract gate's own rule (`_decision_lane`), consumed as data.
fn targets_operator_retraction(question: &str, laws: &[crate::law_match::LawRow]) -> bool {
    laws.iter().any(|l| {
        l.retraction_needs_operator()
            && cites_retraction(question, std::slice::from_ref(&l.decision_id))
    })
}

pub fn run_intake(req: &IntakeRequest, home: &AgentsHome) -> IntakeAnswer {
    // The user's name for the refusal line, and the render cap for the
    // receipt; both absent read as the plain shapes.
    let who = req.display_name.as_deref().unwrap_or("the user");
    let render_cap = req.render_cap.unwrap_or(10);

    let truncated = req.question.chars().count() > QUESTION_CAP;
    let mut answer = IntakeAnswer::exit(0);
    if truncated {
        answer.truncated = true;
        answer.lines.push(format!(
            "outstanding: recorded truncated: the question is {} characters, the event stores {QUESTION_CAP}.",
            req.question.chars().count()
        ));
    }

    // The law refusal first: a live law on this subject means the question
    // is never recorded (fail-closed, d-0fa92eb9) - unless the question's
    // closing action is the retraction of that law. A law-lane row refuses
    // every non-operator retraction (`retract_decision`), so "act on the
    // law; do not ask" has no agent-side remedy there and the user is the
    // only door. The closing action may ride the pin line (`--ask`), so it
    // scans with the question, not after it.
    let closing_text = match req.ask.as_deref() {
        Some(ask) if !ask.trim().is_empty() => format!("{}\n{}", req.question, ask),
        _ => req.question.clone(),
    };
    let retraction_ask = targets_operator_retraction(&closing_text, &req.laws);
    let law_verdict = crate::law_match::ask_answer(&crate::law_match::AskRequest {
        question: req.question.clone(),
        subject: req.subject.clone(),
        node: req.node.clone(),
        laws: req.laws.clone(),
    });
    if (!law_verdict.exact.is_empty() && !retraction_ask) || law_verdict.nearby_refusal.is_some() {
        for hit in &law_verdict.exact {
            let mut line = format!(
                "outstanding: refused: live law already rules on '{}' ({}). Read it: \
fno inbox decisions {} --lane law --state live. Act on the law; do not ask {who}.",
                hit.subject,
                hit.ids.join(", "),
                hit.subject
            );
            if req.subject.is_none() {
                line += " If the question is about another subject, name it with --subject.";
            }
            line += " If the question asks to retract this law, name its id in the \
question and ask again: only the user may record that retraction.";
            answer.lines.push(line);
        }
        if let Some(refusal) = law_verdict.nearby_refusal {
            answer.lines.push(refusal);
        }
        answer.refusal = Some("law".to_string());
        answer.exit_code = 2;
        return answer;
    }

    let parsed = crate::escalation::parse(&req.question);
    let from_file = is_question_file(&req.question);
    // File options carry a per-option next; flag options stay bare strings.
    let file_options = !parsed.options.is_empty();
    let has_options = file_options || !req.options.is_empty();

    // Node-pointer rule: a question with choices names its node.
    let node = req.node.as_deref().map(str::trim).filter(|n| !n.is_empty());
    if has_options && node.is_none() {
        answer.lines.push(
            "outstanding: refused: a question with options names its node (--node). \
One line plus a node pointer (law d-59af3235)."
                .to_string(),
        );
        answer.refusal = Some("node_pointer".to_string());
        answer.exit_code = 2;
        return answer;
    }

    // The context refusal (user design 2026-09-22): the page writer cannot
    // invent context, so a question carries what, why, two options and a
    // recommendation with its reason. One action with no choice is a pin
    // and passes.
    let is_pin = !has_options && req.ask.as_deref().is_some_and(|a| !a.trim().is_empty());
    if !is_pin {
        let mut missing: Vec<&str> = Vec::new();
        if title_of(&req.question, &parsed.title).trim().is_empty() {
            missing.push("what");
        }
        if parsed.blocked_because.trim().is_empty() {
            missing.push("why");
        }
        if parsed.options.len() < 2 && req.options.len() < 2 {
            missing.push("two options");
        }
        let recommendation_ok = parsed
            .recommend
            .is_some_and(|r| (1..=parsed.options.len()).contains(&r))
            && !parsed.recommendation.trim().is_empty();
        if !recommendation_ok {
            missing.push("a recommendation");
        }
        if !missing.is_empty() {
            answer.lines.push(format!(
                "outstanding: refused: a question needs {}. Write a question file \
(docs/architecture/attention-items.md, \"Asking with context\") and pass \
--question-file. One action with no choice is a pin: pass --ask \"<the action>\".",
                missing.join(", ")
            ));
            answer.refusal = Some("context".to_string());
            answer.exit_code = 2;
            return answer;
        }
        // The asker-must-decide refusal (user ruling 2026-09-22): a
        // reversible question with a recommendation is one the asker or its
        // king settles itself; it reaches the user only with a user-only
        // reason in why_user. A question whose closing action is an
        // operator-only retraction is not the asker's to settle, whatever
        // the file says: the same authority rule decides here.
        if parsed.reversible.trim().eq_ignore_ascii_case("yes") {
            let why = parsed.why_user.to_ascii_lowercase();
            let user_only = [
                "irreversible",
                "money",
                "credential",
                "outside",
                "product",
                "taste",
            ]
            .iter()
            .any(|k| why.contains(k));
            if !user_only && !retraction_ask {
                let decide = match node {
                    Some(n) => format!("fno backlog decide {n} \"<ruling>\""),
                    None => "fno backlog decide <node> \"<ruling>\"".to_string(),
                };
                answer.lines.push(format!(
                    "outstanding: refused: you can decide this one: it carries a \
recommendation and marks itself reversible. Record the ruling yourself or \
hand it to your king: {decide} (--authority crown or agent), then continue. \
The user answers only what only a user can: irreversible, spends money or a \
credential, reaches outside the machine, or a product or taste call - name it \
with why_user: in the question file."
                ));
                answer.refusal = Some("decide_yourself".to_string());
                answer.exit_code = 2;
                return answer;
            }
        }
    }

    // Dedup: an open question on the same subject and node already waits.
    let subject = req
        .subject
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let index = req
        .index_path
        .clone()
        .unwrap_or_else(|| questions_path(home));
    if let (Some(s), Some(n)) = (subject, node) {
        let dup = open_rows(&index).into_iter().find(|row| {
            row.subject.as_deref().map(str::trim) == Some(s) && row.node.as_deref() == Some(n)
        });
        if let Some(row) = dup {
            answer.lines.push(format!(
                "outstanding: refused: an open question on subject '{s}' and node '{n}' \
already waits ({}). Answer it or clear it; do not ask twice.",
                row.id
            ));
            answer.refusal = Some("dedup".to_string());
            answer.open_id = Some(row.id);
            answer.exit_code = 2;
            return answer;
        }
    }

    let stored_question: String = if from_file {
        title_of(&req.question, &parsed.title)
    } else if truncated {
        req.question.chars().take(QUESTION_CAP).collect()
    } else {
        req.question.clone()
    };

    let mut data = Map::new();
    let qid = mint_id();
    data.insert("question_id".into(), json!(qid));
    data.insert("question".into(), json!(stored_question));
    for (key, value) in [
        ("session_id", &req.session_id),
        ("cwd", &req.cwd),
        ("asker", &req.asker),
        ("ask", &req.ask),
    ] {
        if let Some(v) = value.as_deref().filter(|v| !v.is_empty()) {
            data.insert(key.into(), json!(v));
        }
    }
    if let Some(n) = node {
        data.insert("node".into(), json!(n));
    }
    if !req.blocks.is_empty() {
        data.insert("blocks".into(), json!(req.blocks));
    }
    if has_options {
        if file_options {
            let options: Vec<Value> = parsed
                .options
                .iter()
                .enumerate()
                .map(|(i, o)| {
                    let mut m = Map::new();
                    m.insert("n".into(), json!(i + 1));
                    m.insert("text".into(), json!(o.text));
                    if !o.next.is_empty() {
                        m.insert("next".into(), json!(o.next));
                    }
                    Value::Object(m)
                })
                .collect();
            data.insert("options".into(), Value::Array(options));
        } else {
            data.insert("options".into(), json!(req.options));
        }
    }
    if let Some(subject) = subject {
        data.insert("subject".into(), json!(subject));
    }

    // The structured context: every non-empty field the projection reads.
    let mut context = Map::new();
    for (key, value) in [
        ("blocked_because", &parsed.blocked_because),
        ("options_rationale", &parsed.options_rationale),
        ("unknowns", &parsed.unknowns),
        ("reversible", &parsed.reversible),
        ("cost_if_wrong", &parsed.cost_if_wrong),
        ("meanwhile", &parsed.meanwhile),
    ] {
        if !value.trim().is_empty() {
            context.insert(key.into(), json!(value.trim()));
        }
    }
    if let Some(rec) = parsed.recommend {
        if rec >= 1 && rec <= parsed.options.len() {
            let mut recommendation = Map::new();
            recommendation.insert("option".into(), json!(rec));
            recommendation.insert("why".into(), json!(parsed.recommendation.trim()));
            if !parsed.downside.trim().is_empty() {
                recommendation.insert("downside".into(), json!(parsed.downside.trim()));
            }
            context.insert("recommendation".into(), Value::Object(recommendation));
        }
    }
    if !context.is_empty() {
        data.insert("context".into(), Value::Object(context));
    }

    // One envelope, two sinks: the journal and the recall index must carry
    // the identical line, so the ts is stamped once. The journal is the
    // durable half and its failure is fatal. The journal path travels in
    // the request (the Python sandbox stays the single path authority, as
    // with index_path); the legacy .fno path is the fallback.
    let journal_path = req
        .journal_path
        .clone()
        .unwrap_or_else(|| req.storage_root.join(".fno").join("events.jsonl"));
    let event = json!({
        "ts": crate::events::now_rfc3339(),
        "type": "operator_question",
        "source": "target",
        "data": Value::Object(data),
    });
    let line = match serde_json::to_string(&event) {
        Ok(l) => l,
        Err(e) => {
            answer.lines.push(format!(
                "outstanding: failed to record question: could not serialize the envelope: {e}"
            ));
            answer.refusal = Some("write".to_string());
            answer.exit_code = 1;
            return answer;
        }
    };
    if let Err(e) = crate::event_store::append_envelope(&journal_path, &line, None) {
        answer.lines.push(format!(
            "outstanding: failed to record question: failed to append question to project journal: {e}"
        ));
        answer.refusal = Some("write".to_string());
        answer.exit_code = 1;
        return answer;
    }

    // Machine-wide recall index: best-effort, reported.
    let index_error = crate::provider_cap::append_questions_row(&index, &event).err();
    if let Some(e) = index_error {
        answer.lines.push(format!(
            "outstanding: recorded {qid} in the project journal, but the recall index \
write failed: {e}. Run `fno inbox outstanding reindex`; do not retry ask, which \
would mint a second id for the same question."
        ));
        answer.qid = Some(qid);
        answer.exit_code = 1;
        return answer;
    }

    answer.lines.push(format!(
        "outstanding: recorded {qid}. Clear it once answered: \
fno inbox outstanding clear {qid} --answer \"...\""
    ));
    let (position, total) = receipt_position(&index, &qid);
    let total = total.unwrap_or(0);
    answer.position = position;
    answer.total = Some(total);
    match position {
        None => answer.lines.push(
            "outstanding: recorded, but its render position could not be read; \
run fno inbox outstanding to check."
                .to_string(),
        ),
        Some(p) if p <= render_cap => {
            answer.lines.push(format!(
                "outstanding: {qid} renders at position {p} of {total}."
            ));
        }
        Some(p) => answer.lines.push(format!(
            "outstanding: {qid} does NOT render: position {p} of {total}, and \
fno inbox outstanding prints {render_cap}. Nothing will show it to the operator; \
raise it another way or answer it yourself."
        )),
    }
    answer.qid = Some(qid);
    answer
}

/// Where the new id lands in the render: (position 1-based, total). None
/// means the read failed and the receipt says so instead of guessing.
fn receipt_position(index: &Path, qid: &str) -> (Option<usize>, Option<usize>) {
    let rows = open_rows(index);
    let total = rows.len();
    let position = rows.iter().position(|r| r.id == qid).map(|i| i + 1);
    match position {
        Some(p) => (Some(p), Some(total)),
        None => (None, Some(total)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::law_match::LawRow;

    fn tmp_home(tag: &str) -> AgentsHome {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-agents-qintake-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Mirror the prod shape (<home>/.fno/agents): questions_path reads
        // root.parent(), and a home rooted at p itself would land the index
        // in the shared temp dir.
        let home = AgentsHome::at(&p.join(".fno").join("agents"));
        home.ensure_root().unwrap();
        home
    }

    fn tmp_root(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-agents-qintake-root-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn req(question: &str, root: &Path) -> IntakeRequest {
        IntakeRequest {
            question: question.to_string(),
            ask: None,
            options: vec![],
            blocks: vec![],
            node: None,
            subject: None,
            session_id: Some("s1".to_string()),
            cwd: Some("/repo/fno".to_string()),
            asker: Some("worker-1".to_string()),
            laws: vec![],
            storage_root: root.to_path_buf(),
            index_path: None,
            journal_path: None,
            display_name: None,
            render_cap: None,
        }
    }

    fn journal_text(root: &Path) -> String {
        // Writes commit to the store beside the journal; the raw file is
        // never appended, so tests read committed rows.
        crate::events::committed_journal_text(&root.join(".fno/events.jsonl"))
    }

    const QUESTION_FILE: &str = "\
---
recommend: 1
---

Is a net-zero Python repair legal with no grant?

## Options
1. Yes, net zero or less needs no grant.
    What happens next: unblocks four fixes today
2. Stay strict.
    What happens next: every Python fix waits on a grant

## Blocked because
the reconcile fix and the merge_close fix are both Python edits

## Why these options
the three readings kings have acted on

## Downside
a repair can hide a feature

## Recommendation
Option 1, the narrowest door: every later fix needs it.

## Not thought through
whether a net-zero move between files counts

## Reversible
costly

## Cost if wrong
the push allowance drops to 0

## Meanwhile
stops
";

    #[test]
    fn ac7_hp_question_file_records_context_and_object_options() {
        let home = tmp_home("hp");
        let root = tmp_root("hp");
        let mut r = req(QUESTION_FILE, &root);
        r.node = Some("x-aaaa".to_string());
        let answer = run_intake(&r, &home);
        assert_eq!(answer.exit_code, 0, "lines: {:?}", answer.lines);
        let qid = answer.qid.clone().unwrap();
        assert!(qid.starts_with("q-"));
        assert_eq!(qid.len(), 10);

        let row: Value = journal_text(&root)
            .lines()
            .last()
            .map(|l| serde_json::from_str(l).unwrap())
            .unwrap();
        let data = row.get("data").unwrap();
        assert_eq!(
            data.get("question").and_then(Value::as_str),
            Some("Is a net-zero Python repair legal with no grant?")
        );
        let options = data.get("options").and_then(Value::as_array).unwrap();
        assert_eq!(options.len(), 2);
        assert_eq!(
            options[0].get("next").and_then(Value::as_str),
            Some("unblocks four fixes today")
        );
        let ctx = data.get("context").unwrap();
        for key in [
            "blocked_because",
            "options_rationale",
            "recommendation",
            "unknowns",
            "reversible",
            "cost_if_wrong",
            "meanwhile",
        ] {
            assert!(ctx.get(key).is_some(), "context missing {key}");
        }
        assert_eq!(
            ctx.get("recommendation")
                .and_then(|r| r.get("option"))
                .and_then(Value::as_u64),
            Some(1)
        );
        // The index row landed too.
        let index =
            crate::event_store::journal_text(&questions_path(&home), &["operator_question"]);
        assert!(index.contains(&qid));
        assert_eq!(answer.position, Some(1));
        assert_eq!(answer.total, Some(1));
    }

    #[test]
    fn ac7_err_file_options_without_node_is_refused_and_records_nothing() {
        let home = tmp_home("err");
        let root = tmp_root("err");
        let answer = run_intake(&req(QUESTION_FILE, &root), &home);
        assert_eq!(answer.exit_code, 2);
        assert_eq!(answer.refusal.as_deref(), Some("node_pointer"));
        assert!(journal_text(&root).is_empty());
    }

    #[test]
    fn ac7_edge_plain_flags_with_options_refuse_for_context() {
        let home = tmp_home("edge");
        let root = tmp_root("edge");
        let mut r = req("plain text question", &root);
        r.options = vec!["a".to_string(), "b".to_string()];
        r.node = Some("x-aaaa".to_string());
        let answer = run_intake(&r, &home);
        assert_eq!(answer.exit_code, 2);
        assert_eq!(answer.refusal.as_deref(), Some("context"));
        assert!(answer.lines.iter().any(|l| l.contains("a question needs")));
        assert!(journal_text(&root).is_empty());
    }

    #[test]
    fn ac9_hp_plain_ask_line_is_refused_and_records_nothing() {
        let home = tmp_home("hp-refusal");
        let root = tmp_root("hp-refusal");
        let answer = run_intake(&req("which lane?", &root), &home);
        assert_eq!(answer.exit_code, 2, "lines: {:?}", answer.lines);
        assert_eq!(answer.refusal.as_deref(), Some("context"));
        assert!(
            answer
                .lines
                .iter()
                .any(|l| l.contains("why, two options and a recommendation")
                    || l.contains("a question needs")),
            "the refusal names the missing parts: {:?}",
            answer.lines
        );
        assert!(journal_text(&root).is_empty());
        assert_eq!(answer.qid, None);
    }

    #[test]
    fn ac9_edge_a_pin_and_a_complete_question_file_pass() {
        // A pin passes: one action, no choice.
        let home = tmp_home("pin");
        let root = tmp_root("pin");
        let mut pin = req("note to self", &root);
        pin.ask = Some("publish the crate".to_string());
        let answer = run_intake(&pin, &home);
        assert_eq!(answer.exit_code, 0, "lines: {:?}", answer.lines);
        assert!(journal_text(&root).lines().count() == 1);
        // A complete question file passes.
        let home = tmp_home("file-ok");
        let root = tmp_root("file-ok");
        let mut r = req(QUESTION_FILE, &root);
        r.node = Some("x-aaaa".to_string());
        let answer2 = run_intake(&r, &home);
        assert_eq!(answer2.exit_code, 0, "lines: {:?}", answer2.lines);
        let _ = answer;
    }

    #[test]
    fn rule10_reversible_recommended_question_refused_without_why_user() {
        let home = tmp_home("rule10");
        let root = tmp_root("rule10");
        let question = QUESTION_FILE.replace("## Reversible\ncostly", "## Reversible\nyes");
        let mut r = req(&question, &root);
        r.node = Some("x-aaaa".to_string());
        let answer = run_intake(&r, &home);
        assert_eq!(answer.exit_code, 2, "lines: {:?}", answer.lines);
        assert_eq!(answer.refusal.as_deref(), Some("decide_yourself"));
        assert!(
            answer
                .lines
                .iter()
                .any(|l| l.contains("fno backlog decide")),
            "the refusal names the decide door: {:?}",
            answer.lines
        );
        assert!(journal_text(&root).is_empty());
    }

    #[test]
    fn rule10_why_user_irreversible_lets_it_pass() {
        let home = tmp_home("rule10-pass");
        let root = tmp_root("rule10-pass");
        let question = QUESTION_FILE
            .replace("## Reversible\ncostly", "## Reversible\nyes")
            .replace(
                "## Meanwhile\nstops\n",
                "## Meanwhile\nstops\n\n## Why user\nirreversible\n",
            );
        let mut r = req(&question, &root);
        r.node = Some("x-aaaa".to_string());
        let answer = run_intake(&r, &home);
        assert_eq!(answer.exit_code, 0, "lines: {:?}", answer.lines);
    }

    #[test]
    fn rule10_empty_why_user_is_refused() {
        let home = tmp_home("rule10-empty");
        let root = tmp_root("rule10-empty");
        let question = QUESTION_FILE
            .replace("## Reversible\ncostly", "## Reversible\nyes")
            .replace(
                "## Meanwhile\nstops\n",
                "## Meanwhile\nstops\n\n## Why user\n\n",
            );
        let mut r = req(&question, &root);
        r.node = Some("x-aaaa".to_string());
        let answer = run_intake(&r, &home);
        assert_eq!(answer.exit_code, 2);
        assert_eq!(answer.refusal.as_deref(), Some("decide_yourself"));
    }

    fn junk_law() -> crate::law_match::LawRow {
        crate::law_match::LawRow {
            decision_id: "d-junk0001".to_string(),
            subject: Some("junk-law".to_string()),
            decision: Some("a placeholder ruling".to_string()),
            ts: Some("2026-08-29T19:20:10Z".to_string()),
            lane: Some("law".to_string()),
        }
    }

    #[test]
    fn a_question_to_retract_a_law_is_accepted() {
        // The closing action is a retraction, and the retract gate refuses
        // every non-operator authority on a law-lane row: the ask is the
        // only door, so neither the law refusal nor decide_yourself fires.
        let home = tmp_home("retract-accept");
        let root = tmp_root("retract-accept");
        let question = QUESTION_FILE
            .replace("## Reversible\ncostly", "## Reversible\nyes")
            .replace(
                "Is a net-zero Python repair legal with no grant?",
                "Retract d-junk0001: the junk law ruling on junk-law?",
            );
        let mut r = req(&question, &root);
        r.subject = Some("junk-law".to_string());
        r.node = Some("x-aaaa".to_string());
        r.laws = vec![junk_law()];
        let answer = run_intake(&r, &home);
        assert_eq!(answer.exit_code, 0, "lines: {:?}", answer.lines);
        assert!(journal_text(&root).lines().count() == 1);
    }

    #[test]
    fn the_retraction_exemption_needs_the_id_not_the_intent_alone() {
        let home = tmp_home("retract-no-id");
        let root = tmp_root("retract-no-id");
        let question = QUESTION_FILE
            .replace("## Reversible\ncostly", "## Reversible\nyes")
            .replace(
                "Is a net-zero Python repair legal with no grant?",
                "Retract the junk law ruling on junk-law?",
            );
        let mut r = req(&question, &root);
        r.subject = Some("junk-law".to_string());
        r.node = Some("x-aaaa".to_string());
        r.laws = vec![junk_law()];
        let answer = run_intake(&r, &home);
        assert_eq!(answer.exit_code, 2, "lines: {:?}", answer.lines);
        assert_eq!(answer.refusal.as_deref(), Some("law"));
    }

    #[test]
    fn citing_the_law_id_without_retract_intent_still_refuses() {
        let home = tmp_home("cite-no-retract");
        let root = tmp_root("cite-no-retract");
        let mut r = req(
            "d-junk0001 rules on junk-law; how do we comply with it?",
            &root,
        );
        r.subject = Some("junk-law".to_string());
        r.ask = Some("note the reading".to_string());
        r.laws = vec![junk_law()];
        let answer = run_intake(&r, &home);
        assert_eq!(answer.exit_code, 2, "lines: {:?}", answer.lines);
        assert_eq!(answer.refusal.as_deref(), Some("law"));
    }

    #[test]
    fn a_reversible_agent_decidable_question_is_still_refused() {
        // Live laws in the payload do not widen the exemption: without the
        // id citation and intent, rule 10 holds. No law matches this
        // question, so the decide_yourself gate is what refuses it.
        let home = tmp_home("retract-still-refused");
        let root = tmp_root("retract-still-refused");
        let question = QUESTION_FILE.replace("## Reversible\ncostly", "## Reversible\nyes");
        let mut r = req(&question, &root);
        r.node = Some("x-aaaa".to_string());
        r.laws = vec![junk_law()];
        let answer = run_intake(&r, &home);
        assert_eq!(answer.exit_code, 2, "lines: {:?}", answer.lines);
        assert_eq!(answer.refusal.as_deref(), Some("decide_yourself"));
    }

    #[test]
    fn a_row_without_a_lane_verdict_keeps_the_old_refusal() {
        // Older callers send no lane; the exemption never arms, which keeps
        // the captured goldens and every pre-lane caller on the old door.
        let home = tmp_home("retract-no-lane");
        let root = tmp_root("retract-no-lane");
        let mut law = junk_law();
        law.lane = None;
        let mut r = req(
            "Retract d-junk0001: the junk law ruling on junk-law?",
            &root,
        );
        r.subject = Some("junk-law".to_string());
        r.ask = Some("retract the junk law".to_string());
        r.laws = vec![law];
        let answer = run_intake(&r, &home);
        assert_eq!(answer.exit_code, 2, "lines: {:?}", answer.lines);
        assert_eq!(answer.refusal.as_deref(), Some("law"));
    }

    #[test]
    fn a_pin_whose_ask_line_carries_the_retraction_is_accepted() {
        // The closing action is the --ask line: a pin whose question names
        // the law only by subject still passes when the action names the id.
        let home = tmp_home("retract-pin-ask");
        let root = tmp_root("retract-pin-ask");
        let mut r = req("the junk law on junk-law blocks this node.", &root);
        r.subject = Some("junk-law".to_string());
        r.ask = Some("retract d-junk0001".to_string());
        r.laws = vec![junk_law()];
        let answer = run_intake(&r, &home);
        assert_eq!(answer.exit_code, 0, "lines: {:?}", answer.lines);
        assert!(journal_text(&root).lines().count() == 1);
    }

    #[test]
    fn ac9_edge_second_ask_on_same_subject_and_node_names_the_open_id() {
        let home = tmp_home("dedup");
        let root = tmp_root("dedup");
        let mut first = req("first", &root);
        first.subject = Some("subject-s".to_string());
        first.node = Some("x-aaaa".to_string());
        first.ask = Some("finish the lane".to_string());
        assert_eq!(run_intake(&first, &home).exit_code, 0);

        let mut second = req("second", &root);
        second.subject = Some("subject-s".to_string());
        second.node = Some("x-aaaa".to_string());
        second.ask = Some("finish the lane".to_string());
        let answer = run_intake(&second, &home);
        assert_eq!(answer.exit_code, 2);
        assert_eq!(answer.refusal.as_deref(), Some("dedup"));
        assert!(answer
            .open_id
            .as_deref()
            .is_some_and(|id| id.starts_with("q-")));
        // Nothing new recorded.
        assert_eq!(journal_text(&root).lines().count(), 1);
    }

    #[test]
    fn law_refusal_records_nothing() {
        let home = tmp_home("law");
        let root = tmp_root("law");
        let mut r = req("test subject again?", &root);
        r.laws = vec![LawRow {
            decision_id: "d-test0001".to_string(),
            subject: Some("test-subject".to_string()),
            decision: Some("stay strict".to_string()),
            ts: Some("2026-09-01T00:00:00Z".to_string()),
            lane: Some("law".to_string()),
        }];
        let answer = run_intake(&r, &home);
        assert_eq!(answer.exit_code, 2);
        assert_eq!(answer.refusal.as_deref(), Some("law"));
        assert!(
            answer.lines.iter().any(|l| l.contains("d-test0001")),
            "the refusal names the law id: {:?}",
            answer.lines
        );
        assert!(journal_text(&root).is_empty());
    }

    #[test]
    fn over_cap_question_is_cut_at_the_cap_and_flagged() {
        let home = tmp_home("cap");
        let root = tmp_root("cap");
        let long = "x".repeat(QUESTION_CAP + 50);
        let mut r = req(&long, &root);
        r.ask = Some("note the cap".to_string());
        let answer = run_intake(&r, &home);
        assert_eq!(answer.exit_code, 0);
        assert!(answer.truncated);
        let row: Value = journal_text(&root)
            .lines()
            .last()
            .map(|l| serde_json::from_str(l).unwrap())
            .unwrap();
        assert_eq!(
            row.pointer("/data/question")
                .and_then(Value::as_str)
                .unwrap()
                .chars()
                .count(),
            QUESTION_CAP
        );
    }

    #[test]
    fn receipt_ranks_newest_first_and_names_the_new_row() {
        let home = tmp_home("receipt");
        let root = tmp_root("receipt");
        let index = questions_path(&home);
        let older = json!({
            "ts": "2026-09-01T00:00:00Z",
            "type": "operator_question",
            "source": "target",
            "data": {"question_id": "q-old", "question": "old", "blocks": ["x-1", "x-2"]}
        });
        crate::provider_cap::append_questions_row(&index, &older).unwrap();
        let mut r = req("newest question", &root);
        r.ask = Some("finish the lane".to_string());
        let answer = run_intake(&r, &home);
        assert_eq!(answer.total, Some(2));
        assert_eq!(answer.position, Some(1), "newest sorts first");
    }

    #[test]
    fn ac5_hp_a_store_only_close_unmarks_the_question() {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-agents-qintake-openrows-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        let index = p.join("questions.jsonl");
        let ask = r#"{"ts":"2026-09-23T01:00:00Z","type":"operator_question","source":"agent","data":{"question_id":"q-t1","question":"ship?","blocks":[]}}"#;
        std::fs::write(&index, format!("{ask}\n")).unwrap();
        let close = json!({
            "ts": "2026-09-23T01:05:00Z",
            "type": "operator_question_closed",
            "source": "agent",
            "data": {"question_id": "q-t1"}
        });
        crate::event_store::append_envelope(&index, &close.to_string(), None).unwrap();
        let open = open_rows(&index);
        assert!(
            open.iter().all(|r| r.id != "q-t1"),
            "a store-only close must unmark the question: {open:?}"
        );
    }
}
