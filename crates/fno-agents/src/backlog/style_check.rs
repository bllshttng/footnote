//! The prose style check at the tool boundary, ported from `cli/src/fno/style.py`
//! for the encounter surface: `--evidence` on a node birth must keep its gate
//! with Python gone. Pure: no filesystem, no state, no network. Rules 1 to 8
//! with the masking pass first, so code costs one word and quoted spans never
//! trip the word-level rules. The Rust regex crate has no lookbehind,
//! lookahead, or backreferences, so four spots are hand-rolled to the same
//! contract: sentence splitting, the condition keyword, flag masking, and the
//! thematic-break line shape.

use std::collections::BTreeSet;
use std::sync::OnceLock;

/// One rule breach on one sentence. `sentence` carries the masked text, so
/// the word count reported in `detail` matches what the reader sees.
#[derive(Debug, Clone)]
pub struct Violation {
    pub rule: u32,
    pub sentence_index: usize,
    pub sentence: String,
    pub detail: String,
}

const LIST_ITEM_CAP: usize = 20;
const PARAGRAPH_CAP: usize = 25;
const MESSAGE_WORD_CAP: usize = 80;
const EXCERPT_CAP: usize = 12;
const PLACEHOLDER: &str = "x";

/// Rule 3 modals. Matched lowercase whole-word; "may" is lowercase-only so
/// the month never fires.
fn banned_modals() -> &'static BTreeSet<&'static str> {
    static SET: OnceLock<BTreeSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| ["should", "would", "might", "could"].into_iter().collect())
}

/// Rule 8 filler words.
fn banned_fillers() -> &'static BTreeSet<&'static str> {
    static SET: OnceLock<BTreeSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| ["please", "thanks", "basically"].into_iter().collect())
}

/// Rule 8 filler phrases, matched in the lowered sentence.
const BANNED_FILLER_PHRASES: [&str; 4] = ["thank you", "of course", "happy to", "feel free"];

/// Rule 4. A closed list, matched case-insensitively after normalising the
/// curly apostrophe.
fn contractions() -> &'static BTreeSet<&'static str> {
    static SET: OnceLock<BTreeSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        [
            "ain't",
            "aren't",
            "can't",
            "could've",
            "couldn't",
            "daren't",
            "didn't",
            "doesn't",
            "don't",
            "hadn't",
            "hasn't",
            "haven't",
            "he'd",
            "he'll",
            "he's",
            "here's",
            "how'd",
            "how'll",
            "how's",
            "i'd",
            "i'll",
            "i'm",
            "i've",
            "isn't",
            "it'd",
            "it'll",
            "it's",
            "let's",
            "ma'am",
            "might've",
            "mightn't",
            "must've",
            "mustn't",
            "needn't",
            "o'clock",
            "oughtn't",
            "shan't",
            "she'd",
            "she'll",
            "she's",
            "should've",
            "shouldn't",
            "that'd",
            "that'll",
            "that's",
            "there's",
            "they'd",
            "they'll",
            "they're",
            "they've",
            "'tis",
            "'twas",
            "we'd",
            "we'll",
            "we're",
            "we've",
            "weren't",
            "what'll",
            "what're",
            "what's",
            "what've",
            "when's",
            "where's",
            "who'd",
            "who'll",
            "who're",
            "who's",
            "who've",
            "why's",
            "won't",
            "would've",
            "wouldn't",
            "y'all",
            "you'd",
            "you'll",
            "you're",
            "you've",
        ]
        .into_iter()
        .collect()
    })
}

fn condition_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Python used \b(if|when)(?![\w-]); the crate has no lookahead, so
        // the rejected follow-up char is consumed instead and the capture
        // group pins the keyword start.
        regex::Regex::new(r"(?i)\b(if|when)(?:[^A-Za-z0-9_-]|$)").expect("condition regex")
    })
}

macro_rules! static_re {
    ($name:ident, $pattern:expr) => {
        fn $name() -> &'static regex::Regex {
            static RE: OnceLock<regex::Regex> = OnceLock::new();
            RE.get_or_init(|| regex::Regex::new($pattern).expect("style regex"))
        }
    };
}

static_re!(line_exception_re, r"(?m)^\s*style-exception:\s*(.+?)\s*$");
static_re!(
    comment_exception_re,
    r"<!--\s*style-exception:\s*(.+?)\s*-->"
);
static_re!(heading_re, r"^\s{0,3}#{1,6}\s");
static_re!(html_block_re, r"^\s{0,3}</?[A-Za-z][A-Za-z0-9_-]*");
static_re!(ref_def_re, r"^\s{0,3}\[[^\]]+\]:\s");
static_re!(field_line_re, r"^\s{0,3}[A-Za-z][A-Za-z0-9_.-]*:[ \t]");
static_re!(blockquote_re, r"^\s{0,3}>");
static_re!(fence_open_re, r"^[ \t]*(`{3,}|~{3,})");
static_re!(
    log_re,
    r"^(\[[A-Z]{2,}\]|ERROR\b|WARN(?:ING)?\b|INFO\b|DEBUG\b|TRACE\b|\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2})"
);
static_re!(delimiter_row_re, r"^:?-+:?(?:[ \t]*\|[ \t]*:?-+:?)+[ \t]*$");
static_re!(list_marker_re, r"^\s{0,3}([-*+]|\d+[.)])\s+");
static_re!(comment_span_re, r"<!--.*?-->");
static_re!(code_double_re, r"``([^`]+)``");
static_re!(code_single_re, r"`([^`]+)`");
static_re!(link_re, r"\[([^\]]+)\]\([^)]*\)");
static_re!(quote_re, r#""[^"]*""#);
static_re!(url_re, r"[A-Za-z][A-Za-z0-9_.+-]*://\S*");
static_re!(
    path_re,
    r"\b[A-Za-z][A-Za-z0-9_-]*(?:::|/)[A-Za-z0-9_./:-]*"
);
static_re!(
    filename_re,
    r"\b[A-Za-z0-9_./-]+\.(?:py|sh|rs|ts|js|toml|ya?ml|json|md|txt|lock)\b"
);
// The ident mask emulates Python's atomic-group trick with a contains('_')
// filter on each plain word token.
static_re!(ident_re, r"\b[A-Za-z][A-Za-z0-9_]*");
// The flag mask consumes one non-word char in front of the flag because the
// crate has no lookbehind; the replacement re-emits it.
static_re!(
    flag_re,
    r"(?:^|([^A-Za-z0-9_-]))(--[A-Za-z][A-Za-z0-9_-]*|-[A-Za-z])"
);

/// True when the raw line opens a table run: a delimiter row, or the header
/// directly above one. A paragraph above a setext underline is untouched.
fn starts_table_run(raw_line: &str, lines: &[&str], index: usize) -> bool {
    let lead = raw_line.trim_start();
    if delimiter_row_re().is_match(lead)
        || (lead.starts_with('|')
            && delimiter_row_re().is_match(lead.trim_matches(['|', ' ', '\t'])))
    {
        return true;
    }
    if !raw_line.contains('|') {
        return false;
    }
    let Some(nxt) = lines.get(index).map(|l| l.trim_start()) else {
        return false;
    };
    delimiter_row_re().is_match(nxt)
        || (nxt.starts_with('|') && delimiter_row_re().is_match(nxt.trim_matches(['|', ' ', '\t'])))
}

/// Python's re.match anchors at the string start; this helper mirrors that
/// for the anchored line-shape regexes.
fn match_at_start(pattern: &regex::Regex, line: &str) -> bool {
    pattern.find(line).map(|m| m.start()) == Some(0)
}

/// The own-line break shapes: a setext underline, or a 3+-of-a-kind thematic
/// break. The crate has no backreference, so the run shape is hand-checked.
fn own_line_break(lead: &str) -> bool {
    let t = lead.trim_end_matches([' ', '\t']);
    if t.is_empty() {
        return false;
    }
    if !t.is_empty() && t.bytes().all(|b| b == b'=') {
        return true;
    }
    if t.bytes().all(|b| b == b'-') {
        return true;
    }
    // Thematic break: one of - * _ repeated 3+ times, optional single spaces/tabs
    // between, and nothing else on the line.
    let run: Vec<u8> = t.bytes().filter(|b| *b != b' ' && *b != b'\t').collect();
    run.len() >= 3 && matches!(run[0], b'-' | b'*' | b'_') && run.iter().all(|&b| b == run[0])
}

/// Replace code constructs with placeholders and blank non-prose lines.
/// Every input line maps to one output line, so a caller can zip raw and
/// masked lines to read block type off the raw line.
pub fn mask(text: &str) -> String {
    let text = text.replace("\r\n", "\n");
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut in_fence = false;
    let mut fence_char = ' ';
    let mut in_comment = false;
    let mut in_frontmatter = false;
    for (index, raw_line) in lines.iter().enumerate() {
        let lead = raw_line.trim_start();
        // Frontmatter: a leading `--- ... ---` block, blanked line-for-line.
        if index == 0 && lead == "---" {
            in_frontmatter = true;
            out.push(String::new());
            continue;
        }
        if in_frontmatter {
            if lead == "---" {
                in_frontmatter = false;
            }
            out.push(String::new());
            continue;
        }
        if in_comment {
            if raw_line.contains("-->") {
                in_comment = false;
            }
            out.push(String::new());
            continue;
        }
        if let Some(fence) = fence_open_re().captures(lead) {
            let ch = fence.get(1).and_then(|m| m.as_str().chars().next());
            if !in_fence {
                in_fence = true;
                if let Some(c) = ch {
                    fence_char = c;
                }
            } else if Some(fence_char) == ch {
                in_fence = false;
                fence_char = ' ';
            }
            out.push(String::new());
            continue;
        }
        if in_fence {
            out.push(String::new());
            continue;
        }
        if raw_line.starts_with("    ") || raw_line.starts_with('\t') {
            out.push(String::new());
            continue;
        }
        if lead.starts_with("<!--") {
            if !raw_line.contains("-->") {
                in_comment = true;
                out.push(String::new());
                continue;
            }
            // A same-line comment falls through: mask_inline strips just the
            // span, so prose trailing the comment is still checked.
        }
        // A leading-pipe table row is removed outright. A pipeless delimiter
        // row is NOT blanked here; its rule-6 exemption lives in run(),
        // scoped to that rule only.
        if lead.starts_with('|') || delimiter_row_re().is_match(lead) {
            out.push(String::new());
            continue;
        }
        if log_re().is_match(lead) {
            out.push(String::new());
            continue;
        }
        out.push(mask_inline(raw_line));
    }
    out.join("\n")
}

/// Inline masking: code spans, links, quoted spans, flags, paths, URLs,
/// dotted filenames, and underscore identifiers become one placeholder each.
fn mask_inline(line: &str) -> String {
    // None of the patterns can match a line without punctuation or marker
    // characters; the fast path keeps long plain bodies linear.
    if !line.contains(['<', '`', '[', '"', '-', '/', ':', '_', '.']) {
        return line.to_string();
    }
    let mut line = comment_span_re().replace_all(line, "").into_owned();
    line = code_double_re()
        .replace_all(&line, PLACEHOLDER)
        .into_owned();
    line = code_single_re()
        .replace_all(&line, PLACEHOLDER)
        .into_owned();
    line = link_re().replace_all(&line, "${1}").into_owned();
    line = quote_re().replace_all(&line, PLACEHOLDER).into_owned();
    line = flag_re().replace_all(&line, "${1}x").into_owned();
    line = url_re().replace_all(&line, PLACEHOLDER).into_owned();
    line = path_re().replace_all(&line, PLACEHOLDER).into_owned();
    line = filename_re().replace_all(&line, PLACEHOLDER).into_owned();
    let mut rebuilt = String::with_capacity(line.len());
    let mut last = 0;
    for m in ident_re().find_iter(&line) {
        if !m.as_str().contains('_') {
            continue;
        }
        rebuilt.push_str(&line[last..m.start()]);
        rebuilt.push_str(PLACEHOLDER);
        last = m.end();
    }
    rebuilt.push_str(&line[last..]);
    rebuilt
}

/// Split a masked line on sentence enders, protecting abbreviations. The
/// crate has no lookbehind, so the split boundary is scanned by hand: a
/// [.!?] followed by whitespace.
fn split_sentences(line: &str) -> Vec<String> {
    let mut protected = line.to_string();
    for abbr in ["e.g.", "i.e.", "vs.", "etc."] {
        protected = protected.replace(abbr, &abbr.replace('.', "\u{0}"));
    }
    let mut parts: Vec<String> = Vec::new();
    let mut start = 0;
    let bytes = protected.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !matches!(bytes[i], b'.' | b'!' | b'?') {
            i += 1;
            continue;
        }
        // The ender, then one or more whitespace chars: a split boundary.
        let mut j = i + 1;
        while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
            j += 1;
        }
        if j == i + 1 {
            i += 1;
            continue;
        }
        parts.push(protected[start..i + 1].to_string());
        start = j;
        i = j;
    }
    parts.push(protected[start..].to_string());
    parts
        .into_iter()
        .map(|p| p.replace('\u{0}', "."))
        .filter(|p| !p.trim().is_empty())
        .collect()
}

/// Return the first non-initial if/when, or None if it leads the sentence.
fn condition_keyword(sentence: &str) -> Option<String> {
    let body = list_marker_re().replace_all(sentence, "");
    let matches: Vec<regex::Captures> = condition_re().captures_iter(&body).collect();
    if matches.is_empty() {
        return None;
    }
    if matches[0].get(1).expect("kw").start() == 0 && matches.len() == 1 {
        return None;
    }
    for m in &matches {
        let start = m.get(1).expect("kw").start();
        if start != 0 {
            return Some(m.get(1).expect("kw").as_str().to_lowercase());
        }
    }
    None
}

/// Rule 3/8 word strip set: . , ; : ! ? " ' ( )
fn strip_word(word: &str) -> &str {
    word.trim_matches(|c| ".,;:!?\"'()".contains(c))
}

/// Rule 4 strip set drops the apostrophe from the trim set.
fn strip_token(word: &str) -> String {
    word.replace('\u{2019}', "'")
        .trim_matches(|c| ".,;:!?\"()".contains(c))
        .to_string()
}

fn check_sentence(sentence: &str, index: usize, is_list: bool) -> Vec<Violation> {
    let mut out = Vec::new();
    let shown = index + 1;
    let cap = if is_list {
        LIST_ITEM_CAP
    } else {
        PARAGRAPH_CAP
    };
    let words: Vec<&str> = sentence.split_whitespace().collect();
    if words.len() > cap {
        out.push(Violation {
            rule: 1,
            sentence_index: index,
            sentence: sentence.to_string(),
            detail: format!(
                "sentence {shown} is {count} words. The cap is {cap}.",
                count = words.len()
            ),
        });
    }
    if sentence.contains(';') {
        out.push(Violation {
            rule: 2,
            sentence_index: index,
            sentence: sentence.to_string(),
            detail: format!("sentence {shown} has a semicolon. Split it into two sentences."),
        });
    }
    for word in words.iter() {
        let stripped = strip_word(word);
        let lowered = stripped.to_lowercase();
        if banned_modals().contains(lowered.as_str()) || stripped == BANNED_MODAL_MAY {
            out.push(Violation {
                rule: 3,
                sentence_index: index,
                sentence: sentence.to_string(),
                detail: format!(
                    "sentence {shown} uses \"{}\". Write \"can\", \"will\", or \"must\" instead.",
                    quote_safe(word)
                ),
            });
        }
        if banned_fillers().contains(lowered.as_str()) {
            out.push(Violation {
                rule: 8,
                sentence_index: index,
                sentence: sentence.to_string(),
                detail: format!(
                    "sentence {shown} uses the filler \"{}\". Delete it. The imperative alone reads stronger.",
                    quote_safe(word)
                ),
            });
        }
    }
    let lowered_sentence = sentence.to_lowercase();
    for phrase in BANNED_FILLER_PHRASES {
        if lowered_sentence.contains(phrase) {
            out.push(Violation {
                rule: 8,
                sentence_index: index,
                sentence: sentence.to_string(),
                detail: format!("sentence {shown} uses the filler phrase \"{phrase}\". Delete it."),
            });
        }
    }
    for word in words.iter() {
        let token = strip_token(word);
        if contractions().contains(token.as_str()) {
            out.push(Violation {
                rule: 4,
                sentence_index: index,
                sentence: sentence.to_string(),
                detail: format!(
                    "sentence {shown} has the contraction \"{}\". Write the words out.",
                    quote_safe(word)
                ),
            });
        }
    }
    if let Some(keyword) = condition_keyword(sentence) {
        out.push(Violation {
            rule: 5,
            sentence_index: index,
            sentence: sentence.to_string(),
            detail: format!(
                "sentence {shown} puts \"{keyword}\" after the command. The condition must start the sentence."
            ),
        });
    }
    out
}

fn quote_safe(s: &str) -> String {
    s.replace('"', "'")
}

const BANNED_MODAL_MAY: &str = "may";

/// The rule loop: mask whole, then walk the lines holding rule 6 state. A
/// blank or block-initial line does not continue a paragraph; only the
/// CONTINUING line is charged.
fn run(text: &str) -> Vec<Violation> {
    let masked_full = mask(text);
    let owned_text = text.replace("\r\n", "\n");
    let raw_lines: Vec<&str> = owned_text.split('\n').collect();
    let masked_lines: Vec<&str> = masked_full.split('\n').collect();
    let mut violations = Vec::new();
    let mut sentence_index = 0;
    let mut prev_continuable = false;
    let mut in_table = false;
    for (i, raw_line) in raw_lines.iter().enumerate() {
        let masked_line = masked_lines[i];
        let blank = masked_line.trim().is_empty();
        let is_list = list_marker_re().is_match(raw_line);
        if blank || !raw_line.contains('|') {
            in_table = false;
        }
        let table_row = starts_table_run(raw_line, &raw_lines, i + 1);
        if table_row {
            in_table = true;
        }
        let own_line = table_row
            || (in_table && raw_line.contains('|') && !blank)
            || match_at_start(heading_re(), raw_line)
            || own_line_break(raw_line.trim_start())
            || match_at_start(html_block_re(), raw_line)
            || match_at_start(ref_def_re(), raw_line)
            || match_at_start(field_line_re(), raw_line);
        let starts_block = is_list || own_line || match_at_start(blockquote_re(), raw_line);
        // Only the CONTINUING line is charged, never the line above it.
        if !blank && !starts_block && prev_continuable {
            violations.push(Violation {
                rule: 6,
                sentence_index: i,
                sentence: masked_line.trim().to_string(),
                detail: format!(
                    "line {} continues the paragraph above. A paragraph is one physical line. Join the two lines, or put a blank line between them.",
                    i + 1
                ),
            });
        }
        prev_continuable = !blank && !own_line;
        if blank {
            continue;
        }
        let work = if is_list {
            list_marker_re().replace(raw_line, "").into_owned()
        } else {
            masked_line.to_string()
        };
        for sentence in split_sentences(&work) {
            violations.extend(check_sentence(&sentence, sentence_index, is_list));
            sentence_index += 1;
        }
    }
    violations
}

fn rule_names(rule: &u32) -> &'static str {
    match rule {
        1 => "length",
        2 => "semicolon",
        3 => "modal",
        4 => "contraction",
        5 => "condition",
        6 => "wrap",
        7 => "wordcap",
        8 => "filler",
        _ => "?",
    }
}

fn excerpt(v: &Violation) -> String {
    let quoted = quote_safe(&v.sentence);
    let mut words: Vec<&str> = quoted.split_whitespace().collect();
    if words.len() > 12 {
        words.truncate(12);
        words.push("...");
    }
    words.join(" ")
}

fn quote_extract(detail: &str) -> Option<String> {
    let start = detail.find('"')?;
    let rest = &detail[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Render violations as a self-teaching refusal message that itself passes
/// rules 1 to 8.
pub fn format_violations(violations: &[Violation], surface: &str) -> String {
    if violations.is_empty() {
        return String::new();
    }
    let mut lines = vec!["message blocked by the style rules.".to_string()];
    let mut by_rule: std::collections::BTreeMap<u32, Vec<&Violation>> = Default::default();
    for v in violations {
        by_rule.entry(v.rule).or_default().push(v);
    }
    for (rule, list) in &by_rule {
        for v in list {
            lines.push(format!(
                "rule {rule} ({}): {} \"{}\"",
                rule_names(rule),
                v.detail,
                excerpt(v)
            ))
        }
    }
    if by_rule.contains_key(&7) {
        lines.push("Cut articles, filler, pleasantries, hedges. Fragments work. Keep technical terms exact.".into());
        lines.push("Status: X. Why Y. Done at Z.".into());
        lines.push(
            "Approval: Problem X. Options Y or Z. I recommend Z because A. Your call?".into(),
        );
        lines.push("Put findings on the node and link it.".into());
    }
    if by_rule.contains_key(&3) || by_rule.contains_key(&4) {
        let first = by_rule
            .get(&3)
            .or_else(|| by_rule.get(&4))
            .and_then(|v| v.first());
        let demo = first
            .and_then(|v| quote_extract(&v.detail))
            .unwrap_or_else(|| "the refused word".to_string());
        lines.push("A quoted word is a mention, not a use.".into());
        lines.push(format!(
            "Wrap \"{demo}\" in double quotes or backticks to name it."
        ));
        lines.push("This refusal does that with every word it names.".into());
    }
    if surface == "markdown" {
        lines.push("commit the rewrite, then run \"fno doctor lint style --surface markdown --diff-base <base>\" to check it.".into());
        return lines.join("\n\n");
    }
    lines.push("add a style-exception line with a reason, or pass --style-exception.".into());
    if by_rule.contains_key(&7) {
        let named = if surface.is_empty() { "mail" } else { surface };
        lines.push(format!(
            "run \"fno doctor lint style --stdin --surface {named}\" to check a rewrite first. Fewer words."
        ));
    } else {
        lines.push(
            "run \"fno doctor lint style --stdin --surface pr-body\" to check a rewrite first."
                .into(),
        );
    }
    lines.join("\n\n")
}

/// Return the bypass reason when the text carries a style-exception marker:
/// the line form (``style-exception: why``) or the HTML-comment form. An
/// empty reason does not count.
pub fn has_exception(text: &str) -> Option<String> {
    let m = line_exception_re()
        .captures(text)
        .or_else(|| comment_exception_re().captures(text))?;
    let reason = m.get(1)?.as_str().trim().to_string();
    if reason.is_empty() {
        return None;
    }
    Some(reason)
}

/// Masked prose word count for one string.
pub fn word_count(text: &str) -> usize {
    mask(text).split_whitespace().count()
}

/// Rule 7 against `cap`, which the refusal names. The number is an argument
/// rather than a module read so the refusal always states the cap enforced.
fn check_message_length(text: &str, cap: usize) -> Vec<Violation> {
    let masked = mask(text);
    let count = word_count(text);
    if count <= cap {
        return Vec::new();
    }
    let first_line = masked.lines().next().unwrap_or_default().trim().to_string();
    vec![Violation {
        rule: 7,
        sentence_index: 0,
        sentence: first_line,
        detail: format!("this message runs {count} words. The cap is {cap} words."),
    }]
}

/// Return every violation found in `text`. Every surface runs rules 1 to 8;
/// the capped surfaces (mail, encounter) also carry the prose word cap.
pub fn check(text: &str, surface: &str, word_cap: Option<usize>) -> Vec<Violation> {
    let mut violations = run(text);
    if matches!(surface, "mail" | "encounter") {
        violations.extend(check_message_length(
            text,
            word_cap.unwrap_or(MESSAGE_WORD_CAP),
        ));
    }
    violations
}
