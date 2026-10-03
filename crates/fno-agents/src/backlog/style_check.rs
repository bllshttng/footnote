//! The prose style check at the tool boundary, ported from `cli/src/fno/style.py`
//! for the encounter surface: `--evidence` on a node birth must keep its gate
//! with Python gone. Pure: no filesystem, no state, no network. Rules 1 to 8
//! with the masking pass first, so code costs one word and quoted spans never
//! trip the word-level rules. The Rust regex crate has no lookbehind,
//! lookahead, or backreferences, so four spots are hand-rolled to the same
//! contract: sentence splitting, the condition keyword, flag masking, and the
//! thematic-break line shape.

use std::collections::BTreeSet;
use std::path::PathBuf;
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
/// CONTINUING line is charged. `only` scopes the report to those 1-based
/// lines: rule 6 still reads the line above a kept line (its state advances
/// on EVERY line), so context comes from the whole text while the charge
/// lands only on a line the caller named.
fn run(text: &str) -> Vec<Violation> {
    run_scoped(text, None)
}

fn run_scoped(text: &str, only: Option<&BTreeSet<usize>>) -> Vec<Violation> {
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
        let in_scope = only.is_none() || only.is_some_and(|set| set.contains(&(i + 1)));
        if !blank && !starts_block && prev_continuable && in_scope {
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
        if blank || !in_scope {
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

/// Check only `line_numbers` (1-based), and report only on those lines. The
/// whole text is masked first, so an added line inside an existing fenced
/// block is masked as code and skipped rather than read as prose. Used by
/// the added-lines markdown gate, where the diff supplies only `+` lines.
pub fn check_lines(text: &str, line_numbers: &BTreeSet<usize>) -> Vec<Violation> {
    run_scoped(text, Some(line_numbers))
}

// Rules a rewrite can clear without an author: a semicolon splits into two
// sentences, a wrapped paragraph rejoins into one physical line. The rest
// change meaning when applied blind, so they stay in the residue.
fn is_fixable(rule: u32) -> bool {
    rule == 2 || rule == 6
}

/// Rewrite the mechanically fixable violations. Return (text, residue).
/// Pure. Re-checks after every pass because a fix can expose another; a pass
/// that changes nothing ends the loop. A non-empty residue is the caller's
/// signal to exit non-zero: a partial fix never reads as a pass.
pub fn fix(text: &str, surface: &str) -> (String, Vec<Violation>) {
    let mut text = text.to_string();
    for _ in 0..10 {
        let violations = check(&text, surface, None);
        if !violations.iter().any(|v| is_fixable(v.rule)) {
            return (text, violations);
        }
        let fixed = apply_fixes(&text, &violations);
        if fixed == text {
            return (text, violations);
        }
        text = fixed;
    }
    let violations = check(&text, surface, None);
    (text, violations)
}

fn apply_fixes(text: &str, violations: &[Violation]) -> String {
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    // Joins run bottom-up so deleting a line cannot shift a pending index.
    let mut joins: Vec<usize> = violations
        .iter()
        .filter(|v| v.rule == 6)
        .map(|v| v.sentence_index)
        .collect();
    joins.sort_unstable();
    joins.dedup();
    for i in joins.into_iter().rev() {
        if 0 < i && i < lines.len() {
            let below = lines.remove(i);
            let above = lines[i - 1].trim_end().to_string();
            lines[i - 1] = format!("{above} {}", below.trim_start());
        }
    }
    let joined = lines.join("\n");
    // A line whose masked form differs carries a construct (code span, fence,
    // path) with no offset map back to raw text, so its semicolons stay in the
    // residue rather than risk a split inside the span. The mask runs on the
    // whole text: fence state spans lines.
    let masked = mask(&joined);
    let masked_lines: Vec<&str> = masked.split('\n').collect();
    joined
        .split('\n')
        .zip(masked_lines)
        .map(|(line, masked)| {
            if line == masked {
                split_semicolons(line)
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

static_re!(semicolon_space_re, r";[ \t]+");
static_re!(trailing_semicolon_re, r";[ \t]*$");

/// Turn `a; b` into `a. B` and a trailing `;` into a period.
fn split_semicolons(line: &str) -> String {
    let parts: Vec<&str> = semicolon_space_re().split(line).collect();
    if parts.len() == 1 {
        return trailing_semicolon_re().replace(line, ".").into_owned();
    }
    let mut out = parts[0].to_string();
    for part in &parts[1..] {
        let mut chars = part.chars();
        match chars.next() {
            Some(c) if c.is_lowercase() => {
                out.push_str(". ");
                out.extend(c.to_uppercase());
                out.push_str(chars.as_str());
            }
            _ => {
                out.push_str(". ");
                out.push_str(part);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The hidden `style-check` verb: the full Python `fno doctor lint style`
// contract (cli/src/fno/lint_cli.py `style` + its diff helpers). Without
// --json the byte contract is the frozen goldens; with --json it is the door
// the Python callers exec.
// ---------------------------------------------------------------------------

const STYLE_SURFACES: [&str; 5] = ["mail", "encounter", "pr-body", "markdown", "comment"];

/// Repo root, Python `resolve_repo_root` order: FNO_REPO_ROOT pin, then
/// `git rev-parse --show-toplevel` from cwd, then cwd.
fn repo_root() -> PathBuf {
    if let Some(pin) = std::env::var_os("FNO_REPO_ROOT") {
        let path = std::path::PathBuf::from(&pin);
        if let Ok(resolved) = path.canonicalize() {
            return resolved;
        }
        return path;
    }
    if let Ok(out) = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// The ONLY way this module spells a rename-aware, quote-safe `git diff`.
fn pinned_diff_argv(tail: &[&str]) -> Vec<String> {
    let mut argv: Vec<String> = [
        "git",
        "-c",
        "diff.renames=true",
        "-c",
        "diff.renameLimit=0",
        "-c",
        "core.quotePath=false",
        "diff",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    argv.extend(tail.iter().map(|s| s.to_string()));
    argv
}

/// Run git, and refuse to let a failure read as a clean result: empty stdout
/// is also the clean answer, so an unchecked return code turns any git error
/// into a green gate. Returns (stdout, stderr).
fn run_git(argv: &[String], repo: &std::path::Path, label: &str) -> Result<(String, String), i32> {
    let out = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(repo)
        .output();
    match out {
        Err(e) => {
            eprintln!("style: {label} failed: {e}");
            Err(2)
        }
        Ok(o) if !o.status.success() => {
            eprintln!(
                "style: {label} failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            Err(2)
        }
        Ok(o) => Ok((
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        )),
    }
}

/// Added-line count per NEW path, from `git diff --numstat -z`. `-z` is the
/// whole point: without it git compresses a rename into one display form, so
/// the new path is never a key.
fn numstat_added(raw: &str) -> std::collections::BTreeMap<String, usize> {
    let mut added = std::collections::BTreeMap::new();
    let fields: Vec<&str> = raw.split('\0').collect();
    let mut i = 0;
    while i < fields.len() {
        let parts: Vec<&str> = fields[i].split('\t').collect();
        if parts.len() < 3 {
            i += 1;
            continue;
        }
        let count = parts[0];
        let mut path = parts[2];
        let mut step = 1;
        if path.is_empty() {
            // Rename: the old path is the next field, the new path the one after.
            path = fields.get(i + 2).copied().unwrap_or("");
            step = 3;
        }
        i += step;
        if let Ok(n) = count.parse::<usize>() {
            if !path.is_empty() {
                added.insert(path.to_string(), n);
            }
        }
    }
    added
}

/// Lexically resolve a caller path to an absolute form (Python Path.resolve
/// with strict=False: symlinks resolved when the file exists, absolute and
/// normalized otherwise).
fn resolve_lenient(p: &str) -> PathBuf {
    let path = std::path::Path::new(p);
    if let Ok(canon) = path.canonicalize() {
        return canon;
    }
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Did `rel` exist at `diff_base`? A false answer here is the whole point of
/// the call, so NOT routed through run_git: any failure reads as "not at the
/// base", which keeps the refusal above fail-closed.
fn existed_at_base(rel: &str, diff_base: &str, repo: &std::path::Path) -> bool {
    std::process::Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["cat-file", "-e", &format!("{diff_base}:{rel}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Repo-root-relative POSIX pathspecs for git, from caller-relative paths.
fn repo_scope(
    paths: &[String],
    repo: &std::path::Path,
    diff_base: &str,
) -> Result<Vec<String>, i32> {
    let root = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let mut out = Vec::new();
    for p in paths {
        let full = resolve_lenient(p);
        let rel = match full.strip_prefix(&root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => {
                eprintln!("style: --files path is outside the repository: {p}");
                return Err(2);
            }
        };
        // Absence from the working tree is still a legitimate scope when the
        // branch deleted or renamed the file; absent at the base too means the
        // path never existed on either side, which refuses.
        if !full.exists() && !existed_at_base(&rel, diff_base, repo) {
            eprintln!(
                "style: --files path does not exist: {p} (resolved to {}), \
and it is not in {diff_base} either. Paths resolve against the current directory.",
                full.display()
            );
            return Err(2);
        }
        out.push(rel);
    }
    Ok(out)
}

fn style_skip_receipt(names: &[String], err: bool) {
    if names.is_empty() {
        return;
    }
    let text = format!(
        "style: skipped {} input(s) by style-exception, so no line in them was read:\n  {}",
        names.len(),
        names.join("\n  ")
    );
    if err {
        eprintln!("{text}");
    } else {
        println!("{text}");
    }
}

fn style_refuse_zero_read() -> i32 {
    eprintln!(
        "style: read 0 lines, because every input carried a style-exception, \
so nothing was checked."
    );
    2
}

/// The added-lines scan: (violations, inspected, changed-files, unexplained).
/// Per changed markdown file anywhere in the repo, only the ADDED lines since
/// diff_base are checked. A style-exception marker does not skip a file here:
/// a marker at the top of a file cannot scope a line written later.
fn style_added_lines(
    diff_base: &str,
    files: &[String],
) -> Result<(Vec<Violation>, usize, usize, Vec<String>), i32> {
    let repo = repo_root();
    // Verified BEFORE scope resolution, because scope resolution asks the base
    // whether a missing path existed there.
    run_git(
        &[
            "git".to_string(),
            "rev-parse".to_string(),
            "--verify".to_string(),
            diff_base.to_string(),
        ],
        &repo,
        &format!("bad diff-base '{diff_base}'"),
    )?;
    let scope = if files.is_empty() {
        Vec::new()
    } else {
        repo_scope(files, &repo, diff_base)?
    };
    let range = format!("{diff_base}...HEAD");
    let mut name_only_tail: Vec<&str> = vec!["--name-only", &range, "--"];
    name_only_tail.extend(scope.iter().map(String::as_str));
    let (diff_files, _) = run_git(
        &pinned_diff_argv(&name_only_tail),
        &repo,
        &format!("listing changed files ({diff_base}...HEAD)"),
    )?;
    // Pre-rename paths, keyed by new path, from an UNSCOPED name-status pass:
    // rename detection needs both sides visible, which a per-file pathspec denies.
    let (name_status_stdout, name_status_stderr) = run_git(
        &pinned_diff_argv(&["--name-status", &format!("{diff_base}...HEAD")]),
        &repo,
        &format!("rename detection ({diff_base}...HEAD)"),
    )?;
    if name_status_stderr.contains("rename detection was skipped") {
        eprintln!(
            "style: git skipped rename detection despite the pinned limit ({}); \
refusing to bill moved files as authored prose",
            name_status_stderr.trim()
        );
        return Err(2);
    }
    let mut renames: std::collections::BTreeMap<String, String> = Default::default();
    for line in name_status_stdout.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() == 3 && parts[0].starts_with('R') {
            renames.insert(parts[2].to_string(), parts[1].to_string());
        }
    }
    // -U0 here is load-bearing: git computes a DIFFERENT edit script at zero
    // context than at the default three, and both sides of the count guard
    // below must read the same diff.
    let (numstat, _) = run_git(
        &pinned_diff_argv(&["--numstat", "-z", "-U0", &format!("{diff_base}...HEAD")]),
        &repo,
        &format!("counting changed lines ({diff_base}...HEAD)"),
    )?;
    let added_by_path = numstat_added(&numstat);
    // Markdown only: the gate is "changed markdown".
    let changed: Vec<&str> = diff_files
        .lines()
        .filter(|l| !l.trim().is_empty() && l.ends_with(".md"))
        .collect();
    let mut violations = Vec::new();
    let mut inspected = 0;
    let mut unexplained = Vec::new();
    for rel in &changed {
        let full = repo.join(rel);
        if !full.is_file() {
            // git lists it as changed and it is absent from the working tree.
            // A deletion is a legitimate zero; a file git says ADDED lines to
            // is the dirty-tree loss, routed to the count guard.
            if added_by_path.get(*rel).copied().unwrap_or(0) > 0 {
                unexplained.push((*rel).to_string());
            }
            continue;
        }
        let whole = match std::fs::read_to_string(&full) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("style: could not read {}: {e}", full.display());
                return Err(1);
            }
        };
        let nums =
            git_added_line_nums(rel, diff_base, &repo, renames.get(*rel).map(String::as_str))?;
        inspected += nums.len();
        if !nums.is_empty() {
            // Mask the WHOLE file and check only the added lines, so an added
            // line inside an existing fenced block is masked as code and skipped.
            violations.extend(check_lines(&whole, &nums));
        }
        if nums.len() != added_by_path.get(*rel).copied().unwrap_or(0) {
            // git and the parser disagree about how many lines this file
            // added: the INSTRUMENT failed. Paths are collected, since a
            // count is not investigable.
            unexplained.push((*rel).to_string());
        }
    }
    Ok((violations, inspected, changed.len(), unexplained))
}

/// 1-based line numbers (in the new file) of added (`+`) lines from
/// `git diff -U0 <base>...HEAD`. Position advances on context and added
/// lines, not on deleted lines, matching how the new file is laid out.
/// `old_rel` is the pre-rename path; passing it is what keeps a MOVED file
/// from reading as an authored one.
fn git_added_line_nums(
    rel: &str,
    diff_base: &str,
    repo: &std::path::Path,
    old_rel: Option<&str>,
) -> Result<BTreeSet<usize>, i32> {
    let range = format!("{diff_base}...HEAD");
    let mut tail: Vec<&str> = vec!["-U0", &range, "--", rel];
    if let Some(old) = old_rel {
        tail.push(old);
    }
    let (stdout, _) = run_git(
        &pinned_diff_argv(&tail),
        repo,
        &format!("reading added lines for {rel}"),
    )?;
    Ok(parse_added_line_nums(&stdout))
}

/// The parser half of `git_added_line_nums`, pure over diff stdout so the
/// line-numbering contracts unit-test without a repo.
fn parse_added_line_nums(stdout: &str) -> BTreeSet<usize> {
    static HUNK_NUM_RE: OnceLock<regex::Regex> = OnceLock::new();
    let hunk_re = HUNK_NUM_RE.get_or_init(|| regex::Regex::new(r"\+(\d+)").expect("hunk regex"));
    let mut nums: BTreeSet<usize> = BTreeSet::new();
    let mut pos: usize = 0;
    let mut in_hunk = false;
    for line in stdout.lines() {
        if line.starts_with("diff --git") {
            // Reset per FILE: the pathspec can carry two paths, so a diff that
            // comes back as two entries must not carry hunk state across.
            in_hunk = false;
        } else if line.starts_with("@@") {
            pos = hunk_re
                .captures(line)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(pos);
            in_hunk = true;
        } else if !in_hunk {
            // File headers live BEFORE the first hunk, so position is what
            // separates them from content, not the +++ / --- prefix.
            continue;
        } else if line.starts_with('\\') {
            // `\ No newline at end of file` is a NOTE about the adjacent
            // line, never a line of the file.
            continue;
        } else if line.starts_with('+') {
            nums.insert(pos);
            pos += 1;
        } else if line.starts_with('-') {
            continue;
        } else {
            pos += 1;
        }
    }
    nums
}

fn violations_json(violations: &[Violation]) -> serde_json::Value {
    serde_json::Value::Array(
        violations
            .iter()
            .map(|v| {
                serde_json::json!({
                    "rule": v.rule,
                    "sentence_index": v.sentence_index,
                    "sentence": v.sentence,
                    "detail": v.detail,
                })
            })
            .collect(),
    )
}

/// Run one style check and print the JSON door receipt: always exit 0.
fn run_json(text: &str, surface: &str, word_cap: Option<usize>) -> i32 {
    let exception = has_exception(text);
    let count = word_count(text);
    let violations = check(text, surface, word_cap);
    let report = format_violations(&violations, surface);
    let payload = serde_json::json!({
        "exception": exception,
        "word_count": count,
        "violations": violations_json(&violations),
        "report": report,
    });
    println!("{payload}");
    0
}

/// Parse `--word-cap`, mirroring Python's `word_cap or MESSAGE_WORD_CAP`: a
/// zero selects the default, never a zero cap.
fn parse_word_cap(raw: &str) -> Result<Option<usize>, i32> {
    match raw.parse::<usize>() {
        Ok(n) => Ok(if n == 0 { None } else { Some(n) }),
        Err(_) => {
            eprintln!("style: --word-cap expects an integer, got {raw:?}");
            Err(2)
        }
    }
}

/// The hidden binary-direct `style-check` verb: the full
/// `fno doctor lint style` contract, from `--stdin`/`--text`/`--files`/
/// `--diff-base` to the receipts and exit codes the goldens freeze.
pub fn run_cli(args: &[String]) -> i32 {
    let mut surface = String::from("mail");
    let mut stdin_mode = false;
    let mut text_arg: Option<String> = None;
    let mut files: Vec<String> = Vec::new();
    let mut diff_base: Option<String> = None;
    let mut fix_mode = false;
    let mut json_mode = false;
    let mut word_cap: Option<usize> = None;
    let mut it = args.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--surface" => surface = it.next().cloned().unwrap_or_default(),
            "--stdin" => stdin_mode = true,
            "--text" => text_arg = it.next().cloned(),
            "--files" => {
                if let Some(f) = it.next() {
                    files.push(f.clone());
                }
            }
            "--diff-base" => diff_base = it.next().cloned(),
            "--fix" => fix_mode = true,
            "--json" => json_mode = true,
            "--word-cap" => match it.next().map(|v| parse_word_cap(v)) {
                Some(Ok(cap)) => word_cap = cap,
                Some(Err(code)) => return code,
                None => {
                    eprintln!("style: --word-cap requires a value");
                    return 2;
                }
            },
            other => {
                eprintln!("style: unknown argument {other:?}");
                return 2;
            }
        }
    }

    if !STYLE_SURFACES.contains(&surface.as_str()) {
        // Spelled from the list, never beside it.
        let known = STYLE_SURFACES.join(", ");
        eprintln!("style: unknown surface '{surface}' ({known})");
        return 2;
    }
    if diff_base.is_some() && surface != "markdown" {
        eprintln!("style: --diff-base applies to --surface markdown only.");
        return 2;
    }
    if fix_mode && diff_base.is_some() {
        eprintln!("style: --fix rewrites whole inputs; it does not combine with --diff-base.");
        return 2;
    }

    if json_mode {
        // The --json door always exits 0; the receipt carries the verdict.
        // It reads one input (--text or stdin) and never prints the
        // human-mode receipts, so dispatch here, before those receipts run.
        let text = match &text_arg {
            Some(t) => t.clone(),
            None => {
                let mut buf = String::new();
                if std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).is_err() {
                    eprintln!("style: could not read stdin");
                    return 1;
                }
                buf
            }
        };
        return run_json(&text, &surface, word_cap);
    }

    let mut violations: Vec<Violation> = Vec::new();
    if let Some(base) = &diff_base {
        match style_added_lines(base, &files) {
            Err(code) => return code,
            Ok((found, inspected, changed, unexplained)) => {
                violations = found;
                println!(
                    "style: inspected {inspected} added line(s) across {changed} changed file(s)."
                );
                if !unexplained.is_empty() {
                    // Reported AFTER the violations, never instead of them.
                    // Exit 2, not 1: an instrument failure, not a finding
                    // against the author. The paths are named, because a
                    // count is not investigable.
                    if !violations.is_empty() {
                        eprintln!("{}", format_violations(&violations, &surface));
                    }
                    eprintln!(
                        "style: git and this gate disagree about how many lines these \
file(s) added, so some added lines went unread. That is a parser failure in the \
gate rather than something to annotate in the file:\n  {}",
                        unexplained.join("\n  ")
                    );
                    return 2;
                }
            }
        }
    } else if stdin_mode || text_arg.is_some() {
        let text = match &text_arg {
            Some(t) => t.clone(),
            None => {
                let mut buf = String::new();
                if std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).is_err() {
                    eprintln!("style: could not read stdin");
                    return 1;
                }
                buf
            }
        };
        // Receipts go to stderr here, because --fix hands the body back on stdout.
        if has_exception(&text).is_some() {
            style_skip_receipt(&["<stdin>".to_string()], true);
            if fix_mode {
                print!("{text}");
            }
            return style_refuse_zero_read();
        }
        eprintln!(
            "style: inspected {} line(s) across 1 input(s).",
            text.lines().count()
        );
        if fix_mode {
            let (fixed, residue) = fix(&text, &surface);
            print!("{fixed}");
            violations = residue;
        } else {
            violations = check(&text, &surface, word_cap);
        }
    } else if !files.is_empty() {
        let mut skipped: Vec<String> = Vec::new();
        let mut read_lines = 0;
        for path in &files {
            let text = match std::fs::read_to_string(path) {
                Ok(text) => text,
                Err(e) => {
                    eprintln!("style: could not read {path}: {e}");
                    return 1;
                }
            };
            if has_exception(&text).is_some() {
                skipped.push(path.clone());
                continue;
            }
            read_lines += text.lines().count();
            if fix_mode {
                let (fixed, residue) = fix(&text, &surface);
                if fixed != text {
                    if let Err(e) = std::fs::write(path, fixed) {
                        eprintln!("style: could not write {path}: {e}");
                        return 1;
                    }
                }
                violations.extend(residue);
            } else {
                violations.extend(check(&text, &surface, word_cap));
            }
        }
        style_skip_receipt(&skipped, false);
        println!(
            "style: inspected {read_lines} line(s) across {} input(s).",
            files.len() - skipped.len()
        );
        if !skipped.is_empty() && read_lines == 0 {
            return style_refuse_zero_read();
        }
    } else {
        eprintln!("style: pass --stdin, --files, or --diff-base.");
        return 2;
    }

    if violations.is_empty() {
        return 0;
    }
    eprintln!("{}", format_violations(&violations, &surface));
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_newline_marker_does_not_shift_added_line_numbers() {
        let diff = "\
diff --git a/n.md b/n.md
@@ -1,2 +1,2 @@
 one
-strict;
+strict
\\ No newline at end of file
+added line;
";
        assert_eq!(parse_added_line_nums(diff), BTreeSet::from([2, 3]));
    }

    #[test]
    fn content_line_starting_with_plus_plus_is_not_read_as_a_header() {
        let diff = "\
diff --git a/n.md b/n.md
@@ -0,0 +1,3 @@
++ b/looks-like-a-header
++ but is content
+third;
";
        assert_eq!(parse_added_line_nums(diff), BTreeSet::from([1, 2, 3]));
    }

    #[test]
    fn two_entry_diff_resets_hunk_state_between_files() {
        let diff = "\
diff --git a/n.md b/n.md
@@ -1 +1 @@
-old
+new
diff --git a/m.md b/m.md
@@ -1 +1 @@
-context
+added;
";
        assert_eq!(parse_added_line_nums(diff), BTreeSet::from([1]));
    }

    #[test]
    fn numstat_rename_row_keys_the_new_path() {
        let raw = "3\t1\tnotes.md\0old.md\0notes.md\0\02\t0\tplain.md\0";
        let added = numstat_added(raw);
        assert_eq!(added.get("notes.md"), Some(&3));
        assert_eq!(added.get("plain.md"), Some(&2));
    }

    #[test]
    fn check_lines_reports_only_the_given_lines() {
        let text = "first; line\n\nsecond; line\n";
        let only = BTreeSet::from([3usize]);
        let violations = check_lines(text, &only);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, 2);
        assert!(
            violations[0].detail.contains("line 3"),
            "{:?}",
            violations[0].detail
        );
    }

    #[test]
    fn added_line_inside_an_existing_fence_is_masked_as_code() {
        // The fence opens on line 1; the added line 2 is inside it, so the
        // whole-text mask blanks it and the added-lines gate reads no prose.
        let text = "```text\nadded; semicolon line\n```\n";
        let only = BTreeSet::from([2usize]);
        assert!(check_lines(text, &only).is_empty());
    }

    #[test]
    fn fix_splits_semicolons_and_rejoins_wraps() {
        let (fixed, residue) = fix("Do this; do that.\nAlso this\ncontinues here.\n", "mail");
        assert!(residue.is_empty(), "{residue:?}");
        assert_eq!(fixed, "Do this. Do that.\nAlso this continues here.\n");
    }

    #[test]
    fn fix_reports_unfixable_residue() {
        let (fixed, residue) = fix("You should do this; do that.\n", "mail");
        assert_eq!(fixed, "You should do this. Do that.\n");
        assert!(residue.iter().any(|v| v.rule == 3), "{residue:?}");
    }

    #[test]
    fn word_cap_zero_selects_the_default() {
        assert_eq!(parse_word_cap("0").unwrap(), None);
        assert_eq!(parse_word_cap("40").unwrap(), Some(40));
        assert!(parse_word_cap("lots").is_err());
    }

    #[test]
    fn message_cap_refusal_names_the_enforced_number() {
        let body = "word ".repeat(81);
        let violations = check(&body, "mail", None);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, 7);
        assert!(violations[0].detail.contains("The cap is 80 words."));
        let none = check(&"word ".repeat(80), "mail", None);
        assert!(none.iter().all(|v| v.rule != 7));
    }
}
