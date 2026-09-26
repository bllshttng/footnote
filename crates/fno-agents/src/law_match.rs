//! The question-to-law matcher: one pure function set behind the front's
//! `fno inbox law` group. The `stage` and `law` modes
//! read the decision index through [`crate::decision_index`]; the `ask` and
//! `validate` modes read no file - Python keeps the decision lifecycle read
//! (`list_decisions`) and the open-question fold (`read_open_questions`) and
//! works only on rows handed to it.

use crate::decision_index;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::Read;

/// Lowercase, split on anything outside `[a-z0-9]`, then drop digit-only
/// tokens, 4-8 character hex tokens that contain a digit (node ids like
/// `14c8`, `cf6a`), and the stop set. A hex word with no digit (`added`,
/// `face`) survives.
pub fn tokens(s: &str) -> BTreeSet<String> {
    const STOP: [&str; 19] = [
        "pr", "x", "ab", "q", "d", "a", "an", "the", "of", "to", "and", "or", "for", "on", "in",
        "is", "it", "no", "not",
    ];
    s.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .filter(|t| !t.bytes().all(|b| b.is_ascii_digit()))
        .filter(|t| {
            let hex_with_digit = (4..=8).contains(&t.len())
                && t.bytes().all(|b| b.is_ascii_hexdigit())
                && t.bytes().any(|b| b.is_ascii_digit());
            !hex_with_digit
        })
        .filter(|t| !STOP.contains(t))
        .map(str::to_owned)
        .collect()
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
enum MatchRequest {
    Ask(AskRequest),
    Law(LawRequest),
    Stage(StageRequest),
    Validate(ValidateRequest),
    #[serde(rename = "record-scope")]
    RecordScope(RecordScopeRequest),
    /// The law write door: `argv` is the `fno inbox law set` command line and
    /// `stdin` is the text `--decision-file -` reads. The answer is NOT a JSON
    /// envelope - the door owns stdout (the decision id) and the process exit
    /// code (0 recorded, 1 recorded-but-index-failed, 3 refused).
    #[serde(rename = "record")]
    Record(RecordDoorRequest),
    #[serde(rename = "scope-split")]
    ScopeSplit(ScopeSplitRequest),
}

/// The record door's request: the law-set argv plus the caller's stdin.
#[derive(Deserialize)]
struct RecordDoorRequest {
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    stdin: String,
}

/// The law door's scope stamp: the recording project by default,
/// `global` only by explicit flag, because law is never inherited by silence.
/// `paths` names the repo-relative globs an edit read keys the law by.
#[derive(Deserialize)]
struct RecordScopeRequest {
    #[serde(default)]
    r#global: bool,
    #[serde(default)]
    paths: Vec<String>,
}

/// The `list_decisions` scope filter: rows in, the kept rows plus the
/// withheld count and a renderable note out. One matcher for both languages;
/// the Python side is a transport, fail-open on any crate trouble.
#[derive(Deserialize)]
struct ScopeSplitRequest {
    rows: Vec<Value>,
}
/// The raw hook payload, verbatim from the harness event. `paths` carries the
/// file targets of a PreToolUse Edit|Write payload (the hook reads them off
/// `tool_input`); non-empty `paths` routes the answer to the edit read.
#[derive(Deserialize)]
struct StageRequest {
    hook: serde_json::Value,
    #[serde(default)]
    paths: Vec<String>,
}

/// The statement `fno inbox law set` wants recorded. `rationale` and
/// `supersedes` are Options because both may be absent; Python sends null.
#[derive(Deserialize)]
struct ValidateRequest {
    subject: String,
    decision: String,
    #[serde(default)]
    rationale: Option<String>,
    #[serde(default)]
    supersedes: Option<String>,
}

// pub(crate): question_intake builds one of these from the transport request
// and reuses the matcher, so the ask refusal logic lives in exactly one place.
#[derive(Deserialize)]
pub(crate) struct AskRequest {
    pub(crate) question: String,
    #[serde(default)]
    pub(crate) subject: Option<String>,
    #[serde(default)]
    pub(crate) node: Option<String>,
    pub(crate) laws: Vec<LawRow>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct LawRow {
    pub decision_id: String,
    // Option, not String+default: Python rows carry null for a missing
    // decision body or ts, and serde's `default` covers absent keys only.
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub ts: Option<String>,
    /// The authority lane the row was classified into by Python's
    /// `_decision_lane` (`list_decisions` stamps it). That function is the
    /// one implementation of the lane rule; this side consumes its verdict
    /// and never re-derives it. Absent (older callers, goldens) reads as
    /// "not law", the historical behavior of every consumer below.
    #[serde(default)]
    pub lane: Option<String>,
}

impl LawRow {
    /// The retract gate's authority rule, as the ask gate reads it:
    /// `fno backlog decide-retract` refuses every non-operator authority on a
    /// law-lane row, so a question whose closing action is that retraction
    /// has no agent-side remedy and may reach the user.
    pub(crate) fn retraction_needs_operator(&self) -> bool {
        self.lane.as_deref() == Some("law")
    }
}

#[derive(Deserialize)]
struct LawRequest {
    law: LawRow,
    questions: Vec<OpenQuestion>,
}

#[derive(Deserialize, Serialize)]
struct OpenQuestion {
    id: String,
    #[serde(default)]
    ts: String,
    #[serde(default)]
    question: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    node: Option<String>,
    #[serde(default)]
    asker: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct ExactHit {
    pub(crate) subject: String,
    pub(crate) ids: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct NearbyHit {
    pub(crate) decision_id: String,
    pub(crate) subject: String,
    pub(crate) decision: String,
    pub(crate) shared: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct AskAnswer {
    pub(crate) ok: bool,
    pub(crate) exact: Vec<ExactHit>,
    pub(crate) nearby: Vec<NearbyHit>,
    pub(crate) uncited: Vec<String>,
    pub(crate) nearby_refusal: Option<String>,
}

#[derive(Serialize)]
struct Candidate {
    question_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    asker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
    shared: Vec<String>,
}

#[derive(Serialize)]
struct LawAnswer {
    ok: bool,
    candidates: Vec<Candidate>,
    total: usize,
    lines: Vec<String>,
}

/// One-line, char-safe truncation: collapse whitespace, then cut at a char
/// boundary so a multi-byte character is never split.
fn one_line(text: &str, cap: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(cap) {
        Some((i, _)) => flat[..i].to_owned(),
        None => flat,
    }
}

/// The exact tier of `_live_law_hits`, ported unchanged. Words are the
/// `[a-z0-9]+` runs of the lowercased question; a law hits when its subject
/// casefold equals a named subject/node, or (only when NO subject was named)
/// every `-` part of a law subject with two or more parts is in the words.
fn exact_tier(req: &AskRequest) -> Vec<ExactHit> {
    let question_lower = req.question.to_lowercase();
    let words: BTreeSet<&str> = question_lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let named: Vec<String> = [req.subject.as_deref(), req.node.as_deref()]
        .into_iter()
        .flatten()
        .map(|s| s.to_lowercase())
        .collect();
    let mut hits: Vec<ExactHit> = Vec::new();
    for law in &req.laws {
        let key = law.subject.as_deref().unwrap_or("").trim();
        if key.is_empty() {
            continue;
        }
        let parts: Vec<String> = key.to_lowercase().split('-').map(str::to_owned).collect();
        let by_text = req.subject.is_none()
            && parts.len() >= 2
            && parts.iter().all(|p| words.contains(p.as_str()));
        if named.contains(&key.to_lowercase()) || by_text {
            if let Some(hit) = hits.iter_mut().find(|h| h.subject == key) {
                hit.ids.push(law.decision_id.clone());
            } else {
                hits.push(ExactHit {
                    subject: key.to_owned(),
                    ids: vec![law.decision_id.clone()],
                });
            }
        }
    }
    hits
}

/// The nearby tier: only when a subject was named and the exact tier found
/// nothing. A law is nearby when its subject's tokens share at least one
/// token with the named subject's tokens. At most 5 are listed, most shared
/// tokens first, then newest law.
fn nearby_tier(req: &AskRequest, exact: &[ExactHit]) -> Vec<NearbyHit> {
    let Some(subject) = req.subject.as_deref() else {
        return Vec::new();
    };
    if !exact.is_empty() {
        return Vec::new();
    }
    let subject_tokens = tokens(subject);
    let mut nearby: Vec<NearbyHit> = Vec::new();
    for law in &req.laws {
        let shared: Vec<String> = tokens(law.subject.as_deref().unwrap_or(""))
            .intersection(&subject_tokens)
            .cloned()
            .collect();
        if shared.is_empty() {
            continue;
        }
        nearby.push(NearbyHit {
            decision_id: law.decision_id.clone(),
            subject: law.subject.clone().unwrap_or_default(),
            decision: law.decision.clone().unwrap_or_default(),
            shared,
        });
    }
    // Most shared tokens first, then newest law ts (ISO strings sort
    // chronologically as bytes).
    nearby.sort_by(|a, b| {
        b.shared
            .len()
            .cmp(&a.shared.len())
            .then_with(|| b_ts(&req.laws, &b.decision_id).cmp(&b_ts(&req.laws, &a.decision_id)))
    });
    nearby.truncate(5);
    nearby
}

fn b_ts<'a>(laws: &'a [LawRow], id: &str) -> &'a str {
    laws.iter()
        .find(|l| l.decision_id == id)
        .and_then(|l| l.ts.as_deref())
        .unwrap_or("")
}

pub(crate) fn ask_answer(req: &AskRequest) -> AskAnswer {
    let exact = exact_tier(req);
    let nearby = nearby_tier(req, &exact);
    let uncited: Vec<String> = nearby
        .iter()
        .filter(|h| !req.question.contains(&h.decision_id))
        .map(|h| h.decision_id.clone())
        .collect();
    let nearby_refusal = if uncited.is_empty() {
        None
    } else {
        let mut line = String::from(
            "outstanding: refused: live law on a nearby subject may already answer this. ",
        );
        for h in &nearby {
            line.push_str(&format!(
                "{} ({}): {}. ",
                h.decision_id,
                h.subject,
                one_line(&h.decision, 160)
            ));
        }
        line.push_str(
            "Read each with fno inbox decisions <id>. If your question still stands, \
             name every id above in the question and ask again.",
        );
        Some(line)
    };
    AskAnswer {
        ok: true,
        exact,
        nearby,
        uncited,
        nearby_refusal,
    }
}

/// A candidate question: its subject's tokens share a token with the law
/// subject's tokens, or the exact tier's text rule holds (every `-` part of
/// a law subject with two or more parts is in the question words). Order is
/// shared-token count, then newest question.
/// The pure core: existing tests pin `lines` exactly, so the near-law read
/// The pure body: near-law lines arrive as a parameter, and the tests pin it
/// directly so they stay hermetic against the machine index.
fn law_answer_with(req: &LawRequest, near: Vec<String>) -> LawAnswer {
    let law_tokens = tokens(req.law.subject.as_deref().unwrap_or(""));
    let mut cands: Vec<(usize, &OpenQuestion, Vec<String>)> = Vec::new();
    for q in &req.questions {
        let q_subject = q.subject.as_deref().unwrap_or("");
        let shared: Vec<String> = tokens(q_subject)
            .intersection(&law_tokens)
            .cloned()
            .collect();
        let mut is_candidate = !shared.is_empty();
        if !is_candidate {
            let question_lower = q.question.to_lowercase();
            let words: BTreeSet<&str> = question_lower
                .split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|t| !t.is_empty())
                .collect();
            let law_subject_lower = req.law.subject.as_deref().unwrap_or("").to_lowercase();
            let parts: Vec<&str> = law_subject_lower.split('-').collect();
            is_candidate = parts.len() >= 2 && parts.iter().all(|p| words.contains(p));
        }
        if is_candidate {
            cands.push((shared.len(), q, shared));
        }
    }
    cands.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.ts.cmp(&a.1.ts)));
    let total = cands.len();
    // Near-law warnings ride at the FRONT of lines: the recording session
    // reads them before the open-question sweep, because the point is to
    // stop a duplicate BEFORE it is repeated, not to route it after.
    let mut lines: Vec<String> = near;
    for (_, q, _) in cands.iter().take(10) {
        let law_id = &req.law.decision_id;
        let mut ident: Vec<String> = Vec::new();
        if let Some(a) = &q.asker {
            ident.push(format!("asker {}", a));
        }
        if let Some(n) = &q.node {
            ident.push(format!("node {}", n));
        }
        let ident = if ident.is_empty() {
            String::new()
        } else {
            format!(" ({})", ident.join(", "))
        };
        let q100 = one_line(&q.question, 100);
        let line = match q.session_id.as_deref().or(q.asker.as_deref()) {
            Some(to) => format!(
                "law: {law_id} may answer open {qid}{ident}: \"{q100}\" If it does, tell the asker: fno agents mail send {to} \"Law {law_id} may answer your question {qid}. Read it: fno inbox decisions {law_id}. If it answers you, act on it and withdraw {qid}.\"",
                law_id = law_id,
                qid = q.id,
                ident = ident,
                q100 = q100,
                to = to,
            ),
            None => format!(
                "law: {law_id} may answer open {qid}{ident}: \"{q100}\" nobody to tell; the operator can answer it: fno inbox outstanding clear {qid} --answer \"per {law_id}\"",
                law_id = law_id,
                qid = q.id,
                ident = ident,
                q100 = q100,
            ),
        };
        lines.push(line);
    }
    if total > 10 {
        lines.push(format!(
            "law: {} more open question(s) may match {}, not shown.",
            total - 10,
            req.law.decision_id
        ));
    }
    LawAnswer {
        ok: true,
        candidates: cands
            .into_iter()
            .map(|(_, q, shared)| Candidate {
                question_id: q.id.clone(),
                asker: q.asker.clone(),
                session_id: q.session_id.clone(),
                node: q.node.clone(),
                subject: q.subject.clone(),
                shared,
            })
            .collect(),
        total,
        lines,
    }
}

/// The four review verb names `hooks/review-hold.sh:84` classifies as a
/// review. Named explicitly rather than pattern-matched, for the same reason
/// the hold does it that way: a substring rule on "review" would fire on
/// `code-review-attest` and every future skill that merely mentions one.
const REVIEW_VERBS: &[&str] = &["code-review", "review", "review-changes", "sigma-review"];

/// The stage table: a stage's keywords decide WHICH laws surface, never
/// their order. Measured against the live corpus 2026-09-14: `review` alone
/// finds 10 of 11 review laws (it misses the one filed under a node id);
/// adding `attest|findings|max_rounds` finds 11 of 11 with zero false
/// positives.
const STAGES: &[(&str, &[&str])] = &[
    ("review", &["review", "attest", "findings", "max_rounds"]),
    (
        "blueprint",
        &[
            "blueprint",
            "plan",
            "planning",
            "difficulty",
            "model",
            "subagent",
            "python",
            "crate",
            "port",
            "verb",
        ],
    ),
    (
        "target",
        &[
            "target", "execute", "worktree", "spawn", "review", "merge", "attest", "python",
            "crate", "port",
        ],
    ),
];

/// The non-review verb names each stage classifies, next to REVIEW_VERBS.
/// `/fno:execute` classifies as target: it runs a bound plan, the same
/// governed ground. Named explicitly for the same reason REVIEW_VERBS is.
const STAGE_VERBS: &[(&str, &str)] = &[
    ("blueprint", "blueprint"),
    ("target", "target"),
    ("execute", "target"),
];

/// Normalize a skill invocation to the bare verb: strip one leading `/` or
/// `$`, cut at the first whitespace, keep the text after the last `:`. The
/// order is the one `review-hold.sh:71-76` documents, so a colon inside a
/// PR URL argument never eats the verb.
fn normalize_verb(raw: &str) -> String {
    let stripped = raw
        .strip_prefix('/')
        .or_else(|| raw.strip_prefix('$'))
        .unwrap_or(raw);
    let head = stripped.split_whitespace().next().unwrap_or("");
    match head.rsplit_once(':') {
        Some((_, tail)) => tail.to_string(),
        None => head.to_string(),
    }
}

/// The stage classifier's input: the raw verb-carrying token and the full
/// text a node id may ride in (skill name plus args, or the whole prompt).
fn stage_input(hook: &Value) -> Option<(String, String)> {
    let tool = hook.get("tool_name").and_then(Value::as_str).unwrap_or("");
    if tool == "Skill" {
        let input = hook.get("tool_input");
        let read = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str);
        let name = read("skill")
            .or_else(|| read("name"))
            .or_else(|| read("command"))?;
        let args = read("args").unwrap_or("");
        Some((name.to_owned(), format!("{name} {args}")))
    } else {
        let prompt = hook.get("prompt").and_then(Value::as_str)?;
        let head = prompt.split_whitespace().next()?.to_owned();
        Some((head, prompt.to_owned()))
    }
}

/// The first node-id-shaped token in the stage input's full text, lowercased.
/// A payload with no node id carries no node subjects to match.
fn payload_node_id(hook: &Value) -> Option<String> {
    let (_, text) = stage_input(hook)?;
    text.split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        .find(|t| matches_node_id_shape(&t.to_lowercase()))
        .map(|t| t.to_lowercase())
}

/// The stage classifier: a Skill tool call reads the skill name under the
/// keys the harness versions use; any other event reads the prompt's first
/// whitespace-separated token.
fn classify_stage(hook: &Value) -> Option<&'static str> {
    let (raw, _) = stage_input(hook)?;
    let verb = normalize_verb(&raw);
    REVIEW_VERBS
        .contains(&verb.as_str())
        .then_some("review")
        .or_else(|| {
            STAGE_VERBS
                .iter()
                .find(|(v, _)| *v == verb)
                .map(|(_, s)| *s)
        })
}

/// The law's first sentence, split at `". "`, `"! "` or `"? "`.
fn first_sentence(text: &str) -> &str {
    let mut end = text.len();
    for sep in [". ", "! ", "? "] {
        if let Some(pos) = text.find(sep) {
            end = end.min(pos);
        }
    }
    &text[..end]
}

/// One law line: `- <id> (<subject>, <scope>): <first sentence, 160 chars>`.
/// The scope rides the line so a reader tells a global ruling from a local
/// one without a second command.
fn stage_law_line(row: &Value) -> Option<String> {
    let id = row.get("decision_id").and_then(Value::as_str)?;
    let subject = row.get("subject").and_then(Value::as_str).unwrap_or("");
    let decision = row.get("decision").and_then(Value::as_str).unwrap_or("");
    Some(format!(
        "- {id} ({subject}, {}): {}",
        decision_index::row_scope(row),
        one_line(first_sentence(decision), 160)
    ))
}

/// Which laws a stage block lists: the node's project scope as a FIELD (never
/// the old substring heuristic, where any law mentioning the slug matched any
/// node of that project), then a keyword match over `{subject} {decision}` or
/// the payload node's subjects (node id, epic id) named in either field - minus
/// rows whose subject equals the node id itself (the think-inspect receipt
/// already carries the node's own rulings).
fn stage_matching_lines(
    index: &decision_index::Index,
    keywords: &[&str],
    idents: &[String],
    node_id: &str,
    project: Option<&str>,
) -> Vec<String> {
    index
        .rows
        .iter()
        .filter(|row| {
            let subject = row.get("subject").and_then(Value::as_str).unwrap_or("");
            // The node id is carried separately: `idents` is sorted, so its
            // first element is whichever subject sorts smallest, never
            // reliably the node id.
            let node_row = !node_id.is_empty() && subject.trim().eq_ignore_ascii_case(node_id);
            !node_row
        })
        // `None` fails open - the reason rides the unread receipt - because a
        // missing law is worse than one that does not apply.
        .filter(|row| project.map_or(true, |slug| decision_index::row_in_scope(row, slug)))
        .filter_map(|row| {
            let subject = row.get("subject").and_then(Value::as_str).unwrap_or("");
            let decision = row.get("decision").and_then(Value::as_str).unwrap_or("");
            let haystack = format!("{subject} {decision}").to_lowercase();
            let hit = keywords.iter().any(|k| haystack.contains(k))
                || idents.iter().any(|i| haystack.contains(i));
            hit.then(|| stage_law_line(row)).flatten()
        })
        .collect()
}

/// Node subjects for a stage payload that names a node: the node id itself and
/// its epic id (the graph row's `parent`), plus the row's `project` field
/// returned SEPARATELY, because scope matches as a stamped field now, not as a
/// substring of law text. An unreadable graph degrades to node-id-only
/// matching and returns the reason so the stage answer can name the missing scope.
fn node_subject_idents(
    node_id: &str,
    graph_path: Option<&std::path::Path>,
) -> (Vec<String>, Option<String>, Option<String>) {
    let mut idents = vec![node_id.to_lowercase()];
    let default_path = crate::graph_get::default_graph_path();
    let path = graph_path.unwrap_or(&default_path);
    if graph_path.is_none() && crate::graph_get::external_backend_selected() {
        return (
            idents,
            None,
            Some("graph: external backend selected".to_owned()),
        );
    }
    let entries = match crate::backlog::api::rows(&crate::backlog::api::Store::new(path)) {
        Ok(entries) => entries,
        Err(error) => return (idents, None, Some(format!("graph: {}", error.0))),
    };
    let mut project = None;
    if let Some(entry) = crate::graph_get::find_entry(&entries, node_id) {
        if let Some(v) = entry.get("parent").and_then(Value::as_str) {
            let v = v.trim().to_lowercase();
            if !v.is_empty() {
                idents.push(v);
            }
        }
        if let Some(v) = entry.get("project").and_then(Value::as_str) {
            let v = v.trim().to_lowercase();
            if !v.is_empty() {
                project = Some(v);
            }
        }
    }
    idents.sort();
    idents.dedup();
    (idents, project, None)
}

/// The context block for a stage with laws: cap 2000 bytes, first law line
/// always renders, and every law past the cap keeps a short id line the
/// validator parses, so overflow law is acknowledged, not hidden.
fn render_stage_block(
    stage: &str,
    matching: &[String],
    damaged: usize,
    unread: &[String],
) -> String {
    let mut text = format!(
        "## Law governing {stage}\n\nThese live operator rulings govern the {stage} you are starting. Act inside them. Do not re-derive them.\n"
    );
    let mut rendered = 0usize;
    for line in matching {
        // The first law line always renders, cap or no cap.
        if rendered > 0 && text.len() + line.len() + 1 > 2000 {
            break;
        }
        text.push_str(line);
        text.push('\n');
        rendered += 1;
    }
    // Newest first, like the full lines. The old single `- and N more` line
    // carried no ids, so the stage-law ack check in validate-plan.sh could
    // never see the laws the cap had cut.
    for line in &matching[rendered..] {
        if let Some(short) = short_law_line(line) {
            text.push_str(&short);
            text.push('\n');
        }
    }
    text.push_str(&render_read_receipt(damaged, unread));
    text
}

fn render_read_receipt(damaged: usize, unread: &[String]) -> String {
    let mut text = String::new();
    if damaged > 0 {
        text.push_str(&format!(
            "{} index row(s) could not be parsed, so this list may be incomplete.\n",
            damaged
        ));
    }
    for reason in unread {
        text.push_str(&format!("Unread: {reason}\n"));
    }
    text
}

/// `- <id> (<subject>): fno backlog decisions <id>`, cut from a full stage
/// line at its first `): `. Laws the 2000-byte cap could not summarize still
/// get a line matching the validator's `- <id> (<subject>):` shape.
fn short_law_line(full: &str) -> Option<String> {
    let rest = full.strip_prefix("- ")?;
    let cut = rest.find("): ")?;
    let head = &rest[..cut];
    let id = head.split(" (").next()?;
    Some(format!("- {head}): fno backlog decisions {id}"))
}

/// The stage answer. A readable index with zero matching laws renders
/// nothing (`hook_output: null`), which is the correct answer for that
/// input; a failed read is a report, never silence. A request carrying edit
/// targets (`paths`) answers from the edit read instead of the verb
/// classifier.
fn stage_answer_with(
    req: StageRequest,
    index_path: Option<&std::path::Path>,
    graph_path: Option<&std::path::Path>,
) -> Value {
    if !req.paths.is_empty() {
        return edit_answer(req, index_path, None);
    }
    let stage = classify_stage(&req.hook);
    let mut hook_output = None;
    let mut unread = Vec::new();
    if let Some(stage_name) = stage {
        let keywords = STAGES
            .iter()
            .find(|(s, _)| *s == stage_name)
            .map(|(_, k)| *k)
            .unwrap_or(&[]);
        let node_id = payload_node_id(&req.hook);
        let (idents, node_project, graph_unread) = node_id
            .as_deref()
            .map(|id| node_subject_idents(id, graph_path))
            .unwrap_or_default();
        if let Some(reason) = &graph_unread {
            unread.push(format!("the node's epic and project ({reason})"));
        }
        // The scope is the NODE's project field: the same stamp the write door
        // uses, so a stage block and a decisions report cannot disagree about
        // one row. A node row without the field falls back to the cwd resolver
        // once; every other unresolvable shape fails OPEN - quietly for a
        // nodeless hook, and with the graph reason already on the receipt when
        // the graph read itself failed.
        let scope_project = match (node_id.as_deref(), node_project.as_deref()) {
            (_, Some(slug)) => Some(slug.to_owned()),
            (Some(_), None) => match graph_unread {
                Some(_) => None,
                None => match resolve_project(None, &settings_sources()) {
                    Ok(slug) => Some(slug),
                    Err(reason) => {
                        unread.push(format!("the node's project ({reason}; scope filter open)"));
                        None
                    }
                },
            },
            (None, _) => None,
        };
        let laws = match index_path {
            Some(p) => decision_index::live_laws(p),
            // The default path is the STORE read: graph.db plus the JSONL
            // rows the db lacks. A JSONL default refused d-608344c1, a live
            // law cited across the fleet, while graph.db held 95 laws to the
            // JSONL's 9.
            None => decision_index::default_store_live().map(decision_index::laws_of),
        };
        match laws {
            Ok(index) => {
                let matching = stage_matching_lines(
                    &index,
                    keywords,
                    &idents,
                    node_id.as_deref().unwrap_or(""),
                    scope_project.as_deref(),
                );
                if !matching.is_empty() || !unread.is_empty() || index.damaged > 0 {
                    let additional_context = if matching.is_empty() {
                        render_read_receipt(index.damaged, &unread)
                    } else {
                        render_stage_block(stage_name, &matching, index.damaged, &unread)
                    };
                    hook_output = Some(json!({
                        "hookSpecificOutput": {
                            "hookEventName": req.hook.get("hook_event_name").cloned().unwrap_or(Value::Null),
                            "additionalContext": additional_context,
                        }
                    }));
                }
            }
            Err(reason) => {
                unread.push(format!("the decision index ({reason})"));
                let text = format!(
                    "The decision index could not be read ({reason}), so the rulings that govern this {stage_name} are unknown. Run fno backlog decisions --lane law --state live before you act on {stage_name} policy.\n{}",
                    render_read_receipt(0, &unread)
                );
                hook_output = Some(json!({
                    "hookSpecificOutput": {
                        "hookEventName": req.hook.get("hook_event_name").cloned().unwrap_or(Value::Null),
                        "additionalContext": text,
                    }
                }));
            }
        }
    }
    json!({"ok": true, "stage": stage, "hook_output": hook_output, "unread": unread})
}

/// Comma-joined flag values read as separate globs: split on commas, trim,
/// drop empties.
fn normalize_paths(raw: &[String]) -> Vec<String> {
    raw.iter()
        .flat_map(|p| p.split(','))
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A law names repo paths: a glob is refused when it is absolute or holds a
/// `..` component.
fn glob_is_repo_relative(glob: &str) -> bool {
    !glob.starts_with('/') && glob.split('/').all(|component| component != "..")
}

/// The state root the edit seen-set lives under: `$FNO_HOME`, else `~/.fno`
/// (the shared `default_state_path` resolution, taken at its parent).
fn default_state_root() -> std::path::PathBuf {
    decision_index::default_state_path("law-edit-seen")
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// The paths a row names. A row with no non-empty `paths` array never
/// matches the edit read.
fn row_paths(row: &Value) -> Vec<String> {
    row.get("paths")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The edit read: the files a PreToolUse Edit|Write payload is about to
/// change, matched against live laws that name paths (`paths` globs on the
/// row, matched with the crate's fnmatch, where `*` crosses `/`). The read
/// prints once per session per decision id, keyed in
/// `law-edit-seen/<session>.json`; a payload with no `session_id` prints
/// every time. An unreadable index is a report naming this edit, never
/// silence, and zero surviving laws render nothing.
fn edit_answer(
    req: StageRequest,
    index_path: Option<&std::path::Path>,
    state_root: Option<&std::path::Path>,
) -> Value {
    // The hook sends one target per array element already; comma-splitting
    // here would corrupt a legal path that contains a comma. The record door
    // owns the comma-joined flag form and normalizes at its own edge.
    let targets: Vec<String> = req
        .paths
        .iter()
        .map(String::as_str)
        .map(str::to_owned)
        .collect();
    let hook_event = req
        .hook
        .get("hook_event_name")
        .cloned()
        .unwrap_or(Value::Null);
    let laws = match index_path {
        Some(p) => decision_index::live_laws(p),
        None => decision_index::default_store_live().map(decision_index::laws_of),
    };
    let hook_output = match laws {
        Ok(index) => {
            let mut hits: Vec<&Value> = index
                .rows
                .iter()
                .filter(|row| {
                    let globs = row_paths(row);
                    !globs.is_empty()
                        && globs.iter().any(|glob| {
                            targets
                                .iter()
                                .any(|t| crate::sync_canonical::fnmatch(t, glob))
                        })
                })
                .collect();
            let session_id = req.hook.get("session_id").and_then(Value::as_str);
            if let Some(session_id) = session_id {
                let fallback;
                let root = match state_root {
                    Some(p) => p,
                    None => {
                        fallback = default_state_root();
                        &fallback
                    }
                };
                let mut seen = crate::announce::load_cursor(root, "law-edit-seen", session_id);
                hits.retain(|row| {
                    let id = row.get("decision_id").and_then(Value::as_str).unwrap_or("");
                    !seen.contains(id)
                });
                if !hits.is_empty() {
                    for row in &hits {
                        if let Some(id) = row.get("decision_id").and_then(Value::as_str) {
                            seen.insert(id.to_owned());
                        }
                    }
                    crate::announce::save_cursor(root, "law-edit-seen", session_id, &seen);
                }
            }
            let matching: Vec<String> = hits.iter().filter_map(|row| stage_law_line(row)).collect();
            (!matching.is_empty()).then(|| {
                json!({
                    "hookSpecificOutput": {
                        "hookEventName": hook_event,
                        "additionalContext": render_stage_block("edit", &matching, index.damaged, &[]),
                    }
                })
            })
        }
        Err(reason) => Some(json!({
            "hookSpecificOutput": {
                "hookEventName": hook_event,
                "additionalContext": format!(
                    "The decision index could not be read ({reason}), so the rulings that govern this edit are unknown. Run `fno backlog decisions --lane law --state live` before you act on the files you are changing.\n"
                ),
            }
        })),
    };
    json!({"ok": true, "stage": "edit", "hook_output": hook_output, "unread": []})
}

/// The statement validator, a word-for-word port of
/// `validate_durable_law` (`cli/src/fno/law.py`) plus the one rule that
/// module cannot own: a bare node id or `pr-<n>` subject is refused, because
/// a ruling found only by the id of the work that prompted it is unfindable.
fn validate_answer(req: &ValidateRequest) -> Value {
    let refusal = if req.subject.trim().is_empty() || req.decision.trim().is_empty() {
        Some("subject and decision are required".to_string())
    } else if req.subject.trim().chars().count() < 2 || req.decision.trim().chars().count() < 2 {
        // A one-character subject or decision is a placeholder, not a
        // statement: a 2026-08-29 smoke run of this verb with x/y/z landed a
        // live law nobody could act on or clear. The gate refuses the shape
        // wherever it is invoked from, live session or suite.
        Some(
            "subject and decision must be more than one character: a single \
             letter is a placeholder, not law"
                .to_string(),
        )
    } else if req
        .rationale
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        Some("rationale is required for durable law".to_string())
    } else {
        let lowered = req.decision.to_lowercase();
        if COORDINATION_MARKERS.iter().any(|m| lowered.contains(m)) {
            Some("the statement is coordination, not durable law".to_string())
        } else if let Some(sup) = req.supersedes.as_deref() {
            if !is_decision_id(sup) {
                Some("supersedes must be a decision id".to_string())
            } else if matches_node_id_shape(req.subject.trim().to_lowercase().as_str())
                || is_pr_subject(req.subject.trim().to_lowercase().as_str())
            {
                Some(format!(
                    "a node or PR id is not a law subject. Law is found by topic: name the topic, for example review-rounds, and cite {} in the decision text",
                    req.subject.trim()
                ))
            } else {
                None
            }
        } else if matches_node_id_shape(req.subject.trim().to_lowercase().as_str())
            || is_pr_subject(req.subject.trim().to_lowercase().as_str())
        {
            Some(format!(
                "a node or PR id is not a law subject. Law is found by topic: name the topic, for example review-rounds, and cite {} in the decision text",
                req.subject.trim()
            ))
        } else {
            None
        }
    };
    json!({"ok": true, "refusal": refusal})
}

/// The coordination markers of `law.py:18-25`, moved with the validator.
const COORDINATION_MARKERS: &[&str] = &[
    "this pr",
    "this node",
    "this target",
    "temporary",
    "until merge",
    "for this change",
];

/// `^d-[0-9a-f]{8}$`, the shape `law.py` enforced on `--supersedes`.
fn is_decision_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("d-") else {
        return false;
    };
    rest.len() == 8 && rest.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `^[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}$`, the node-id shape
/// `parse-claims-arg.sh` uses. Measured 2026-09-14: the shape matches 3 of
/// 59 live law subjects, and all 3 are real node ids.
fn matches_node_id_shape(s: &str) -> bool {
    let Some((prefix, suffix)) = s.split_once('-') else {
        return false;
    };
    let p = prefix.as_bytes();
    if p.is_empty() || p.len() > 8 || !p[0].is_ascii_lowercase() {
        return false;
    }
    if !p[1..]
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return false;
    }
    let sfx = suffix.as_bytes();
    (4..=8).contains(&sfx.len()) && sfx.iter().all(|b| b.is_ascii_hexdigit())
}

/// `^pr-[0-9]+$`.
fn is_pr_subject(s: &str) -> bool {
    match s.strip_prefix("pr-") {
        Some(rest) => !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// Mirror of `fno.agents.discover.resolve_project_for_cwd`, the settings
/// resolver whose answer is the graph node's `project` vocabulary. NOT
/// `worktree_paths.resolve_project_id`, whose git-remote basename answers
/// `footnote` where every node says `fno`: stamp and node must agree or the
/// stage matcher and the filter answer different questions about one row.
/// Adds one rung the Python resolver lacks: fno-managed worktrees at
/// `~/.fno/worktrees/<repo>/<name>`, attributed through the `<repo>` segment
/// like the conductor layout, so a law recorded from a worktree session
/// stamps the parent repo's project instead of refusing.
fn resolve_project(
    cwd: Option<&std::path::Path>,
    sources: &[std::path::PathBuf],
) -> Result<String, String> {
    let cwd = match cwd {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().map_err(|e| format!("cwd unreadable ({e})"))?,
    };
    let p = cwd.to_string_lossy().to_string();
    let sep = std::path::MAIN_SEPARATOR;
    // <root>/.claude/worktrees/<name> attributes to <root>, then a direct
    // settings match on the root itself.
    let claude_marker = format!("{sep}.claude{sep}worktrees{sep}");
    if let Some((root, _)) = p.split_once(&claude_marker) {
        if !root.is_empty() {
            if let Some(slug) = project_in_sources(std::path::Path::new(root), sources) {
                return Ok(slug);
            }
        }
    }
    // ~/.fno/worktrees/<repo>/... and conductor workspaces/<repo>/... map the
    // repo segment through the settings basename.
    for marker in ["/.fno/worktrees/", "/workspaces/"] {
        if let Some((_, rest)) = p.split_once(marker) {
            if let Some(repo) = rest.split(sep).next().filter(|s| !s.is_empty()) {
                if let Some(slug) = project_by_repo_basename(repo, sources) {
                    return Ok(slug);
                }
            }
        }
    }
    project_in_sources(&cwd, sources).ok_or_else(|| {
        format!(
            "no work.workspaces project names {}; add it to the work map",
            cwd.display()
        )
    })
}

/// The direct settings match: the first candidate file whose work map names
/// this exact path. Missing and malformed files contribute nothing, mirroring
/// `detect_project_from_settings`.
fn project_in_sources(target: &std::path::Path, sources: &[std::path::PathBuf]) -> Option<String> {
    let want = target.to_string_lossy().trim_end_matches('/').to_string();
    for (name, path) in iter_settings_projects(sources) {
        if path == want {
            return Some(name);
        }
    }
    None
}

fn project_by_repo_basename(repo: &str, sources: &[std::path::PathBuf]) -> Option<String> {
    iter_settings_projects(sources)
        .into_iter()
        .find(|(_, path)| {
            std::path::Path::new(path)
                .file_name()
                .map_or(false, |base| base == repo)
        })
        .map(|(name, _)| name)
}

/// The work map: `(name, normalized path)` pairs from the candidate settings
/// files, multi-workspace first then legacy flat, mirroring
/// `fno.agents.discover._iter_settings_projects`.
fn iter_settings_projects(sources: &[std::path::PathBuf]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for source in sources {
        let Some(text) = std::fs::read_to_string(source).ok() else {
            continue;
        };
        let is_toml = source.extension().map_or(false, |e| e == "toml");
        // Both formats funnel into one serde_json::Value so the walk below
        // reads one type. A whole-document serde conversion would drop the
        // file over one TOML datetime anywhere in it, so toml converts
        // value-by-value with datetimes stringified.
        let parsed = if is_toml {
            toml::from_str::<toml::Value>(&text).ok().map(toml_to_json)
        } else {
            serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text)
                .ok()
                .and_then(|v| serde_json::to_value(v).ok())
        };
        let work = parsed.as_ref().and_then(|v| v.get("work").cloned());
        let Some(work) = work else { continue };
        if let Some(workspaces) = work.get("workspaces").and_then(Value::as_object) {
            for ws in workspaces.values() {
                let Some(projects) = ws.get("projects").and_then(Value::as_array) else {
                    continue;
                };
                for proj in projects {
                    if let (Some(name), Some(path)) = (
                        proj.get("name").and_then(Value::as_str),
                        proj.get("path").and_then(Value::as_str),
                    ) {
                        out.push((name.to_string(), expand_tilde(path)));
                    }
                }
            }
        }
        if let Some(flat) = work.get("projects").and_then(Value::as_object) {
            for (name, cfg) in flat {
                if let Some(path) = cfg.get("path").and_then(Value::as_str) {
                    out.push((name.to_string(), expand_tilde(path)));
                }
            }
        }
    }
    out
}

/// toml::Value -> serde_json::Value, value by value. A TOML datetime (legal
/// anywhere in a config) has no JSON form, so it stringifies instead of
/// failing the whole document.
fn toml_to_json(value: toml::Value) -> Value {
    match value {
        toml::Value::String(s) => json!(s),
        toml::Value::Integer(i) => json!(i),
        toml::Value::Float(f) => json!(f),
        toml::Value::Boolean(b) => json!(b),
        toml::Value::Datetime(d) => json!(d.to_string()),
        toml::Value::Array(items) => Value::Array(items.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => Value::Object(
            table
                .into_iter()
                .map(|(k, v)| (k, toml_to_json(v)))
                .collect(),
        ),
    }
}

/// `~/x` -> `$HOME/x`; everything else verbatim. Matches the Python
/// expanduser + normpath shape closely enough for path equality.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::Path::new(&home)
                .join(rest)
                .to_string_lossy()
                .to_string();
        }
    }
    path.to_string()
}

/// Settings sources, nearest first: project-local `.fno/config.toml` then
/// `.fno/settings.yaml`, then the global pair (honoring
/// `FNO_GLOBAL_SETTINGS_PATH`, like `config_read_candidates`). The
/// `work.workspaces` map lives in the global file, so the global candidates
/// are what make resolution work at all.
fn settings_sources() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd.join(".fno/config.toml"));
        out.push(cwd.join(".fno/settings.yaml"));
    }
    let global_dir = match std::env::var_os("FNO_GLOBAL_SETTINGS_PATH") {
        // The redirect names the global settings FILE; its siblings win too.
        Some(p) => std::path::PathBuf::from(p)
            .parent()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(".")),
        None => {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            home.unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".fno")
        }
    };
    out.push(global_dir.join("config.toml"));
    out.push(global_dir.join("settings.yaml"));
    out
}

/// The law door's scope answer: `global` by explicit flag, else the recording
/// project. The door fails closed: an unresolvable project is a refusal that
/// names what failed, because a row stamped with a guessed project is worse
/// than a row not written.
fn record_scope_answer(req: RecordScopeRequest) -> Value {
    record_scope_answer_in(None, &settings_sources(), req.r#global, &req.paths)
}

fn record_scope_answer_in(
    cwd: Option<&std::path::Path>,
    sources: &[std::path::PathBuf],
    is_global: bool,
    raw_paths: &[String],
) -> Value {
    // The globs ride the door's validation: split, trim, drop empties, then
    // refuse any glob that is absolute or holds `..` - a law names repo
    // paths, and an absolute or escaping glob would match outside the repo.
    let paths = normalize_paths(raw_paths);
    for glob in &paths {
        if !glob_is_repo_relative(glob) {
            return json!({
                "ok": false,
                "refusal": format!(
                    "not a repo-relative glob: {glob} (a law names repo paths: no leading /, no ..)"
                )
            });
        }
    }
    if is_global {
        if paths.is_empty() {
            return json!({"ok": true, "scope": "global"});
        }
        return json!({"ok": true, "scope": "global", "paths": paths});
    }
    match resolve_project(cwd, sources) {
        Ok(slug) if paths.is_empty() => json!({"ok": true, "scope": format!("project:{slug}")}),
        Ok(slug) => json!({"ok": true, "scope": format!("project:{slug}"), "paths": paths}),
        Err(reason) => json!({
            "ok": false,
            "refusal": format!("no project stamps this law ({reason}); pass --global to widen")
        }),
    }
}

/// The `list_decisions` scope filter: law-lane rows outside the session's
/// project hide, `global` stays, an absent scope reads `project:fno` (every
/// pre-scope row was recorded in fno). An unresolvable project fails open
/// with a renderable note, never silence.
fn scope_split_answer(req: ScopeSplitRequest) -> Value {
    scope_split_answer_in(None, &settings_sources(), req)
}

fn scope_split_answer_in(
    cwd: Option<&std::path::Path>,
    sources: &[std::path::PathBuf],
    req: ScopeSplitRequest,
) -> Value {
    let project = resolve_project(cwd, sources);
    let mut kept = Vec::new();
    let mut hidden = 0usize;
    match project {
        Ok(slug) => {
            for row in req.rows {
                let is_law = row.get("lane").and_then(Value::as_str) == Some("law");
                if is_law && !decision_index::row_in_scope(&row, &slug) {
                    hidden += 1;
                } else {
                    kept.push(row);
                }
            }
            let note = if hidden > 0 {
                format!(" (hid {hidden} out-of-scope)")
            } else {
                String::new()
            };
            json!({"ok": true, "kept": kept, "hidden": hidden, "note": note})
        }
        // Fail open with a named reason: losing a law is worse than seeing
        // one that does not apply (d-0fa92eb9's posture). The reason rides
        // stderr, not the note, so a caller's labels stay byte-stable.
        Err(reason) => {
            eprintln!("fno inbox law: project unresolvable ({reason}); nothing hidden");
            json!({"ok": true, "kept": req.rows, "hidden": 0, "note": ""})
        }
    }
}

// ---------------------------------------------------------------------------
// The record door: the `record` mode carrying the `fno inbox law set` argv
//
// The Python `fno inbox law set` command is a shim that forwards its argv
// here, so these are the door's real flags: --global and --paths live on the
// Rust side and the typer option ratchet stays at zero for the shim. The
// gate order and every refusal text mirror `record_command` + the law-door
// path of `record_decision` (cli/src/fno/law.py, cli/src/fno/decide/__init__.py)
// one for one: the authority gate is law, and a drifted refusal is a second
// law.
// ---------------------------------------------------------------------------

const WAIVER_SUBJECT_PREFIX: &str = "review-coverage-waiver";

/// The law-set argv, parsed natively.
pub(crate) struct RecordDoor {
    subject: String,
    decision: Option<String>,
    decision_file: Option<String>,
    rationale: Option<String>,
    options: Vec<String>,
    supersedes: Option<String>,
    graduation: Option<String>,
    graduation_ref: Option<String>,
    reads: Vec<String>,
    is_global: bool,
    raw_paths: Vec<String>,
}

const RECORD_USAGE: &str = "usage: fno inbox law set <subject> [decision] [--decision-file f|-] [--rationale s] [--option s]... [--supersedes d-x] [--graduation k] [--graduation-ref r] [--read cmd]... [--global] [--paths glob,glob]";

fn parse_record_door(args: &[String]) -> Result<RecordDoor, String> {
    let mut door = RecordDoor {
        subject: String::new(),
        decision: None,
        decision_file: None,
        rationale: None,
        options: Vec::new(),
        supersedes: None,
        graduation: None,
        graduation_ref: None,
        reads: Vec::new(),
        is_global: false,
        raw_paths: Vec::new(),
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
                .ok_or_else(|| format!("{flag} needs a value\n{RECORD_USAGE}"))
        };
        match flag.as_str() {
            "--decision-file" => door.decision_file = Some(take(&mut i)?),
            "--rationale" => door.rationale = Some(take(&mut i)?),
            "--option" => door.options.push(take(&mut i)?),
            "--supersedes" => door.supersedes = Some(take(&mut i)?),
            "--graduation" => door.graduation = Some(take(&mut i)?),
            "--graduation-ref" => door.graduation_ref = Some(take(&mut i)?),
            "--read" => door.reads.push(take(&mut i)?),
            "--paths" => door.raw_paths.push(take(&mut i)?),
            "--global" => {
                // An inline value parses like a bool flag should: --global=false
                // must never widen because the value was ignored.
                door.is_global = match inline.as_deref() {
                    None => true,
                    Some(v) => matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
                };
            }
            f if f.starts_with('-') && f != "-" => {
                return Err(format!("no such option: {f}\n{RECORD_USAGE}"));
            }
            _ => {
                positional += 1;
                if positional == 1 {
                    door.subject = args[i].clone();
                } else if positional == 2 {
                    door.decision = Some(args[i].clone());
                } else {
                    return Err(format!("too many positional arguments\n{RECORD_USAGE}"));
                }
            }
        }
        i += 1;
    }
    if positional == 0 {
        return Err(format!("subject is required\n{RECORD_USAGE}"));
    }
    Ok(door)
}

fn mint_decision_id() -> String {
    // 'd-<hex>', matching decide/__init__.py::mint_decision_id (8 hex chars).
    let mut buf = [0u8; 4];
    if getrandom::fill(&mut buf).is_err() {
        // Fallback entropy: pid + clock. A collision costs one duplicate id.
        let seed = (std::process::id() as u64) << 32
            | std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(0);
        buf.copy_from_slice(&seed.to_le_bytes()[..4]);
    }
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("d-{hex}")
}

fn attended_terminal() -> bool {
    // A positive marker, not an absence (the Python doc states the limit
    // plainly: a tty is obtainable; this raises the cost of forging the
    // superuser lane and never stands alone).
    unsafe { libc::isatty(0) == 1 }
}

/// The trusted columns: (decided_by, attested_by, relayed_by). Port of
/// `_resolve_decider` for the law door (no `decided_by` claim, no origin):
/// a resolved session stamps its handle; an attended terminal records
/// "operator" and marks attested_by; the unattributed state never reaches
/// here because `require_marked_caller` refused it.
/// The resolved caller: the authority lane plus the provenance columns,
/// derived from ONE identity resolution. Port of require_marked_caller +
/// `_resolve_decider` composed, so the two can never disagree.
pub(crate) struct Caller {
    authority: String,
    decided_by: String,
    attested_by: Option<String>,
    relayed_by: Option<String>,
}

/// Resolve the caller from process truth: the ancestry prover first, the
/// attended terminal second, the fail-closed refusal last.
pub(crate) fn resolve_caller() -> Result<Caller, String> {
    let get = |name: &str| std::env::var(name).ok();
    let ident = crate::spawn_context::resolve_self_identity(
        &get,
        None,
        None,
        &crate::paths::AgentsHome::from_env(),
    );
    if let (Some(session_id), Some(_harness)) = (&ident.session_id, &ident.harness) {
        let handle = crate::identity::canonical_handle(session_id);
        if !handle.is_empty() {
            return Ok(Caller {
                authority: "chat_attested".to_string(),
                decided_by: handle,
                attested_by: None,
                relayed_by: None,
            });
        }
    }
    if attended_terminal() {
        return Ok(Caller {
            authority: "operator".to_string(),
            decided_by: "operator".to_string(),
            attested_by: Some("operator".to_string()),
            relayed_by: None,
        });
    }
    Err("no session identity and no terminal, so nothing here marks a decider".to_string())
}

#[cfg(test)]
impl Caller {
    fn as_authority(authority: &str) -> Caller {
        Caller {
            authority: authority.to_string(),
            decided_by: if authority == "operator" {
                "operator".to_string()
            } else {
                "testf4c6".to_string()
            },
            attested_by: (authority == "operator").then(|| "operator".to_string()),
            relayed_by: None,
        }
    }
}

/// Port of `graduation.validate_graduation` + `graduation_or_guidance`:
/// an omitted declaration (both flags absent) defaults to honest guidance;
/// an empty `kind` with a reference is not a kind and refuses.
fn graduation_or_guidance(kind: Option<&str>, reference: Option<&str>) -> Result<Value, String> {
    match (kind, reference) {
        (None, None) => Ok(json!({"kind": "guidance"})),
        _ => validate_graduation(kind.unwrap_or("").trim(), reference.unwrap_or("").trim()),
    }
}

fn validate_graduation(kind: &str, reference: &str) -> Result<Value, String> {
    match kind {
        "" => Err(
            "graduation must be enforced, guidance, or should-be-enforced-but-i-did-not"
                .to_string(),
        ),
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
                "follow_up": reference.to_ascii_lowercase()
            }))
        }
        other => Err(format!(
            "graduation must be enforced, guidance, or should-be-enforced-but-i-did-not (got {other})"
        )),
    }
}

/// The newest recoverable row for a decision id, casefold-equal. Port of
/// `_decision_row_by_id` over the same store read.
fn find_decision_row(index: &decision_index::Index, decision_id: &str) -> Option<Value> {
    index
        .rows
        .iter()
        .filter(|row| {
            let etype = row.get("_event_type").and_then(Value::as_str);
            matches!(etype, None | Some("operator_decision"))
                && row
                    .get("decision_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .eq_ignore_ascii_case(decision_id)
        })
        .max_by_key(|row| {
            (
                row.get("ts")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                row.get("decision_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            )
        })
        .cloned()
}

/// The repo root the evidence gate resolves citations against: the
/// FNO_REPO_ROOT test hook, then the git toplevel, then the cwd.
fn evidence_repo_root() -> std::path::PathBuf {
    if let Some(root) = std::env::var_os("FNO_REPO_ROOT") {
        return std::path::PathBuf::from(root);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&cwd)
        .output()
    {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !text.is_empty() {
                return std::path::PathBuf::from(text);
            }
        }
    }
    cwd
}

/// The project journal beside the carveout ledger under the CANONICAL (main)
/// worktree: `resolve_carveout_root` + `events_path`. The canonical root
/// honors the FNO_REPO_ROOT test hook first, then the first `git worktree
/// list` row.
fn project_events_journal() -> std::path::PathBuf {
    if let Some(root) = std::env::var_os("FNO_REPO_ROOT") {
        return std::path::PathBuf::from(root)
            .join(".fno")
            .join("events.jsonl");
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let main_root = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(&cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .find_map(|l| l.strip_prefix("worktree ").map(str::to_string))
        })
        .map(std::path::PathBuf::from)
        .unwrap_or(cwd);
    main_root.join(".fno").join("events.jsonl")
}

/// Open questions the new law may answer: `read_open_questions`' fold over
/// the machine questions store, minus the closed rows. Best-effort: a missing
/// or malformed store reads as no questions, never an error.
fn read_open_questions() -> Vec<OpenQuestion> {
    let path = decision_index::default_state_path("questions.jsonl");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut asked: Vec<OpenQuestion> = Vec::new();
    let mut closed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in text.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let data = row.get("data");
        let id = data
            .and_then(|d| d.get("question_id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if id.is_empty() {
            continue;
        }
        match row.get("type").and_then(Value::as_str) {
            Some("operator_question") => {
                let d = data.and_then(|d| d.as_object());
                asked.push(OpenQuestion {
                    id: id.to_string(),
                    ts: d
                        .and_then(|d| d.get("ts"))
                        .or(row.get("ts"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    question: d
                        .and_then(|d| d.get("question"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    subject: None,
                    node: None,
                    asker: None,
                    session_id: None,
                })
            }
            Some("operator_question_closed") => {
                closed.insert(id.to_string());
            }
            _ => {}
        }
    }
    asked.retain(|q| !closed.contains(&q.id));
    asked
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn door_refuse(message: &str) -> i32 {
    eprintln!("fno law: refused: {message}. Nothing was recorded.");
    3
}

/// The door body, gate by gate in `record_command`'s order. Prints the
/// decision id on stdout alone; every refusal and hint rides stderr.
fn run_record_door(args: &[String], stdin_text: &str) -> i32 {
    let (door, decision) = match record_door_preflight(args, stdin_text) {
        Ok(pair) => pair,
        Err(code) => return code,
    };
    // The authority gate: law is never inherited by silence. The resolver
    // reads process truth (the ancestry prover), never env claims, so the
    // gate is not forgeable by environment.
    let caller = match resolve_caller() {
        Ok(c) => c,
        Err(message) => return door_refuse(&message),
    };
    record_door_write(door, decision, &caller)
}

/// The argv parse, the decision text, and the statement validator: the gates
/// that run before anyone asks who is calling. Split from the write so tests
/// can drive the gated write with an injected authority (the real gate reads
/// process ancestry and has no hermetic shape).
fn record_door_preflight(args: &[String], stdin_text: &str) -> Result<(RecordDoor, String), i32> {
    let door = match parse_record_door(args) {
        Ok(d) => d,
        Err(usage) => {
            eprintln!("fno inbox law set: {usage}");
            return Err(2);
        }
    };
    // The decision text: positional, a file, or stdin ('-').
    let decision: String = match door.decision_file.as_deref() {
        // '-' reads the CALLER's stdin, which the transport (the `fno inbox
        // law set` shim) carried in the request's stdin field.
        Some("-") => stdin_text.to_string(),
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => return Err(door_refuse(&format!("could not read {path} ({e})"))),
        },
        None => door.decision.clone().unwrap_or_default(),
    };
    // The statement validator, in-process (mode validate's own gate).
    let validate = validate_answer(&ValidateRequest {
        subject: door.subject.clone(),
        decision: decision.clone(),
        rationale: door.rationale.clone(),
        supersedes: door.supersedes.clone(),
    });
    if let Some(msg) = validate.get("refusal").and_then(Value::as_str) {
        return Err(door_refuse(msg));
    }
    Ok((door, decision))
}

/// The gated write: every gate from graduation through the stores, then the
/// sweep. `authority` is the RESOLVED caller lane (chat_attested | operator).
pub(crate) fn record_door_write(door: RecordDoor, decision: String, caller: &Caller) -> i32 {
    let authority = caller.authority.as_str();
    let graduation =
        match graduation_or_guidance(door.graduation.as_deref(), door.graduation_ref.as_deref()) {
            Ok(g) => g,
            Err(message) => return door_refuse(&message),
        };
    let Caller {
        authority: _,
        decided_by,
        attested_by,
        relayed_by,
    } = caller;
    // The scope stamp with the widening and the globs validated at the door.
    let scope_answer =
        record_scope_answer_in(None, &settings_sources(), door.is_global, &door.raw_paths);
    if scope_answer.get("ok") != Some(&json!(true)) {
        let message = scope_answer
            .get("refusal")
            .and_then(Value::as_str)
            .unwrap_or("no project to stamp under");
        return door_refuse(message);
    }
    let scope = scope_answer
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let paths: Vec<Value> = scope_answer
        .get("paths")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // The evidence gate: a measured claim carries the read that produced it.
    // Exempt when the RESOLVED authority is operator.
    let mut read_rows: Option<Vec<Value>> = None;
    if authority != "operator" {
        let text = format!("{decision}\n{}", door.rationale.as_deref().unwrap_or(""));
        let root = evidence_repo_root();
        let mut runner =
            |cmd: &str, root: &std::path::Path| crate::evidence::shell_run(cmd, root, 20);
        match crate::evidence::check_ruling_evidence(&text, &door.reads, &root, &mut runner) {
            Ok(rows) => read_rows = rows,
            Err(gate) => return door_refuse(&gate.message),
        }
    }
    // A waiver subject is operator-evidence-only.
    if (door.subject == WAIVER_SUBJECT_PREFIX
        || door
            .subject
            .starts_with(&format!("{WAIVER_SUBJECT_PREFIX}:")))
        && authority != "operator"
    {
        return door_refuse(&format!(
            "'{}' is a review-coverage waiver subject: waiver evidence needs \
             superuser authority, and a chat-attested row proves only that a \
             session was addressed, not that a person reviewed anything. \
             Waivers are recorded by the attended command \
             `fno do pr coverage-waive <pr> --reason \"...\"` at an operator \
             terminal; a session a harness identifies records nothing there.",
            door.subject
        ));
    }
    // Supersession: the target must be recoverable, and a chat recording may
    // retire its own kind, never the operator's.
    if let Some(sup) = &door.supersedes {
        let index = match decision_index::default_store_live() {
            Ok(i) => i,
            Err(reason) => {
                return door_refuse(&format!(
                    "the decision index could not be read ({reason}); \
                     supersession needs it first"
                ));
            }
        };
        let Some(row) = find_decision_row(&index, sup) else {
            return door_refuse(&format!(
                "supersession target {sup} is not recoverable from the \
                 decision index. Run `fno backlog decide-reindex` before retrying."
            ));
        };
        if decision_index::is_law(&row) && authority != "operator" && authority != "chat_attested" {
            return door_refuse(&format!(
                "agent {decided_by} cannot record under superuser authority"
            ));
        }
        if authority == "chat_attested"
            && row.get("authority_source").and_then(Value::as_str) == Some("operator")
        {
            return door_refuse(&format!(
                "agent {decided_by} cannot record under superuser authority"
            ));
        }
    }
    let decision_id = mint_decision_id();
    let ts = now_iso();
    // The envelope mirrors the Python `operator_decision` builder: a key
    // appears only when its value is set; text fields cap at 2000 chars.
    let mut data = json!({
        "decision_id": decision_id,
        "decision": text_cap(&decision, 2000),
        "authority_source": authority,
        "graduation": graduation,
    });
    if !door.subject.trim().is_empty() {
        data["subject"] = json!(door.subject.trim());
    }
    if !door.options.is_empty() {
        data["options"] = json!(door.options);
    }
    data["decided_by"] = json!(decided_by);
    if let Some(a) = &attested_by {
        data["attested_by"] = json!(a);
    }
    if let Some(r) = &relayed_by {
        data["relayed_by"] = json!(r);
    }
    if let Some(rationale) = &door.rationale {
        if !rationale.trim().is_empty() {
            data["rationale"] = json!(text_cap(rationale, 2000));
        }
    }
    if let Some(sup) = &door.supersedes {
        data["supersedes"] = json!(sup);
    }
    if let Some(rows) = &read_rows {
        data["reads"] = json!(rows);
    }
    if !scope.is_empty() {
        data["scope"] = json!(scope);
    }
    if !paths.is_empty() {
        data["paths"] = json!(paths);
    }
    let envelope = json!({"ts": ts, "type": "operator_decision", "source": "target", "data": data});
    // Durability first: the project journal. A failed write here records
    // nothing anywhere.
    let journal = project_events_journal();
    if let Err(e) = crate::event_store::append_envelope(&journal, &envelope.to_string(), None) {
        return door_refuse(&format!("the project journal write failed ({e})"));
    }
    // Recall second: the machine-wide decision index. The event id names the
    // recovery, because re-running would mint a second id for one ruling.
    let index_path = decision_index::default_state_path("decisions.jsonl");
    if let Err(e) = crate::event_store::append_envelope(&index_path, &envelope.to_string(), None) {
        eprintln!(
            "fno law: recorded {decision_id} to the project journal, but the \
             recall index write failed ({e}). Run `fno backlog decide-reindex`; \
             do not re-run the law command."
        );
        return 1;
    }
    // The graph decisions table is the store `fno backlog decisions` reads
    // first; a refusal degrades to the durable capture, never a lost ruling.
    let graph_path = crate::graph_get::default_graph_path();
    if let Err(e) = crate::backlog::api::decision_record(
        &crate::backlog::api::Store::new(&graph_path),
        envelope.clone(),
    ) {
        eprintln!(
            "decide: recorded {decision_id}, but the graph store refused the \
             ruling ({e:?}). The decision is durable in the journal and the index."
        );
    }
    if matches_node_id_shape(&door.subject.to_lowercase()) {
        // The node-view projection (`_project`) is Python-side and not ported;
        // the durable stores hold the row and every decisions read reaches it.
        eprintln!(
            "decide: recorded {decision_id}, but the node projection is not \
             ported to the record door yet; the decision is durable and \
             recoverable with `fno backlog decisions`."
        );
    }
    println!("{decision_id}");
    // The best-effort rule-time join, on stderr: near laws first, then the
    // open questions the new law may answer.
    let law_row = LawRow {
        decision_id: decision_id.clone(),
        subject: Some(door.subject.clone()),
        decision: Some(decision),
        ts: Some(ts),
        lane: None,
    };
    let near = near_law_lines(&law_row);
    let answer = law_answer_with(
        &LawRequest {
            law: law_row,
            questions: read_open_questions(),
        },
        near,
    );
    for line in &answer.lines {
        eprintln!("{line}");
    }
    0
}

/// Raw text cap: the first `cap` chars, Python `text[:cap]` semantics (no
/// newline collapse; the law row keeps its line breaks).
fn text_cap(text: &str, cap: usize) -> String {
    text.chars().take(cap).collect()
}

/// Near-law lines for a law being recorded: live laws on the same subject
/// (casefold equality) or a nearby subject (shared `tokens()`), at most 5,
/// newest first. A warning at record time, never a refusal.
fn near_law_lines_from(index: &decision_index::Index, law: &LawRow) -> Vec<String> {
    let new_id = law.decision_id.as_str();
    let new_subject = law.subject.as_deref().unwrap_or("").trim();
    let new_tokens = tokens(new_subject);
    let mut lines: Vec<String> = Vec::new();
    for row in &index.rows {
        let id = row.get("decision_id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() || id.eq_ignore_ascii_case(new_id) {
            continue;
        }
        let subject = row.get("subject").and_then(Value::as_str).unwrap_or("");
        let exact = subject.trim().eq_ignore_ascii_case(new_subject);
        let shared: Vec<String> = tokens(subject).intersection(&new_tokens).cloned().collect();
        if !exact && shared.is_empty() {
            continue;
        }
        let decision = row.get("decision").and_then(Value::as_str).unwrap_or("");
        lines.push(format!(
            "law: {new_id} sits near live law {id} ({subject}): {}. If it repeats that ruling, retract it: fno backlog decide-retract {new_id} --reason \"repeats {id}\". If it replaces that ruling, record it again with --supersedes {id}.",
            one_line(decision, 120)
        ));
    }
    lines.truncate(5);
    lines
}

/// The store-reading variant: an unreadable store is a one-line report, so a
/// recording against a damaged store still completes.
fn near_law_lines(law: &LawRow) -> Vec<String> {
    match decision_index::default_store_live() {
        Ok(index) => near_law_lines_from(&index, law),
        Err(reason) => vec![format!("law: near-law check skipped ({reason})")],
    }
}

/// The `fno inbox law match` transport: one JSON request on stdin, one JSON
/// answer on stdout, exit 0 whenever an answer was computed; exit 2 on
/// malformed args or an unreadable request.
pub fn run_law_match(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "usage: fno inbox law match (one JSON request on stdin: mode=ask|law|stage|validate|record-scope|scope-split|record; record takes argv: the fno inbox law set command line, and stdin: the text --decision-file - reads)"
        );
        return 0;
    }
    if !args.is_empty() {
        eprintln!("fno inbox law match: unexpected arguments; the request rides stdin");
        return 2;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("fno inbox law match: could not read stdin");
        return 2;
    }
    run_law_match_str(&input)
}

/// The request parser and dispatcher over an in-memory request. The `fno
/// inbox law` verbs on the fno front call this with their own text; the
/// record arm owns stdout (the decision id) and returns the door's exit
/// code, every other arm prints one JSON answer and exits 0.
pub fn run_law_match_str(input: &str) -> i32 {
    let req: MatchRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fno inbox law match: bad request: {e}");
            return 2;
        }
    };
    let answer = match req {
        MatchRequest::Record(r) => {
            // The door owns stdout and the exit code; no envelope here.
            return run_record_door(&r.argv, &r.stdin);
        }
        MatchRequest::Ask(r) => serde_json::to_string(&ask_answer(&r)).expect("serializes"),
        MatchRequest::Law(r) => {
            let near = near_law_lines(&r.law);
            serde_json::to_string(&law_answer_with(&r, near)).expect("serializes")
        }
        MatchRequest::Stage(r) => {
            serde_json::to_string(&stage_answer_with(r, None, None)).expect("serializes")
        }
        MatchRequest::Validate(r) => {
            serde_json::to_string(&validate_answer(&r)).expect("serializes")
        }
        MatchRequest::RecordScope(r) => {
            serde_json::to_string(&record_scope_answer(r)).expect("serializes")
        }
        MatchRequest::ScopeSplit(r) => {
            serde_json::to_string(&scope_split_answer(r)).expect("serializes")
        }
    };
    println!("{answer}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn law(id: &str, subject: &str, decision: &str, ts: &str) -> LawRow {
        LawRow {
            decision_id: id.to_owned(),
            subject: Some(subject.to_owned()),
            decision: Some(decision.to_owned()),
            ts: Some(ts.to_owned()),
            lane: None,
        }
    }

    /// The six real rows the plan pins as fixtures.
    fn fixture_laws() -> Vec<LawRow> {
        vec![
            law(
                "d-4b39ad4c",
                "file-budget",
                "A size-budget refusal is never answered by raising the allowance and never by splitting the PR.",
                "2026-09-04T16:59:33Z",
            ),
            law(
                "d-52ae01cb",
                "file-budget-port-residual",
                "A port that leaves residual code over the allowance is not finished.",
                "2026-09-07T10:00:00Z",
            ),
            law(
                "d-7678146e",
                "x-aaaa-file-budget-exception",
                "PR 84b2's allowance exception stands.",
                "2026-09-11T12:00:00Z",
            ),
            law(
                "d-5fff6924",
                "x-bbbb-file-budget-exception",
                "PR e64b's allowance exception stands.",
                "2026-09-11T09:00:00Z",
            ),
            law(
                "d-e177f3c5",
                "pr-1720-file-budget-allowance",
                "PR 1720's allowance is raised once.",
                "2026-09-11T15:00:00Z",
            ),
            law(
                "d-b6cc1a2a",
                "new-code-language",
                "New code goes in crates. Existing Python is shrink-only.",
                "2026-09-12T23:18:00Z",
            ),
        ]
    }

    #[test]
    fn ac3_tokens_drops_hex_ids_and_stop_words() {
        let got = tokens("x-1111 pr-1847 cf6a file-budget added");
        let want: BTreeSet<String> = ["added", "budget", "file"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(got, want);
    }

    fn ask_req(question: &str, subject: Option<&str>, node: Option<&str>) -> AskRequest {
        AskRequest {
            question: question.to_owned(),
            subject: subject.map(str::to_owned),
            node: node.map(str::to_owned),
            laws: fixture_laws(),
        }
    }

    const Q470: &str =
        "PR 1847 shrank +207 to +142. Requesting the operator-applied budget-exception label.";

    #[test]
    fn ac1_nearby_finds_the_five_file_budget_laws() {
        let req = ask_req(Q470, Some("pr-1847-budget-exception"), Some("x-cccc"));
        let ans = ask_answer(&req);
        assert!(ans.exact.is_empty());
        let ids: Vec<&str> = ans.nearby.iter().map(|h| h.decision_id.as_str()).collect();
        assert_eq!(ids.len(), 5);
        assert_eq!(ids[0], "d-7678146e");
        assert_eq!(ids[1], "d-5fff6924");
        assert!(!ids.contains(&"d-b6cc1a2a"));
        assert_eq!(ans.uncited, ids);
        let refusal = ans.nearby_refusal.expect("refusal names every listed law");
        for id in &ids {
            assert!(refusal.contains(id));
        }
    }

    #[test]
    fn ac2_cited_ids_pass_the_nearby_gate() {
        let mut question = String::from(Q470);
        for id in [
            "d-4b39ad4c",
            "d-52ae01cb",
            "d-7678146e",
            "d-5fff6924",
            "d-e177f3c5",
        ] {
            question.push_str(&format!(" {id}"));
        }
        let req = ask_req(&question, Some("pr-1847-budget-exception"), Some("x-cccc"));
        let ans = ask_answer(&req);
        assert!(ans.uncited.is_empty());
        assert!(ans.nearby_refusal.is_none());
    }

    #[test]
    fn ac4_exact_tier_review_coverage_hits_by_text() {
        let mut req = ask_req(
            "PR 1717 is stuck at the review cap: coverage reads uncovered and the \
             attestation is stale. Approve, or set the override label?",
            None,
            None,
        );
        req.laws = vec![law(
            "d-0fa92eb9",
            "review-coverage",
            "Two reviews maximum. Two reviews is two reviews.",
            "2026-08-28T00:00:00Z",
        )];
        let ans = ask_answer(&req);
        assert_eq!(ans.exact.len(), 1);
        assert_eq!(ans.exact[0].subject, "review-coverage");
        assert_eq!(ans.exact[0].ids, vec!["d-0fa92eb9"]);
        assert!(
            ans.nearby.is_empty(),
            "nearby runs only when a subject was named"
        );
    }

    fn open_q(
        id: &str,
        ts: &str,
        question: &str,
        subject: Option<&str>,
        asker: Option<&str>,
        session: Option<&str>,
    ) -> OpenQuestion {
        OpenQuestion {
            id: id.to_owned(),
            ts: ts.to_owned(),
            question: question.to_owned(),
            subject: subject.map(str::to_owned),
            node: None,
            asker: asker.map(str::to_owned),
            session_id: session.map(str::to_owned),
        }
    }

    #[test]
    fn ac5_law_mode_finds_the_subject_candidate() {
        let law_row = law("d-4b39ad4c", "file-budget", "A size-budget refusal is never answered by raising the allowance and never by splitting the PR.", "2026-09-04T16:59:33Z");
        let req = LawRequest {
            law: law_row,
            questions: vec![
                open_q(
                    "q-470f40d2",
                    "2026-09-12T17:16:00Z",
                    Q470,
                    Some("pr-1847-budget-exception"),
                    Some("2cf809f6"),
                    Some("sess-9f2c"),
                ),
                open_q(
                    "q-plain",
                    "2026-09-12T18:00:00Z",
                    "which base do we rebase on?",
                    None,
                    None,
                    None,
                ),
            ],
        };
        let ans = law_answer_with(&req, Vec::new());
        assert_eq!(ans.total, 1);
        assert_eq!(ans.candidates.len(), 1);
        let c = &ans.candidates[0];
        assert_eq!(c.question_id, "q-470f40d2");
        assert_eq!(c.asker.as_deref(), Some("2cf809f6"));
        assert_eq!(c.session_id.as_deref(), Some("sess-9f2c"));
        let line = &ans.lines[0];
        assert!(line.contains("fno agents mail send sess-9f2c"), "{line}");
        assert!(line.contains("q-470f40d2"), "{line}");
    }

    #[test]
    fn ac6_more_than_ten_candidates_leaves_a_count_line() {
        let law_row = law(
            "d-4b39ad4c",
            "file-budget",
            "budget law",
            "2026-09-04T16:59:33Z",
        );
        let questions: Vec<OpenQuestion> = (0..12)
            .map(|i| {
                open_q(
                    &format!("q-m{i:02}"),
                    "2026-09-12T10:00:00Z",
                    "raise the file budget?",
                    Some("file-budget-x"),
                    None,
                    None,
                )
            })
            .collect();
        let req = LawRequest {
            law: law_row,
            questions,
        };
        let ans = law_answer_with(&req, Vec::new());
        assert_eq!(ans.total, 12);
        assert_eq!(ans.lines.len(), 11, "10 candidate lines + 1 count line");
        assert!(ans.lines[10].starts_with("law: 2 more open question(s) may match"));
    }

    #[test]
    fn verb_contract_help_args_and_bad_json() {
        let h: Vec<String> = vec!["-h".into()];
        assert_eq!(run_law_match(&h), 0);
        let extra: Vec<String> = vec!["--nope".into()];
        assert_eq!(run_law_match(&extra), 2);
        let bad: Vec<String> = vec![];
        assert_eq!(run_law_match(&bad), 2, "unparsable stdin must exit 2");
    }

    #[test]
    fn ac2_hp_skill_payload_surfaces_the_review_laws() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("decisions.jsonl");
        let mut rows: Vec<String> = Vec::new();
        for i in 0..11 {
            rows.push(format!(
                "{{\"type\":\"operator_decision\",\"ts\":\"2026-09-10T00:00:{i:02}Z\",\
                 \"data\":{{\"decision_id\":\"d-rev{i:02}000\",\"subject\":\"review-rounds-sufficient\",\
                 \"decision\":\"Review rounds are capped. Law {i}.\",\"text\":\"x\",\
                 \"authority_source\":\"operator\"}}}}"
            ));
        }
        // The three specimens the plan names: texts whose near-miss wording
        // ("finding", "shrink round") must NOT match the keyword table.
        for (id, text) in [
            ("d-10a72d88", "finding a better place for the verb"),
            ("d-5fff6924", "shrink round stands"),
            ("d-7678146e", "shrink round stands"),
        ] {
            rows.push(format!(
                "{{\"type\":\"operator_decision\",\"ts\":\"2026-09-11T00:00:00Z\",\
                 \"data\":{{\"decision_id\":\"{id}\",\"subject\":\"file-budget-exception\",\
                 \"decision\":\"{text}\",\"text\":\"x\",\"authority_source\":\"operator\"}}}}"
            ));
        }
        std::fs::write(&path, rows.join("\n") + "\n").expect("writes");
        let hook = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Skill",
            "tool_input": {
                "skill": "fno:review",
                "args": "high https://github.com/o/r/pull/1"
            }
        });
        let answer = stage_answer_with(
            StageRequest {
                hook,
                paths: vec![],
            },
            Some(&path),
            None,
        );
        assert_eq!(answer["stage"], "review");
        let ctx = answer["hook_output"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("context present");
        assert_eq!(
            answer["hook_output"]["hookSpecificOutput"]["hookEventName"],
            "PostToolUse"
        );
        for i in 0..11 {
            assert!(
                ctx.contains(&format!("d-rev{i:02}000")),
                "law {i} missing: {ctx}"
            );
        }
        assert!(!ctx.contains("d-10a72d88"));
        assert!(!ctx.contains("d-5fff6924"));
        assert!(!ctx.contains("d-7678146e"));
    }

    #[test]
    fn ac2_prompt_first_token_classifies_the_stage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("decisions.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"operator_decision\",\"ts\":\"2026-09-01T00:00:00Z\",\
                 \"data\":{{\"decision_id\":\"d-0fa92eb9\",\"subject\":\"review-coverage\",\
                 \"decision\":\"Two reviews maximum.\",\"text\":\"x\",\
                 \"authority_source\":\"operator\"}}}}\n"
            ),
        )
        .expect("writes");
        let hook = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": "$fno:review low"
        });
        let answer = stage_answer_with(
            StageRequest {
                hook,
                paths: vec![],
            },
            Some(&path),
            None,
        );
        assert_eq!(answer["stage"], "review");
        assert_eq!(
            answer["hook_output"]["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        let ctx = answer["hook_output"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("context present");
        assert!(ctx.contains("d-0fa92eb9"), "{ctx}");
    }

    #[test]
    fn ac2_err_unreadable_index_is_a_report_not_silence() {
        let hook = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": "/fno:review low"
        });
        let answer = stage_answer_with(
            StageRequest {
                hook,
                paths: vec![],
            },
            Some(std::path::Path::new("/nonexistent/fno/decisions.jsonl")),
            None,
        );
        assert_eq!(answer["stage"], "review");
        let ctx = answer["hook_output"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("a failed read still renders the block");
        assert!(ctx.contains("could not be read"), "{ctx}");
        assert!(
            ctx.contains("fno backlog decisions --lane law --state live"),
            "{ctx}"
        );
    }

    #[test]
    fn ac2_edge_non_review_actions_stay_silent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("decisions.png");
        std::fs::write(&path, "").expect("writes");
        for hook in [
            serde_json::json!({
                "hook_event_name": "PostToolUse",
                "tool_name": "Skill",
                "tool_input": { "skill": "code-review-attest" }
            }),
            serde_json::json!({
                "hook_event_name": "UserPromptSubmit",
                "prompt": "please review this PR"
            }),
            serde_json::json!({
                "hook_event_name": "UserPromptSubmit",
                "prompt": "/fno:reviewer"
            }),
        ] {
            let answer = stage_answer_with(
                StageRequest {
                    hook: hook.clone(),
                    paths: vec![],
                },
                Some(&path),
                None,
            );
            assert_eq!(answer["stage"], Value::Null, "{hook}");
            assert_eq!(answer["hook_output"], Value::Null, "{hook}");
        }
    }

    #[test]
    fn ac2_edge_review_with_no_matching_law_renders_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("decisions.jsonl");
        std::fs::write(&path, "").expect("writes");
        let hook = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": "/fno:review low"
        });
        let answer = stage_answer_with(
            StageRequest {
                hook,
                paths: vec![],
            },
            Some(&path),
            None,
        );
        assert_eq!(answer["stage"], "review");
        assert_eq!(answer["hook_output"], Value::Null);
    }

    #[test]
    fn ac3_hp_cap_overflow_still_lists_every_law_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("decisions.jsonl");
        let mut rows: Vec<String> = Vec::new();
        for i in 0..40 {
            rows.push(format!(
                "{{\"type\":\"operator_decision\",\"ts\":\"2026-09-10T00:00:{i:02}Z\",\
                 \"data\":{{\"decision_id\":\"d-cap{i:04}000\",\"subject\":\"review-cap-fixture\",\
                 \"decision\":\"Ruling number {i} stands. {}\",\"text\":\"x\",\
                 \"authority_source\":\"operator\"}}}}",
                "x".repeat(90)
            ));
        }
        std::fs::write(&path, rows.join("\n") + "\n").expect("writes");
        let hook = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": "/fno:review low"
        });
        let answer = stage_answer_with(
            StageRequest {
                hook,
                paths: vec![],
            },
            Some(&path),
            None,
        );
        let ctx = answer["hook_output"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("context present");
        let overflow: Vec<&str> = ctx.lines().filter(|l| l.starts_with("- and ")).collect();
        assert!(
            overflow.is_empty(),
            "no `- and N more` line may remain: {ctx}"
        );
        // Every matched law id on its own line, in the validator's
        // `- <id> (<subject>):` shape, whatever the cap cut.
        for i in 0..40 {
            let id = format!("d-cap{i:04}000");
            let listed = ctx
                .lines()
                .any(|l| l.starts_with(&format!("- {id} (review-cap-fixture, project:fno):")));
            assert!(listed, "{id} missing from the block: {ctx}");
        }
        // The summarized body under the cap; short overflow lines excluded.
        let body_len: usize = ctx
            .lines()
            .filter(|l| !l.contains("fno backlog decisions"))
            .map(|l| l.len() + 1)
            .sum();
        assert!(body_len <= 2000, "body {body_len} exceeds the cap");
        assert_eq!(
            answer["hook_output"]["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
    }

    #[test]
    fn ac4_hp_verb_law_matches_blueprint_stage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_index(
            dir.path(),
            &[stage_row(
                "d-verbs0001",
                "top-level-verbs",
                "The root menu caps top-level verbs.",
            )],
        );
        let hook = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Skill",
            "tool_input": {
                "skill": "fno:blueprint",
                "args": "x-aaaa"
            }
        });
        let answer = stage_answer_with(
            StageRequest {
                hook,
                paths: vec![],
            },
            Some(&path),
            None,
        );
        let ctx = answer["hook_output"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("context present");
        assert!(
            ctx.contains("- d-verbs0001 (top-level-verbs, project:fno):"),
            "{ctx}"
        );
    }

    fn validate_req(
        subject: &str,
        decision: &str,
        rationale: Option<&str>,
        supersedes: Option<&str>,
    ) -> ValidateRequest {
        ValidateRequest {
            subject: subject.to_owned(),
            decision: decision.to_owned(),
            rationale: rationale.map(str::to_owned),
            supersedes: supersedes.map(str::to_owned),
        }
    }

    #[test]
    fn ac4_hp_node_id_and_pr_subjects_are_refused() {
        for subject in ["x-aaaa", "pr-1157"] {
            let req = validate_req(subject, "Two rounds.", Some("r"), None);
            let answer = validate_answer(&req);
            let refusal = answer["refusal"].as_str().expect("refusal");
            assert!(
                refusal.contains("a node or PR id is not a law subject"),
                "{subject}: {refusal}"
            );
            assert!(refusal.contains(subject), "{subject}: {refusal}");
        }
    }

    #[test]
    fn a_placeholder_statement_is_refused() {
        // The 2026-08-29 junk law was exactly this shape: a smoke call with
        // x/y/z landed a live law only the operator could clear.
        let req = validate_req("x", "y", Some("z"), None);
        let answer = validate_answer(&req);
        let refusal = answer["refusal"].as_str().expect("refusal");
        assert!(
            refusal.contains("must be more than one character"),
            "{refusal}"
        );
        // The boundary: two characters are a statement, poor but legal.
        let req = validate_req("mx", "my", Some("why"), None);
        let answer = validate_answer(&req);
        assert_eq!(answer["refusal"], Value::Null);
    }

    #[test]
    fn ac4_topic_a_topic_subject_with_a_cited_node_id_passes() {
        let req = validate_req("review-rounds-cap", "Cite x-aaaa in text.", Some("r"), None);
        let answer = validate_answer(&req);
        assert_eq!(answer["refusal"], Value::Null);
    }

    #[test]
    fn ac4_port_rules_match_the_python_word_for_word() {
        let cases: Vec<(ValidateRequest, &str)> = vec![
            (
                validate_req("", "Decision.", Some("r"), None),
                "subject and decision are required",
            ),
            (
                validate_req("subject", "", Some("r"), None),
                "subject and decision are required",
            ),
            (
                validate_req("subject", "Decision.", None, None),
                "rationale is required for durable law",
            ),
            (
                validate_req("subject", "Decision.", Some("  "), None),
                "rationale is required for durable law",
            ),
            (
                validate_req("subject", "This PR merges now.", Some("r"), None),
                "the statement is coordination, not durable law",
            ),
            (
                validate_req("subject", "Until merge it stands.", Some("r"), None),
                "the statement is coordination, not durable law",
            ),
            (
                validate_req("subject", "Decision.", Some("r"), Some("x-bbbb")),
                "supersedes must be a decision id",
            ),
        ];
        for (req, want) in cases {
            let answer = validate_answer(&req);
            let refusal = answer["refusal"].as_str().expect("refusal");
            assert!(
                refusal.starts_with(want.split(',').next().unwrap_or(want)),
                "wanted {want}, got {refusal}"
            );
        }
        // The happy shape records: no refusal at all.
        let ok = validate_answer(&validate_req(
            "review-rounds-cap",
            "Two rounds.",
            Some("r"),
            Some("d-0ad0ad0a"),
        ));
        assert_eq!(ok["refusal"], Value::Null);
    }

    #[test]
    fn ac4_near_law_lines_name_the_prior_ruling_and_both_remedies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("decisions.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"operator_decision\",\"ts\":\"2026-09-01T00:00:00Z\",\
                 \"data\":{{\"decision_id\":\"d-777e7d1f\",\"subject\":\"review-rounds-sufficient\",\
                 \"decision\":\"Two rounds complete the review cap.\",\"text\":\"x\",\
                 \"authority_source\":\"operator\"}}}}\n"
            ),
        )
        .expect("writes");
        let index = decision_index::live_laws(&path).expect("reads");
        let new_law = LawRow {
            decision_id: "d-00000001".to_owned(),
            subject: Some("exhausted-rounds-disposition".to_owned()),
            decision: Some("A fourth round is spent".to_owned()),
            ts: None,
            lane: None,
        };
        let lines = near_law_lines_from(&index, &new_law);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert!(line.contains("d-777e7d1f"), "{line}");
        assert!(line.contains("review-rounds-sufficient"), "{line}");
        assert!(line.contains("decide-retract"), "{line}");
        assert!(line.contains("--supersedes"), "{line}");
        // The new law is named only as the subject of the line, never as a
        // near hit of itself.
        assert!(!line.contains("near live law d-00000001"), "{line}");
        assert!(line.contains("sits near live law d-777e7d1f"), "{line}");
    }

    fn write_index(dir: &std::path::Path, rows: &[String]) -> std::path::PathBuf {
        let path = dir.join("decisions.jsonl");
        std::fs::write(&path, rows.join("\n") + "\n").expect("writes");
        path
    }

    fn stage_row(id: &str, subject: &str, decision: &str) -> String {
        format!(
            "{{\"type\":\"operator_decision\",\"ts\":\"2026-09-12T00:00:00Z\",\
             \"data\":{{\"decision_id\":\"{id}\",\"subject\":\"{subject}\",\
             \"decision\":\"{decision}\",\"text\":\"x\",\"authority_source\":\"operator\"}}}}"
        )
    }
    fn edit_row(id: &str, paths: &str) -> String {
        format!(
            "{{\"type\":\"operator_decision\",\"ts\":\"2026-09-12T00:00:00Z\",\
             \"data\":{{\"decision_id\":\"{id}\",\"subject\":\"path-governed\",\
             \"decision\":\"the edit-governed ruling\",\"text\":\"x\",\
             \"authority_source\":\"operator\",\"paths\":{paths}}}}}"
        )
    }
    fn edit_req(session: Option<&str>, targets: &[&str]) -> StageRequest {
        let mut hook = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Edit",
            "tool_input": {"file_path": "crates/x.rs"}
        });
        if let Some(s) = session {
            hook["session_id"] = serde_json::json!(s);
        }
        StageRequest {
            hook,
            paths: targets.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn ac1_edit_payload_prints_the_path_law() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = tempfile::tempdir().expect("tempdir");
        let path = write_index(dir.path(), &[edit_row("d-editlaw01", "[\"crates/**\"]")]);
        let req = edit_req(Some("S"), &["crates/fno-agents/src/lib.rs"]);
        let answer = edit_answer(req, Some(&path), Some(state.path()));
        assert_eq!(answer["stage"], "edit");
        let ctx = answer["hook_output"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("context present");
        assert!(ctx.contains("Law governing edit"), "{ctx}");
        assert!(ctx.contains("d-editlaw01"), "{ctx}");
    }

    #[test]
    fn ac2_the_same_session_prints_the_law_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = tempfile::tempdir().expect("tempdir");
        let path = write_index(dir.path(), &[edit_row("d-editlaw01", "[\"crates/**\"]")]);
        let first = edit_answer(
            edit_req(Some("S"), &["crates/fno-agents/src/lib.rs"]),
            Some(&path),
            Some(state.path()),
        );
        assert!(first["hook_output"].is_object(), "{first}");
        let second = edit_answer(
            edit_req(Some("S"), &["crates/fno/src/main.rs"]),
            Some(&path),
            Some(state.path()),
        );
        assert!(second["hook_output"].is_null(), "{second}");
    }

    #[test]
    fn ac3_a_row_without_paths_never_prints_at_an_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = tempfile::tempdir().expect("tempdir");
        // The row's TEXT contains the word crates; the edit read matches
        // paths globs only, never keywords.
        let row = stage_row(
            "d-textonly1",
            "new-code-language",
            "New code goes in crates.",
        );
        let path = write_index(dir.path(), &[row]);
        let answer = edit_answer(
            edit_req(Some("S"), &["crates/x.rs"]),
            Some(&path),
            Some(state.path()),
        );
        assert!(answer["hook_output"].is_null(), "{answer}");
    }

    #[test]
    fn ac3_a_request_without_paths_answers_the_verb_stage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_index(
            dir.path(),
            &[stage_row(
                "d-b6cc1a2a",
                "new-code-language",
                "New code goes in crates. Existing Python is shrink-only.",
            )],
        );
        let hook = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Skill",
            "tool_input": {"skill": "fno:blueprint", "args": "x-aaaa"}
        });
        let answer = stage_answer_with(
            StageRequest {
                hook,
                paths: vec![],
            },
            Some(&path),
            None,
        );
        assert_eq!(answer["stage"], "blueprint");
    }

    #[test]
    fn ac4_an_unreadable_index_reports_never_silence() {
        let missing = std::path::Path::new("/nonexistent/fno-edit-read/decisions.jsonl");
        let state = tempfile::tempdir().expect("tempdir");
        let answer = edit_answer(
            edit_req(Some("S"), &["crates/x.rs"]),
            Some(missing),
            Some(state.path()),
        );
        let ctx = answer["hook_output"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("context present");
        assert!(ctx.contains("could not be read"), "{ctx}");
    }

    #[test]
    fn an_edit_with_no_session_prints_every_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = tempfile::tempdir().expect("tempdir");
        let path = write_index(dir.path(), &[edit_row("d-editlaw02", "[\"crates/**\"]")]);
        for _ in 0..2 {
            let answer = edit_answer(
                edit_req(None, &["crates/x.rs"]),
                Some(&path),
                Some(state.path()),
            );
            assert!(answer["hook_output"].is_object(), "{answer}");
        }
    }

    #[test]
    fn ac1_blueprint_payload_surfaces_the_language_law() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_index(
            dir.path(),
            &[stage_row(
                "d-b6cc1a2a",
                "new-code-language",
                "New code goes in crates. Existing Python is shrink-only.",
            )],
        );
        let hook = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Skill",
            "tool_input": {
                "skill": "fno:blueprint",
                "args": "x-aaaa"
            }
        });
        let answer = stage_answer_with(
            StageRequest {
                hook,
                paths: vec![],
            },
            Some(&path),
            None,
        );
        assert_eq!(answer["stage"], "blueprint");
        let ctx = answer["hook_output"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("context present");
        assert!(ctx.contains("Law governing blueprint"), "{ctx}");
        assert!(ctx.contains("d-b6cc1a2a"), "{ctx}");
    }

    #[test]
    fn ac2_epic_named_law_lists_node_subject_law_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_index(
            dir.path(),
            &[
                stage_row(
                    "d-epic0001",
                    "epic-merge-authority",
                    "The x-bbbb epic keeps merge authority with the crown.",
                ),
                stage_row("d-node0001", "x-aaaa", "unfindable by topic"),
                stage_row(
                    "d-fnos0001",
                    "fno",
                    "a ruling named for the project slug stands",
                ),
            ],
        );
        let hook = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Skill",
            "tool_input": {
                "skill": "fno:blueprint",
                "args": "x-aaaa"
            }
        });
        // Hermetic graph fixture: the epic read must never lean on the
        // machine's live graph.json.
        let graph = dir.path().join("graph.json");
        std::fs::write(
            &graph,
            serde_json::json!({
                "entries": [
                    {"id": "x-aaaa", "parent": "x-bbbb", "project": "fno"}
                ]
            })
            .to_string(),
        )
        .expect("writes");
        let answer = stage_answer_with(
            StageRequest {
                hook,
                paths: vec![],
            },
            Some(&path),
            Some(&graph),
        );
        let ctx = answer["hook_output"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("context present");
        assert!(ctx.contains("d-epic0001"), "{ctx}");
        assert!(
            !ctx.contains("unfindable by topic"),
            "node-subject row must be dropped: {ctx}"
        );
        assert!(
            !ctx.contains("d-fnos0001"),
            "a project-slug SUBJECT is a topic, not a scope: idents no \
             longer carry the slug, so the row only lists when the node's \
             epic or a stage keyword names it: {ctx}"
        );
    }

    #[test]
    fn ac1_target_and_execute_verbs_classify_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_index(
            dir.path(),
            &[stage_row(
                "d-b6cc1a2a",
                "new-code-language",
                "New code goes in crates. Existing Python is shrink-only.",
            )],
        );
        for hook in [
            serde_json::json!({
                "hook_event_name": "UserPromptSubmit",
                "prompt": "/fno:target x-aaaa"
            }),
            serde_json::json!({
                "hook_event_name": "UserPromptSubmit",
                "prompt": "$fno:execute a-plan-path"
            }),
        ] {
            let answer = stage_answer_with(
                StageRequest {
                    hook,
                    paths: vec![],
                },
                Some(&path),
                None,
            );
            assert_eq!(answer["stage"], "target");
            let ctx = answer["hook_output"]["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .expect("context present");
            assert!(ctx.contains("Law governing target"), "{ctx}");
        }
    }

    #[test]
    fn payload_node_id_extracts_from_skill_args_or_prompt() {
        let skill = serde_json::json!({
            "tool_name": "Skill",
            "tool_input": { "skill": "fno:blueprint", "args": "x-aaaa" }
        });
        assert_eq!(payload_node_id(&skill).as_deref(), Some("x-aaaa"));
        let prompt = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": "/fno:target x-aaaa now"
        });
        assert_eq!(payload_node_id(&prompt).as_deref(), Some("x-aaaa"));
        let no_node = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": "/fno:target auto-merge \"a feature\""
        });
        assert_eq!(payload_node_id(&no_node), None);
    }

    #[test]
    fn node_subject_idents_resolves_parent_and_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let graph = dir.path().join("graph.json");
        std::fs::write(
            &graph,
            serde_json::json!({
                "entries": [
                    {"id": "x-aaaa", "parent": "x-bbbb", "project": "fno"}
                ]
            })
            .to_string(),
        )
        .expect("writes");
        let (idents, project, unread) = node_subject_idents("x-aaaa", Some(&graph));
        assert_eq!(idents, vec!["x-aaaa", "x-bbbb"]);
        assert_eq!(project, Some("fno".to_string()));
        assert!(unread.is_none());
        let (missing, missing_project, missing_unread) =
            node_subject_idents("x-ffff", Some(&graph));
        assert_eq!(missing, vec!["x-ffff"]);
        assert!(missing_project.is_none());
        assert!(missing_unread.is_none());
    }

    #[test]
    fn ac1_readable_graph_has_no_unread_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = write_index(
            dir.path(),
            &[stage_row(
                "d-epic0001",
                "x-bbbb",
                "The epic ruling is readable.",
            )],
        );
        let graph = dir.path().join("graph.json");
        std::fs::write(
            &graph,
            serde_json::json!({
                "entries": [{"id": "x-aaaa", "parent": "x-bbbb", "project": "fno"}]
            })
            .to_string(),
        )
        .expect("writes");
        let hook = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": "/fno:blueprint x-aaaa"
        });
        let answer = stage_answer_with(
            StageRequest {
                hook,
                paths: vec![],
            },
            Some(&index),
            Some(&graph),
        );
        assert_eq!(answer["unread"], serde_json::json!([]));
        assert!(
            answer["hook_output"]["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .expect("context")
                .contains("d-epic0001")
        );
    }

    #[test]
    fn ac1_unreadable_graph_names_scope_and_keeps_node_id_matching() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = write_index(
            dir.path(),
            &[stage_row(
                "d-node0001",
                "x-aaaa-context",
                "The node context ruling is readable.",
            )],
        );
        let graph = dir.path().join("graph.json");
        std::fs::write(&graph, "not json").expect("writes");
        let hook = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": "/fno:blueprint x-aaaa"
        });
        let answer = stage_answer_with(
            StageRequest {
                hook,
                paths: vec![],
            },
            Some(&index),
            Some(&graph),
        );
        let ctx = answer["hook_output"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("context");
        assert!(
            ctx.contains("Unread: the node's epic and project (graph:"),
            "{ctx}"
        );
        assert!(ctx.contains("d-node0001"), "{ctx}");
        assert_eq!(answer["unread"].as_array().expect("unread").len(), 1);
    }

    #[test]
    fn ac2_unreadable_graph_and_index_keep_both_reasons() {
        let dir = tempfile::tempdir().expect("tempdir");
        let graph = dir.path().join("graph.json");
        std::fs::write(&graph, "not json").expect("writes");
        let index = dir.path().join("missing-decisions.jsonl");
        let hook = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": "/fno:blueprint x-aaaa"
        });
        let answer = stage_answer_with(
            StageRequest {
                hook,
                paths: vec![],
            },
            Some(&index),
            Some(&graph),
        );
        let ctx = answer["hook_output"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("context");
        assert!(
            ctx.contains("Unread: the node's epic and project (graph:"),
            "{ctx}"
        );
        assert!(ctx.contains("Unread: the decision index ("), "{ctx}");
        assert!(!ctx.contains("These live operator rulings govern"), "{ctx}");
        assert_eq!(answer["unread"].as_array().expect("unread").len(), 2);
    }

    #[test]
    fn ac2_empty_match_with_unread_graph_has_no_empty_law_block() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = write_index(
            dir.path(),
            &[stage_row("d-other0001", "unrelated", "not for this stage")],
        );
        let graph = dir.path().join("graph.json");
        std::fs::write(&graph, "not json").expect("writes");
        let hook = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": "/fno:blueprint x-aaaa"
        });
        let answer = stage_answer_with(
            StageRequest {
                hook,
                paths: vec![],
            },
            Some(&index),
            Some(&graph),
        );
        let ctx = answer["hook_output"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("context");
        assert!(
            ctx.contains("Unread: the node's epic and project (graph:"),
            "{ctx}"
        );
        assert!(!ctx.contains("These live operator rulings govern"), "{ctx}");
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;

    fn sources(tmp: &std::path::Path) -> Vec<std::path::PathBuf> {
        vec![tmp.join("global.toml")]
    }

    fn write_map(tmp: &std::path::Path, slug: &str, proj: &std::path::Path) {
        std::fs::write(
            tmp.join("global.toml"),
            format!(
                "[[work.workspaces.main.projects]]\nname = \"{slug}\"\npath = \"{}\"\n",
                proj.display()
            ),
        )
        .expect("writes");
    }

    fn law_row(id: &str, lane: &str, scope: Option<&str>) -> Value {
        let mut row = serde_json::json!({"decision_id": id, "lane": lane});
        if let Some(scope) = scope {
            row["scope"] = serde_json::json!(scope);
        }
        row
    }

    #[test]
    fn the_direct_settings_match_names_the_configured_project() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_map(tmp.path(), "demo", &tmp.path().join("proj"));
        let cwd = tmp.path().join("proj");
        let slug = resolve_project(Some(&cwd), &sources(tmp.path())).expect("resolves");
        assert_eq!(slug, "demo");
    }

    #[test]
    fn a_fno_worktree_rung_attributes_the_repo_segment() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_map(
            tmp.path(),
            "demo",
            &tmp.path().join("code").join("footnote"),
        );
        let cwd = tmp
            .path()
            .join("state")
            .join(".fno")
            .join("worktrees")
            .join("footnote")
            .join("feat-branch");
        let slug = resolve_project(Some(&cwd), &sources(tmp.path())).expect("resolves");
        assert_eq!(slug, "demo");
    }

    #[test]
    fn an_unplacable_cwd_refuses_and_names_the_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_map(tmp.path(), "demo", &tmp.path().join("proj"));
        let cwd = tmp.path().join("nowhere");
        let err = resolve_project(Some(&cwd), &sources(tmp.path())).expect_err("refuses");
        assert!(err.contains("nowhere"), "{err}");
    }

    #[test]
    fn the_door_stamps_project_by_default_and_global_by_flag() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_map(tmp.path(), "demo", &tmp.path().join("proj"));
        let cwd = tmp.path().join("proj");
        let answer = record_scope_answer_in(Some(&cwd), &sources(tmp.path()), false, &[]);
        assert_eq!(answer["scope"], "project:demo");
        let answer = record_scope_answer_in(Some(&cwd), &sources(tmp.path()), true, &[]);
        assert_eq!(answer["scope"], "global");
    }

    #[test]
    fn the_door_refuses_closed_when_no_project_resolves() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_map(tmp.path(), "demo", &tmp.path().join("proj"));
        let answer = record_scope_answer_in(
            Some(&tmp.path().join("nowhere")),
            &sources(tmp.path()),
            false,
            &[],
        );
        assert_eq!(answer["ok"], false);
        assert!(answer["refusal"]
            .as_str()
            .expect("refusal")
            .contains("no project stamps this law"));
    }

    #[test]
    fn the_record_door_splits_echoes_and_refuses_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_map(tmp.path(), "demo", &tmp.path().join("proj"));
        let cwd = tmp.path().join("proj");
        let answer = record_scope_answer_in(
            Some(&cwd),
            &sources(tmp.path()),
            false,
            &["crates/**, hooks/*.sh".to_string(), "  ".to_string()],
        );
        assert_eq!(answer["ok"], true);
        assert_eq!(answer["scope"], "project:demo");
        assert_eq!(
            answer["paths"],
            serde_json::json!(["crates/**", "hooks/*.sh"])
        );
        for bad in ["/etc/**", "../x/**"] {
            let answer =
                record_scope_answer_in(Some(&cwd), &sources(tmp.path()), false, &[bad.to_string()]);
            assert_eq!(answer["ok"], false, "{bad}");
            assert!(
                answer["refusal"].as_str().expect("refusal").contains(bad),
                "{answer}"
            );
        }
    }

    // ── the record door ───────────────────────────────────────────────────

    fn door(subject: &str, decision: &str) -> RecordDoor {
        RecordDoor {
            subject: subject.to_string(),
            decision: Some(decision.to_string()),
            decision_file: None,
            rationale: Some("the operator owns durable policy.".to_string()),
            options: Vec::new(),
            supersedes: None,
            graduation: None,
            graduation_ref: None,
            reads: Vec::new(),
            is_global: false,
            raw_paths: Vec::new(),
        }
    }

    /// Hermetic state for a door write: tmp FNO_HOME (index, questions,
    /// graph) + tmp FNO_REPO_ROOT (journal, evidence root) + a work map.
    /// The env is process-global, so the lock serializes every env-touching
    /// door test; the guard drops after the env restore.
    static DOOR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // Field 2 is the env lock guard, held for its Drop and never read.
    #[allow(dead_code)]
    struct DoorEnv(
        tempfile::TempDir,
        tempfile::TempDir,
        std::sync::MutexGuard<'static, ()>,
    );

    impl DoorEnv {
        fn new() -> Self {
            let guard = DOOR_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let home = tempfile::tempdir().expect("tempdir");
            let root = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(root.path().join(".fno")).expect("mkdir");
            std::env::set_var("FNO_HOME", home.path());
            std::env::set_var("FNO_REPO_ROOT", root.path());
            let map = root.path().join("settings.yaml");
            // The map names THIS process's cwd: the door resolves the scope
            // from std::env::current_dir against the work map.
            std::fs::write(
                &map,
                format!(
                    "work:\n  workspaces:\n    main:\n      projects:\n        - name: demo\n          path: {}\n",
                    std::env::current_dir()
                        .expect("cwd")
                        .display()
                ),
            )
            .expect("writes");
            std::env::set_var("FNO_GLOBAL_SETTINGS_PATH", &map);
            Self(home, root, guard)
        }

        // The appends land in the SQLite store beside the journal, so the
        // reads go through the store merge, never the raw jsonl.
        fn store_text(&self, journal: &std::path::Path) -> String {
            crate::event_store::journal_text(journal, &["operator_decision"])
        }

        fn index_text(&self) -> String {
            self.store_text(&self.0.path().join("decisions.jsonl"))
        }

        fn journal_text(&self) -> String {
            self.store_text(&self.1.path().join(".fno").join("events.jsonl"))
        }
    }

    impl Drop for DoorEnv {
        fn drop(&mut self) {
            std::env::remove_var("FNO_HOME");
            std::env::remove_var("FNO_REPO_ROOT");
            std::env::remove_var("FNO_GLOBAL_SETTINGS_PATH");
        }
    }

    #[test]
    fn parse_record_door_reads_positionals_flags_and_inline_values() {
        let argv: Vec<String> = [
            "topic",
            "The body",
            "--rationale=why",
            "--option",
            "a",
            "--option",
            "b",
            "--global",
            "--paths",
            "crates/**, hooks/*.sh",
            "--read",
            "head -5 x.rs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let door = parse_record_door(&argv).expect("parses");
        assert_eq!(door.subject, "topic");
        assert_eq!(door.decision.as_deref(), Some("The body"));
        assert_eq!(door.rationale.as_deref(), Some("why"));
        assert_eq!(door.options, vec!["a", "b"]);
        assert!(door.is_global);
        assert_eq!(door.raw_paths, vec!["crates/**, hooks/*.sh"]);
        assert_eq!(door.reads, vec!["head -5 x.rs"]);

        assert!(parse_record_door(&[]).is_err());
        assert!(parse_record_door(&["--nope".to_string(), "x".to_string()]).is_err());
        assert!(parse_record_door(&["--rationale".to_string()])
            .err()
            .expect("errs")
            .contains("needs a value"));
    }

    #[test]
    fn graduation_defaults_to_guidance_and_refuses_bad_kinds() {
        assert_eq!(
            graduation_or_guidance(None, None).expect("ok")["kind"],
            json!("guidance")
        );
        assert_eq!(
            graduation_or_guidance(Some("guidance"), None).expect("ok")["kind"],
            json!("guidance")
        );
        assert!(graduation_or_guidance(Some("guidance"), Some("x")).is_err());
        let enforced =
            graduation_or_guidance(Some("enforced"), Some("file:docs/law.md=>marker")).expect("ok");
        assert_eq!(enforced["artifact"], json!("file:docs/law.md=>marker"));
        assert!(graduation_or_guidance(Some("enforced"), Some("no prefix")).is_err());
        let follow = graduation_or_guidance(
            Some("should-be-enforced-but-i-did-not"),
            Some("NODE:x-4a11c2de"),
        )
        .expect("ok");
        assert_eq!(follow["follow_up"], json!("node:x-4a11c2de"));
        assert!(graduation_or_guidance(Some("nonsense"), None).is_err());
        assert!(graduation_or_guidance(None, Some("x")).is_err());
    }

    #[test]
    fn the_door_records_journal_index_and_table_under_chat_authority() {
        let env = DoorEnv::new();
        let code = record_door_write(
            door("merge-authority", "Merges belong to the operator"),
            "Merges belong to the operator".to_string(),
            &Caller::as_authority("chat_attested"),
        );
        assert_eq!(code, 0, "exit 0 on the happy record");
        let index = env.index_text();
        assert!(
            index.contains("\"authority_source\":\"chat_attested\""),
            "{index}"
        );
        assert!(index.contains("\"scope\":\"project:demo\""), "{index}");
        assert!(
            index.contains("\"graduation\":{\"kind\":\"guidance\"}"),
            "{index}"
        );
        assert!(
            !index.contains("\"paths\""),
            "no paths key when none sent: {index}"
        );
        let journal = env.journal_text();
        assert!(
            journal.contains("Merges belong to the operator"),
            "{journal}"
        );
    }

    #[test]
    fn the_door_records_the_paths_globs_it_validated() {
        let env = DoorEnv::new();
        let mut d = door("edit-governed", "A law that names a path.");
        d.raw_paths = vec!["crates/**".to_string()];
        let code = record_door_write(
            d,
            "A law that names a path.".to_string(),
            &Caller::as_authority("chat_attested"),
        );
        assert_eq!(code, 0);
        assert!(
            env.index_text().contains("\"paths\":[\"crates/**\"]"),
            "{}",
            env.index_text()
        );
    }

    #[test]
    fn chat_cannot_supersede_an_operator_row_but_can_its_own() {
        let env = DoorEnv::new();
        let index = env.0.path().join("decisions.jsonl");
        std::fs::write(
            &index,
            concat!(
                "{\"ts\":\"2026-08-29T19:00:00Z\",\"type\":\"operator_decision\",\"source\":\"test\",",
                "\"data\":{\"decision_id\":\"d-0ad0ad0a\",\"subject\":\"merge-authority\",",
                "\"decision\":\"Merges belong to the operator\",\"authority_source\":\"operator\"}}\n"
            ),
        )
        .expect("writes");
        let mut d = door("merge-authority", "Merges belong to whoever asks");
        d.supersedes = Some("d-0ad0ad0a".to_string());
        let code = record_door_write(
            d,
            "Merges belong to whoever asks".to_string(),
            &Caller::as_authority("chat_attested"),
        );
        assert_eq!(code, 3, "operator rows are out of a chat session's reach");
        assert_eq!(
            std::fs::read_to_string(&index)
                .expect("reads")
                .lines()
                .count(),
            1,
            "nothing new recorded"
        );

        // The make-it-fail control: the same supersession at a chat row lands.
        let body = concat!(
            "{\"ts\":\"2026-08-29T19:00:00Z\",\"type\":\"operator_decision\",\"source\":\"test\",",
            "\"data\":{\"decision_id\":\"d-c4a7c4a7\",\"subject\":\"merge-authority\",",
            "\"decision\":\"Merges belong to the operator\",\"authority_source\":\"chat_attested\"}}\n"
        );
        std::fs::write(&index, body).expect("writes");
        let mut d = door("merge-authority", "Merges belong to whoever asks");
        d.supersedes = Some("d-c4a7c4a7".to_string());
        let code = record_door_write(
            d,
            "Merges belong to whoever asks".to_string(),
            &Caller::as_authority("chat_attested"),
        );
        assert_eq!(code, 0);
        assert!(env.index_text().contains("d-c4a7c4a7"));
    }

    #[test]
    fn a_waiver_subject_refuses_chat_authority() {
        let env = DoorEnv::new();
        let code = record_door_write(
            door(
                "review-coverage-waiver:acme/widgets#42@cccc",
                "review coverage waived for this head",
            ),
            "review coverage waived for this head".to_string(),
            &Caller::as_authority("chat_attested"),
        );
        assert_eq!(code, 3);
        assert!(env.index_text().is_empty());
    }

    #[test]
    fn a_code_fact_without_a_read_refuses_and_with_one_records_the_row() {
        let env = DoorEnv::new();
        std::fs::write(
            env.1.path().join("advance.py"),
            (1..=200).map(|i| format!("line {i}\n")).collect::<String>(),
        )
        .expect("writes");
        let code = record_door_write(
            door(
                "territory-resolver",
                "advance.py:167 is the territory resolver",
            ),
            "advance.py:167 is the territory resolver".to_string(),
            &Caller::as_authority("chat_attested"),
        );
        assert_eq!(code, 3, "unmeasured claim refused");
        assert!(env.index_text().is_empty());

        let mut d = door("territory-resolver", "advance.py is 200 lines");
        d.reads = vec!["head -5 advance.py".to_string()];
        let code = record_door_write(
            d,
            "advance.py is 200 lines".to_string(),
            &Caller::as_authority("chat_attested"),
        );
        assert_eq!(code, 0);
        let index = env.index_text();
        assert!(index.contains("\"reads\":[{"), "{index}");
        assert!(index.contains("head -5 advance.py"), "{index}");
    }

    #[test]
    fn an_index_write_failure_exits_1_with_the_journal_durable() {
        let env = DoorEnv::new();
        // A DIRECTORY at the index store path: the recall append cannot land.
        let home = env.0.path();
        std::fs::remove_file(home.join("decisions.jsonl")).ok();
        std::fs::create_dir_all(home.join("decisions.db")).expect("mkdir");
        let code = record_door_write(
            door("merge-authority", "Merges belong to the operator"),
            "Merges belong to the operator".to_string(),
            &Caller::as_authority("chat_attested"),
        );
        assert_eq!(code, 1, "recorded-but-index-failed");
        assert!(
            env.journal_text().contains("Merges belong to the operator"),
            "the journal holds the ruling"
        );
    }

    #[test]
    fn preflight_refuses_a_placeholder_statement_before_any_identity_read() {
        let argv: Vec<String> = ["x", "y", "--rationale", "z"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match record_door_preflight(&argv, "") {
            Err(code) => assert_eq!(code, 3),
            Ok(_) => panic!("placeholder must refuse"),
        }
    }

    #[test]
    fn scope_split_hides_only_foreign_law_rows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_map(tmp.path(), "demo", &tmp.path().join("proj"));
        let cwd = tmp.path().join("proj");
        let req = ScopeSplitRequest {
            rows: vec![
                law_row("d-1", "law", Some("global")),
                law_row("d-2", "law", Some("project:demo")),
                law_row("d-3", "law", Some("project:etl")),
                law_row("d-4", "coord", None),
            ],
        };
        let answer = scope_split_answer_in(Some(&cwd), &sources(tmp.path()), req);
        assert_eq!(answer["hidden"], 1);
        let kept: Vec<&str> = answer["kept"]
            .as_array()
            .expect("kept")
            .iter()
            .map(|r| r.get("decision_id").and_then(Value::as_str).unwrap_or(""))
            .collect();
        assert_eq!(kept, vec!["d-1", "d-2", "d-4"]);
        assert!(answer["note"].as_str().unwrap_or("").contains("hid 1"));
    }

    #[test]
    fn scope_split_fails_open_with_a_note_when_the_project_cannot_resolve() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_map(tmp.path(), "demo", &tmp.path().join("proj"));
        let req = ScopeSplitRequest {
            rows: vec![law_row("d-1", "law", Some("project:demo"))],
        };
        let answer =
            scope_split_answer_in(Some(&tmp.path().join("nowhere")), &sources(tmp.path()), req);
        assert_eq!(answer["hidden"], 0);
        assert_eq!(answer["kept"].as_array().expect("kept").len(), 1);
        assert_eq!(answer["note"], "");
    }
}
