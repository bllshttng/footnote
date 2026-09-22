//! The question-to-law matcher: one pure function set behind a hidden
//! JSON verb, reached as `fno-agents law-match`. The `stage` and `law` modes
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
}

/// The raw hook payload, verbatim from the harness event.
#[derive(Deserialize)]
struct StageRequest {
    hook: serde_json::Value,
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

#[derive(Deserialize)]
struct AskRequest {
    question: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    node: Option<String>,
    laws: Vec<LawRow>,
}

#[derive(Deserialize, Serialize)]
struct LawRow {
    decision_id: String,
    // Option, not String+default: Python rows carry null for a missing
    // decision body or ts, and serde's `default` covers absent keys only.
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    ts: Option<String>,
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
struct ExactHit {
    subject: String,
    ids: Vec<String>,
}

#[derive(Serialize)]
struct NearbyHit {
    decision_id: String,
    subject: String,
    decision: String,
    shared: Vec<String>,
}

#[derive(Serialize)]
struct AskAnswer {
    ok: bool,
    exact: Vec<ExactHit>,
    nearby: Vec<NearbyHit>,
    uncited: Vec<String>,
    nearby_refusal: Option<String>,
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

fn ask_answer(req: &AskRequest) -> AskAnswer {
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

/// One law line: `- <id> (<subject>): <first sentence, 160 chars>`.
fn stage_law_line(row: &Value) -> Option<String> {
    let id = row.get("decision_id").and_then(Value::as_str)?;
    let subject = row.get("subject").and_then(Value::as_str).unwrap_or("");
    let decision = row.get("decision").and_then(Value::as_str).unwrap_or("");
    Some(format!(
        "- {id} ({subject}): {}",
        one_line(first_sentence(decision), 160)
    ))
}

/// Which laws a stage block lists: keyword match over `{subject} {decision}`,
/// or the payload node's subjects (node id, epic id, project slug) named in
/// either field - minus rows whose subject equals the node id itself (the
/// think-inspect receipt already carries the node's own rulings).
fn stage_matching_lines(
    index: &decision_index::Index,
    keywords: &[&str],
    idents: &[String],
    node_id: &str,
) -> Vec<String> {
    index
        .rows
        .iter()
        .filter(|row| {
            let subject = row.get("subject").and_then(Value::as_str).unwrap_or("");
            // The node id is carried separately: `idents` is sorted, so its
            // first element is whichever subject sorts smallest (often the
            // project slug), never reliably the node id.
            let node_row = !node_id.is_empty() && subject.trim().eq_ignore_ascii_case(node_id);
            !node_row
        })
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

/// Node subjects for a stage payload that names a node: the node id itself,
/// its epic id (the graph row's `parent`) and the project slug, read through
/// the crate's graph read. An unreadable graph degrades to node-id-only
/// matching and returns the reason so the stage answer can name the missing scope.
fn node_subject_idents(
    node_id: &str,
    graph_path: Option<&std::path::Path>,
) -> (Vec<String>, Option<String>) {
    let mut idents = vec![node_id.to_lowercase()];
    let default_path = crate::graph_get::default_graph_path();
    let path = graph_path.unwrap_or(&default_path);
    if graph_path.is_none() && crate::graph_get::external_backend_selected() {
        return (idents, Some("graph: external backend selected".to_owned()));
    }
    let entries = match crate::backlog::api::rows(&crate::backlog::api::Store::new(path)) {
        Ok(entries) => entries,
        Err(error) => return (idents, Some(format!("graph: {}", error.0))),
    };
    if let Some(entry) = crate::graph_get::find_entry(&entries, node_id) {
        for field in ["parent", "project"] {
            if let Some(v) = entry.get(field).and_then(Value::as_str) {
                let v = v.trim().to_lowercase();
                if !v.is_empty() {
                    idents.push(v);
                }
            }
        }
    }
    idents.sort();
    idents.dedup();
    (idents, None)
}

/// The context block for a stage with laws: cap 2000 bytes, first law line
/// always renders, and every law past the cap keeps a short id line the
/// validator parses, so overflow law is acknowledged, not hidden.
fn render_stage_block(stage: &str, matching: &[String], damaged: usize) -> String {
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
/// input; a failed read is a report, never silence.
fn stage_answer_with(
    req: StageRequest,
    index_path: Option<&std::path::Path>,
    graph_path: Option<&std::path::Path>,
) -> Value {
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
        let (idents, graph_unread) = node_id
            .as_deref()
            .map(|id| node_subject_idents(id, graph_path))
            .unwrap_or_default();
        if let Some(reason) = graph_unread {
            unread.push(format!("the node's epic and project ({reason})"));
        }
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

/// The statement validator, a word-for-word port of
/// `validate_durable_law` (`cli/src/fno/law.py`) plus the one rule that
/// module cannot own: a bare node id or `pr-<n>` subject is refused, because
/// a ruling found only by the id of the work that prompted it is unfindable.
fn validate_answer(req: &ValidateRequest) -> Value {
    let refusal = if req.subject.trim().is_empty() || req.decision.trim().is_empty() {
        Some("subject and decision are required".to_string())
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

/// `fno-agents law-match`: the hidden binary-direct transport. One JSON
/// request on stdin, one JSON answer on stdout, exit 0 whenever an answer
/// was computed; exit 2 on malformed args or an unreadable request.
pub fn run_law_match(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "usage: fno-agents law-match (one JSON request on stdin: mode=ask|law|stage|validate)"
        );
        return 0;
    }
    if !args.is_empty() {
        eprintln!("fno-agents law-match: unexpected arguments; the request rides stdin");
        return 2;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("fno-agents law-match: could not read stdin");
        return 2;
    }
    let req: MatchRequest = match serde_json::from_str(&input) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fno-agents law-match: bad request: {e}");
            return 2;
        }
    };
    let answer = match req {
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
        let answer = stage_answer_with(StageRequest { hook }, Some(&path), None);
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
        let answer = stage_answer_with(StageRequest { hook }, Some(&path), None);
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
            StageRequest { hook },
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
            let answer = stage_answer_with(StageRequest { hook: hook.clone() }, Some(&path), None);
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
        let answer = stage_answer_with(StageRequest { hook }, Some(&path), None);
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
        let answer = stage_answer_with(StageRequest { hook }, Some(&path), None);
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
                .any(|l| l.starts_with(&format!("- {id} (review-cap-fixture):")));
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
        let answer = stage_answer_with(StageRequest { hook }, Some(&path), None);
        let ctx = answer["hook_output"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("context present");
        assert!(ctx.contains("- d-verbs0001 (top-level-verbs):"), "{ctx}");
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
        let answer = stage_answer_with(StageRequest { hook }, Some(&path), None);
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
        let answer = stage_answer_with(StageRequest { hook }, Some(&path), Some(&graph));
        let ctx = answer["hook_output"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("context present");
        assert!(ctx.contains("d-epic0001"), "{ctx}");
        assert!(
            !ctx.contains("unfindable by topic"),
            "node-subject row must be dropped: {ctx}"
        );
        assert!(
            ctx.contains("d-fnos0001"),
            "project-slug subject must not be dropped: {ctx}"
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
            let answer = stage_answer_with(StageRequest { hook }, Some(&path), None);
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
        let (idents, unread) = node_subject_idents("x-aaaa", Some(&graph));
        assert_eq!(idents, vec!["fno", "x-aaaa", "x-bbbb"]);
        assert!(unread.is_none());
        let (missing, missing_unread) = node_subject_idents("x-ffff", Some(&graph));
        assert_eq!(missing, vec!["x-ffff"]);
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
        let answer = stage_answer_with(StageRequest { hook }, Some(&index), Some(&graph));
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
        let answer = stage_answer_with(StageRequest { hook }, Some(&index), Some(&graph));
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
        let answer = stage_answer_with(StageRequest { hook }, Some(&index), Some(&graph));
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
        let answer = stage_answer_with(StageRequest { hook }, Some(&index), Some(&graph));
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
