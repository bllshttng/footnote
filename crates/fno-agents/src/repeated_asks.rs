//! Reads the king's last 20 end-of-turn replies and names asks repeated there.
//! An ask belongs on a question page, never in chat.

use std::{
    collections::{HashSet, VecDeque},
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
    sync::OnceLock,
};

use regex::Regex;
use serde_json::{json, Value};

const REPLY_WINDOW: usize = 20;
const REPEAT_FLOOR: usize = 3;
const SAME_ASK: f64 = 0.6;
const PRINT_CHARS: usize = 100;
const ASK: &[&str] = &[
    "waiting on you",
    "waiting on your",
    "your call",
    "your pick",
    "your decision",
    "your approval",
    "your go",
    "needs you",
    "need you",
    "needs your",
    "need your",
    "reply ",
    "say so",
    "say yes",
    "tell me",
    "let me know",
    "want me to",
    "should i ",
    "shall i ",
    "may i ",
    "please run",
    "can you ",
    "confirm",
];
const START: &[&str] = &["answer ", "approve ", "pick ", "choose "];
const NOT_ASK: &[&str] = &[
    "nothing needs you",
    "nothing needs your",
    "nothing needed",
    "no decision needed",
];

pub(crate) fn reading() -> Result<Value, String> {
    let path = crate::king_checkin::own_claude_transcript()?;
    Ok(fold(&last_replies(&path)?))
}

fn last_replies(path: &Path) -> Result<Vec<String>, String> {
    // ponytail: reads the whole transcript each beat, as refusal_rate does; seek to a tail window if the check-in's reader time matters.
    let file = File::open(path).map_err(|e| format!("transcript unreadable: {e}"))?;
    let mut replies = VecDeque::with_capacity(REPLY_WINDOW);
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| format!("transcript unreadable: {e}"))?;
        if !line.contains("\"end_turn\"") {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let message = row.get("message").unwrap_or(&Value::Null);
        if row.get("type").and_then(Value::as_str) != Some("assistant")
            || row.get("isSidechain").and_then(Value::as_bool) == Some(true)
            || message.get("stop_reason").and_then(Value::as_str) != Some("end_turn")
        {
            continue;
        }
        let text = crate::loopcheck::extract_assistant_text(&row);
        if !text.trim().is_empty() {
            replies.push_back(text);
            if replies.len() > REPLY_WINDOW {
                replies.pop_front();
            }
        }
    }
    Ok(replies.into_iter().collect())
}

fn asks_in(reply: &str) -> Vec<String> {
    let mut asks = Vec::new();
    for sentence in sentences(reply) {
        if sentence.trim_start().starts_with('|') {
            continue;
        }
        let normalized = normalize(sentence);
        if normalized.chars().count() < 8
            || NOT_ASK.iter().any(|marker| normalized.contains(marker))
        {
            continue;
        }
        if normalized.ends_with('?')
            || START.iter().any(|marker| normalized.starts_with(marker))
            || ASK
                .iter()
                .any(|marker| format!("{normalized} ").contains(marker))
        {
            asks.push(normalized);
        }
    }
    asks
}

fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for (index, (byte, ch)) in chars.iter().copied().enumerate() {
        let end = byte + ch.len_utf8();
        let next = chars.get(index + 1).map(|(_, c)| *c);
        if ch == '\n' || (matches!(ch, '.' | '?' | '!') && next.is_some_and(char::is_whitespace)) {
            if start < end {
                out.push(&text[start..end]);
            }
            start = end;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn normalize(sentence: &str) -> String {
    let mut text: String = sentence
        .to_lowercase()
        .chars()
        .filter(|ch| !matches!(ch, '*' | '`' | '"' | '“' | '”' | '>' | '#'))
        .collect();
    text = strip_list_marker(&text).to_string();
    let trimmed = text.trim_start();
    for prefix in ["next:", "also:"] {
        if trimmed.starts_with(prefix) {
            text = trimmed[prefix.len()..].to_string();
            break;
        }
    }
    text = clock_regex().replace_all(&text, "").into_owned();
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .trim_end_matches(['.', '!', ':', ';', ','])
        .to_string()
}

fn strip_list_marker(text: &str) -> &str {
    let text = text.trim_start();
    for marker in ["- ", "+ "] {
        if let Some(rest) = text.strip_prefix(marker) {
            return rest;
        }
    }
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && text.as_bytes().get(digits) == Some(&b'.') {
        let after = digits + 1;
        if text
            .as_bytes()
            .get(after)
            .is_some_and(u8::is_ascii_whitespace)
        {
            return &text[after + 1..];
        }
    }
    text
}

fn clock_regex() -> &'static Regex {
    static CLOCK: OnceLock<Regex> = OnceLock::new();
    CLOCK.get_or_init(|| Regex::new(r"\b\d{1,2}:\d{2}z?\b").expect("clock regex is valid"))
}

fn tokens(text: &str) -> HashSet<String> {
    static TOKEN: OnceLock<Regex> = OnceLock::new();
    let regex =
        TOKEN.get_or_init(|| Regex::new(r"[a-z0-9][a-z0-9_.'-]*").expect("token regex is valid"));
    regex
        .find_iter(text)
        .map(|m| m.as_str().to_string())
        .collect()
}

fn same_ask(a: &str, b: &str) -> bool {
    let a = tokens(a);
    let b = tokens(b);
    let union = a.union(&b).count();
    a.intersection(&b).count() as f64 / union.max(1) as f64 >= SAME_ASK
}

fn fold(replies: &[String]) -> Value {
    let mut clusters: Vec<(String, String, usize)> = Vec::new();
    for reply in replies {
        let mut counted = HashSet::new();
        for ask in asks_in(reply) {
            if let Some((index, cluster)) = clusters
                .iter_mut()
                .enumerate()
                .find(|(_, cluster)| same_ask(&ask, &cluster.0))
            {
                if counted.insert(index) {
                    cluster.2 += 1;
                }
                cluster.1 = ask;
            } else {
                let index = clusters.len();
                clusters.push((ask.clone(), ask, 1));
                counted.insert(index);
            }
        }
    }
    let mut repeated: Vec<_> = clusters
        .into_iter()
        .filter(|cluster| cluster.2 >= REPEAT_FLOOR)
        .collect();
    repeated.sort_by(|a, b| b.2.cmp(&a.2));
    json!({
        "replies": replies.len(),
        "asks": repeated.into_iter().map(|(_, text, count)| {
            json!({"text": text.chars().take(PRINT_CHARS).collect::<String>(), "count": count})
        }).collect::<Vec<_>>(),
    })
}

pub(crate) fn lines(readings: &[crate::king_checkin::Reading]) -> Vec<String> {
    let Some(reading) = readings
        .iter()
        .find(|reading| reading.name == "repeated_asks")
    else {
        return Vec::new();
    };
    if !reading.ok {
        return vec![format!("READER FAILED repeated_asks: {}", reading.error)];
    }
    let asks = reading
        .value
        .get("asks")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if asks.is_empty() {
        let replies = reading
            .value
            .get("replies")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        return vec![format!("repeated asks: none in the last {replies} replies")];
    }
    let mut lines: Vec<String> = asks
        .iter()
        .map(|ask| {
            let text = ask.get("text").and_then(Value::as_str).unwrap_or("");
            let count = ask.get("count").and_then(Value::as_u64).unwrap_or(0);
            format!("repeated ask: {text} x{count}")
        })
        .collect();
    lines.push("  remedy: file it as a question page: fno inbox outstanding ask --question-file <file> --node <id>".into());
    lines.push("  remedy: or decide it yourself under law d-1726ee1c (reversible, with a recommendation) and tell the user it is reversible".into());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::king_checkin::Reading;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn asks_rows() {
        let named = NamedTempFile::new().unwrap();
        let transcript = named.path().to_path_buf();
        let mut file = named.reopen().unwrap();
        let f = "Next: reply \"f90d build\" or \"f90d drop\". I recommend drop.";
        let y = "Next: reply \"yes to all four\" on the model-catalog questions, or name the ones you'd change.";
        let y2 = "Next: reply \"yes to all four\" on the model-catalog questions. Questions 2 and 4 change what t-x-aaaa is building right now.";
        let status = "The next check-in runs at 06:36.\n\nNothing needs you right now.\n\n| 2377 | x-aaaa (won't-do, your answer c) |";
        let mut rows = Vec::new();
        rows.extend((0..3).map(|_| {
            (
                "Next: reply \"sol or wait\".".to_string(),
                "end_turn",
                false,
            )
        }));
        rows.extend((0..5).map(|_| (format!("{status}\n\n{f}"), "end_turn", false)));
        rows.push((
            format!("Reply \"f90d build\" or \"f90d drop\".\n\n{f}"),
            "end_turn",
            false,
        ));
        rows.extend((0..3).map(|_| (format!("{status}\n\n{y}"), "end_turn", false)));
        rows.push((y2.to_string(), "end_turn", false));
        rows.extend((0..2).map(|_| ("Still waiting on you.".to_string(), "end_turn", false)));
        rows.extend((0..5).map(|_| (status.to_string(), "end_turn", false)));
        rows.extend((0..3).map(|_| ("🤔🤔?".to_string(), "end_turn", false)));
        rows.extend((0..4).map(|_| ("Should I merge PR 2558?".to_string(), "tool_use", false)));
        rows.extend((0..4).map(|_| ("Should I merge PR 2558?".to_string(), "end_turn", true)));
        for (text, stop, sidechain) in rows {
            let row = json!({"type":"assistant", "isSidechain":sidechain, "message":{"role":"assistant", "stop_reason":stop, "content":[{"type":"text", "text":text}]}});
            writeln!(file, "{row}").unwrap();
        }
        drop(file);
        let folded = fold(&last_replies(&transcript).unwrap());
        assert_eq!(folded["replies"], 20);
        assert_eq!(
            folded["asks"],
            json!([
                {"text":"reply f90d build or f90d drop", "count":6},
                {"text":"reply yes to all four on the model-catalog questions", "count":4},
            ])
        );

        let repeated = Reading::took(
            "repeated_asks",
            json!({"replies":20,"asks":[{"text":"reply f90d build or drop", "count":3}]}),
        );
        let rendered = lines(&[repeated]);
        assert_eq!(rendered.len(), 3);
        assert_eq!(rendered[0], "repeated ask: reply f90d build or drop x3");
        assert!(rendered[1].contains("question page"));
        assert!(rendered[2].contains("d-1726ee1c"));
        assert_eq!(
            lines(&[Reading::took(
                "repeated_asks",
                json!({"replies":20,"asks":[]})
            )]),
            ["repeated asks: none in the last 20 replies"]
        );
        assert_eq!(
            lines(&[Reading::failed(
                "repeated_asks",
                "transcript unreadable".into()
            )]),
            ["READER FAILED repeated_asks: transcript unreadable"]
        );
        assert!(lines(&[]).is_empty());
    }
}
