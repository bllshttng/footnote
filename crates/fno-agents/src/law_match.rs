//! The question-to-law matcher : one pure function set behind a hidden
//! JSON verb, reached as `fno-agents law-match`. Reads no file - Python keeps
//! the decision lifecycle read (`list_decisions`) and the open-question fold
//! (`read_open_questions`) and works only on rows handed to it; this side
//! only matches.

use serde::{Deserialize, Serialize};
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
fn law_answer(req: &LawRequest) -> LawAnswer {
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
    let mut lines: Vec<String> = Vec::new();
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

/// `fno-agents law-match`: the hidden binary-direct transport. One JSON
/// request on stdin, one JSON answer on stdout, exit 0 whenever an answer
/// was computed; exit 2 on malformed args or an unreadable request.
pub fn run_law_match(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: fno-agents law-match (one JSON request on stdin: mode=ask|law)");
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
        MatchRequest::Law(r) => serde_json::to_string(&law_answer(&r)).expect("serializes"),
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
        let ans = law_answer(&req);
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
        let ans = law_answer(&req);
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
}
