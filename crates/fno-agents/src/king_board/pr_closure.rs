//! The one parser and renderer for the PR-body closure line.
//!
//! The line format lives here and nowhere else: the Python `closure.py`
//! readers forward to this module through `fno-agents pr closure parse|render`,
//! so Python and Rust can never disagree about what a body claims. Writers
//! emit only the `Fixes` spelling; readers also accept the retired
//! `Backlog-Closure:` spelling until no open PR carries it.
//!
//! Grammar, one rule for both spellings: the keyword sits at the START of a
//! line and matches case-insensitively, with or without a colon. Every token
//! after it is a well-formed node id, split by commas and/or spaces. If any
//! token is not a node id, the line is prose and claims nothing. The LAST
//! matching line wins; a prose line is not a match and never erases an
//! earlier good one.

use super::queues::NODE_ID_BODY;
use serde_json::{json, Value};

/// The keywords a closure line may start with. `Fixes` is the only spelling
/// writers render; `Backlog-Closure` is read for PRs opened before the rename.
const KEYWORDS: [&str; 2] = ["fixes", "backlog-closure"];

/// The well-formed, deduplicated ids ONE line claims; empty when the line is
/// prose (no keyword, or any malformed token).
pub(crate) fn line_ids(line: &str) -> Vec<String> {
    let Some(rest) = closure_line_rest(line) else {
        return Vec::new();
    };
    parse_line_tokens(rest).unwrap_or_default()
}

/// The rest of `line` after a closure keyword, when the line opens with one.
/// `None` on any line whose head is not keyword + (colon or whitespace).
fn closure_line_rest(line: &str) -> Option<&str> {
    for kw in KEYWORDS {
        // get(), never [..len]: a line opening with a multibyte character
        // makes the keyword length a non-boundary, and a bare slice panics.
        if line.len() >= kw.len()
            && line
                .get(..kw.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(kw))
        {
            let rest = &line[kw.len()..];
            return match rest.chars().next() {
                Some(':') => Some(&rest[1..]),
                Some(c) if c.is_whitespace() => Some(rest),
                None => Some(rest),
                // Glued text ("fixated") is a word, not the keyword.
                Some(_) => None,
            };
        }
    }
    None
}

/// The well-formed, deduplicated ids in the token soup after the keyword.
/// `None` when any token is malformed: the line is prose and claims nothing.
fn parse_line_tokens(rest: &str) -> Option<Vec<String>> {
    let id_re = regex::Regex::new(&format!("^{NODE_ID_BODY}$")).expect("static regex");
    let mut ids: Vec<String> = Vec::new();
    for token in rest
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
    {
        if !id_re.is_match(token) {
            return None;
        }
        if !ids.iter().any(|i| i == token) {
            ids.push(token.to_string());
        }
    }
    Some(ids)
}

/// Well-formed node ids claimed by the LAST well-formed closure line of
/// `body`, order-preserved, deduplicated. A candidate line carrying any
/// malformed token is prose: it claims nothing and does not win.
pub(crate) fn parse(body: &str) -> Vec<String> {
    let mut last: Option<Vec<String>> = None;
    for line in body.lines() {
        let Some(rest) = closure_line_rest(line) else {
            continue;
        };
        if let Some(ids) = parse_line_tokens(rest) {
            last = Some(ids);
        }
    }
    last.unwrap_or_default()
}

/// The one spelling writers emit: `Fixes <ids>`, deduplicated, well-formed
/// ids only; empty when nothing well-formed remains, so a caller can append
/// the result to a body unconditionally.
pub(crate) fn render<I, S>(node_ids: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let id_re = regex::Regex::new(&format!("^{NODE_ID_BODY}$")).expect("static regex");
    let mut ids: Vec<String> = Vec::new();
    for id in node_ids {
        let id = id.as_ref();
        if id_re.is_match(id) && !ids.iter().any(|i| i == id) {
            ids.push(id.to_string());
        }
    }
    if ids.is_empty() {
        String::new()
    } else {
        format!("Fixes {}", ids.join(" "))
    }
}

/// `fno-agents pr-closure-parse|pr-closure-render`: the Python forwarders'
/// door. One JSON payload on stdin, one JSON answer on stdout (the house
/// transport `fno.rust_binary.verb_call` speaks). Parse reads `{"body": s}`
/// and answers `{"ids": [...]}`; render reads `{"ids": [...]}` and answers
/// `{"line": s}` (empty when nothing well-formed remains).
pub fn run(args: &[String]) -> i32 {
    use std::io::Read;

    let mut payload = String::new();
    if std::io::stdin().read_to_string(&mut payload).is_err() {
        eprint!("pr-closure: cannot read stdin\n");
        return 2;
    }
    let parsed: Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => {
            eprint!("pr-closure: bad payload: {e}\n");
            return 2;
        }
    };
    match args.first().map(String::as_str) {
        Some("pr-closure-parse") => {
            let body = parsed.get("body").and_then(Value::as_str).unwrap_or("");
            println!(
                "{}",
                serde_json::to_string(&json!({ "ids": parse(body) }))
                    .unwrap_or_else(|_| "{\"ids\":[]}".into())
            );
            0
        }
        Some("pr-closure-render") => {
            let ids: Vec<String> = parsed
                .get("ids")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            println!(
                "{}",
                serde_json::to_string(&json!({ "line": render(ids) }))
                    .unwrap_or_else(|_| "{\"line\":\"\"}".into())
            );
            0
        }
        _ => {
            eprint!(
                "usage: fno-agents pr-closure-parse   (JSON {{body}} in; {{ids}} out)\n       fno-agents pr-closure-render  (JSON {{ids}} in; {{line}} out)\n"
            );
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/pr-closure-cases.json")
    }

    /// The shared corpus is the one contract both legs read. A missing or
    /// malformed fixture fails here, never silently passes.
    #[test]
    fn the_shared_corpus_parses_as_the_claims_column_says() {
        let text = std::fs::read_to_string(corpus_path()).expect("shared corpus fixture");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("corpus is JSON");
        let cases = parsed
            .as_object()
            .expect("corpus object")
            .get("cases")
            .expect("cases array");
        let cases = cases.as_array().expect("cases array");
        assert!(
            cases.len() >= 7,
            "the corpus carries at least the plan's seven cases"
        );
        for case in cases {
            let body = case
                .get("body")
                .and_then(|b| b.as_str())
                .expect("case body");
            let want: Vec<String> = case
                .get("claims")
                .and_then(|c| c.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(parse(body), want, "corpus case failed: {body:?}");
        }
    }

    #[test]
    fn last_wellformed_line_wins_and_prose_never_earases_it() {
        let body = "Fixes x-aaaa\nsome text\nBacklog-Closure: x-bbbb\n";
        assert_eq!(parse(body), vec!["x-bbbb"]);
        // A malformed keyword line is prose: it claims nothing AND does not
        // win, so the earlier good line keeps its claims.
        let body = "Fixes x-aaaa\nFixes the thing.\n";
        assert_eq!(parse(body), vec!["x-aaaa"]);
    }

    #[test]
    fn render_emits_only_the_new_spelling_and_round_trips() {
        assert_eq!(render(["x-aaaa", "x-bbbb"]), "Fixes x-aaaa x-bbbb");
        assert_eq!(
            parse(&render(["x-aaaa", "x-bbbb"])),
            vec!["x-aaaa", "x-bbbb"]
        );
        assert_eq!(render(["not-an-id", "x-aaaa", "x-aaaa"]), "Fixes x-aaaa");
        assert_eq!(render(Vec::<String>::new()), "");
    }

    #[test]
    fn a_multibyte_line_never_panics_the_keyword_slice() {
        // A body line opening with a multibyte character puts a non-char
        // boundary at the keyword length; the prefix check must read None,
        // not panic.
        let body = "日本語の行です。\nFixes x-aaaa\n";
        assert_eq!(parse(body), vec!["x-aaaa"]);
        assert_eq!(line_ids("日本語の行です。"), Vec::<String>::new());
    }
}
