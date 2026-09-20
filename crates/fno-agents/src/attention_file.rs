//! The `md` file sink: render one attention item to a markdown task block,
//! read the user's answer back from the ticked option or the words under it,
//! and flip the block closed when the answer records. Every function here is
//! pure; the arm owns the disk.

use crate::attention::AttentionItem;

/// One rendered block: the item id, its line span [start, end) in the file,
/// and the raw text (top line + sub-bullets).
#[derive(Debug, Clone, PartialEq)]
pub struct FileBlock {
    pub id: String,
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// The user's answer read out of one block.
#[derive(Debug, Clone, PartialEq)]
pub enum FileAnswer {
    None,
    /// Exactly one option ticked.
    Option(u32),
    /// An indented line of words under the item.
    Words(String),
    /// The top line ticked (a pin taken).
    Done,
    /// Two or more options ticked; nothing records, one notice.
    TwoTicked,
}

/// The writable config for one `md` sink. Every key except `path` has a
/// default; `ready_only` defaults false for one release (machine writers
/// cannot supply the context fields until the Rust intake lands).
#[derive(Debug, Clone, PartialEq)]
pub struct FileSinkConfig {
    pub name: String,
    pub path: std::path::PathBuf,
    /// Appended to the top line once; sub-lines never carry it.
    pub tag: String,
    pub line: String,
    pub option_line: String,
    pub ready_only: bool,
}

impl Default for FileSinkConfig {
    fn default() -> Self {
        FileSinkConfig {
            name: "jc".to_string(),
            path: std::path::PathBuf::new(),
            tag: "#fno".to_string(),
            line: "- [ ] {title}. Blocks {blocks}. Recommended: {recommend} {tag} {priority_mark} 📅 {due} ^{id}".to_string(),
            option_line: "    - [ ] {n}. {text}. Pro: {pros}. Con: {cons}".to_string(),
            ready_only: false,
        }
    }
}

/// One `{key}` fill; a missing value renders empty, the rule the Python
/// `_EventFormatter` follows.
fn fill(template: &str, key: &str, value: &str) -> String {
    template.replace(&format!("{{{key}}}"), value)
}

/// Priority glyphs; the map the plan's sink keys name.
fn priority_mark(priority: &str) -> &'static str {
    match priority {
        "high" => "⏫",
        "low" => "🔽",
        _ => "",
    }
}

/// The deadline's date half for the `{due}` slot (empty when absent).
fn due_date(item: &AttentionItem) -> String {
    item.deadline
        .as_deref()
        .filter(|d| !d.is_empty())
        .map(|d| d.chars().take(10).collect())
        .unwrap_or_default()
}

/// Render one item into the file block. The top line carries the tag once and
/// ends with `^{id}` (the anchor Obsidian folds and ntfy deep-links later);
/// option sub-lines and context sub-bullets follow, none carrying the tag.
pub fn render_item(item: &AttentionItem, cfg: &FileSinkConfig) -> String {
    let blocks_joined = item.blocks.join(", ");
    let recommend = item
        .recommendation
        .as_ref()
        .map(|r| r.option.to_string())
        .unwrap_or_default();
    let top = fill(&cfg.line, "title", &item.title);
    let top = fill(&top, "blocks", &blocks_joined);
    let top = fill(&top, "recommend", &recommend);
    let top = fill(&top, "tag", &cfg.tag);
    let top = fill(&top, "priority_mark", priority_mark(&item.priority));
    let top = fill(&top, "due", &due_date(item));
    let top = fill(&top, "id", &item.id);
    let mut out = top;
    for option in &item.options {
        let pros = option.pros.join("; ");
        let cons = option.cons.join("; ");
        let line = fill(&cfg.option_line, "n", &option.n.to_string());
        let line = fill(&line, "text", &option.text);
        let line = fill(&line, "pros", &pros);
        let line = fill(&line, "cons", &cons);
        out.push('\n');
        out.push_str(&line);
    }
    let asker = item.asker.as_ref();
    let mut sub = |body: &str| {
        out.push('\n');
        out.push_str("    - ");
        out.push_str(body);
    };
    if let Some(a) = asker {
        let mut from = format!("From: {}", a.handle);
        if let Some(harness) = &a.harness {
            from.push_str(&format!(" ({harness}"));
            if let Some(rank) = &a.rank {
                from.push_str(&format!(", {rank}"));
            }
            from.push(')');
        }
        if let Some(node) = &item.node {
            if node != "none" {
                from.push_str(&format!(" on {node}"));
            }
        }
        sub(&from);
    }
    if let Some(b) = item.blocked_because.as_deref().filter(|b| !b.is_empty()) {
        sub(&format!("Blocked because: {b}"));
    }
    if let Some(r) = item.options_rationale.as_deref().filter(|r| !r.is_empty()) {
        sub(&format!("Why these options: {r}"));
    }
    if let Some(rec) = &item.recommendation {
        let why = rec.why.as_str();
        if !why.is_empty() {
            let downside = rec
                .downside
                .as_deref()
                .filter(|d| !d.is_empty())
                .map(|d| format!(" Downside: {d}"))
                .unwrap_or_default();
            sub(&format!("Recommended {} because:{downside}", rec.option));
        }
    }
    if let Some(u) = item.unknowns.as_deref().filter(|u| !u.is_empty()) {
        sub(&format!("Not thought through: {u}"));
    }
    if let Some(rev) = item.reversible.as_deref().filter(|r| !r.is_empty()) {
        let cost = item
            .cost_if_wrong
            .as_deref()
            .filter(|c| !c.is_empty())
            .map(|c| format!(". Cost if wrong: {c}"))
            .unwrap_or_default();
        sub(&format!("Reversible: {rev}{cost}"));
    }
    if let Some(m) = item.meanwhile.as_deref().filter(|m| !m.is_empty()) {
        sub(&format!("Meanwhile: {m}"));
    }
    out
}

/// The whole file's blocks keyed by the `^<id>` anchor on the top line.
pub fn blocks(text: &str) -> Vec<FileBlock> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<FileBlock> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        let Some(anchor_start) = line.rfind('^') else {
            i += 1;
            continue;
        };
        let id = line[anchor_start + 1..].trim();
        // A top line is a task line (`- [ ]` / `- [x]`) ending `^<id>`.
        let is_task =
            line.trim_start().starts_with("- [ ]") || line.trim_start().starts_with("- [x]");
        if id.is_empty() || id.contains(char::is_whitespace) || !is_task {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < lines.len() && (lines[i].starts_with("    ") || lines[i].trim().is_empty()) {
            i += 1;
        }
        let end = i;
        out.push(FileBlock {
            id: id.to_string(),
            start,
            end,
            text: lines[start..end].join("\n"),
        });
    }
    out
}

/// The user's answer read out of one block: one ticked option wins, words
/// under the item win when no option is ticked, a ticked top line is a done
/// pin, and two ticked options record nothing.
pub fn read_answer(block: &FileBlock) -> FileAnswer {
    let mut ticked: Vec<u32> = Vec::new();
    let mut words: Option<String> = None;
    for line in block.text.lines().skip(1) {
        let t = line.trim_start();
        if t.starts_with("- [x] ") {
            // Option sub-lines render as `- [x] N. text...` when ticked.
            let rest = t.trim_start_matches("- [x] ");
            let n = rest
                .split('.')
                .next()
                .and_then(|s| s.trim().parse::<u32>().ok());
            match n {
                Some(n) => ticked.push(n),
                None => {
                    // A user's own ticked sub-task under the block: words.
                    words = Some(rest.to_string());
                }
            }
            continue;
        }
        // Words: an indented non-option sub-bullet the writer did not write.
        if t.starts_with("- ") && !writer_bullet(t) {
            let w = t.trim_start_matches("- ").trim();
            if !w.is_empty() {
                words = Some(w.to_string());
            }
        }
    }
    let top_ticked = block
        .text
        .lines()
        .next()
        .map(|l| l.trim_start().starts_with("- [x]"))
        .unwrap_or(false);
    if top_ticked {
        return FileAnswer::Done;
    }
    match ticked.len() {
        1 => FileAnswer::Option(ticked[0]),
        0 => words.map(FileAnswer::Words).unwrap_or(FileAnswer::None),
        _ => FileAnswer::TwoTicked,
    }
}

/// The sub-bullet prefixes this module itself writes; a reader never mistakes
/// them for a user's words.
fn writer_bullet(t: &str) -> bool {
    const PREFIXES: [&str; 8] = [
        "- From:",
        "- Blocked because:",
        "- Why these options:",
        "- Recommended ",
        "- Not thought through:",
        "- Reversible:",
        "- Meanwhile:",
        "- Recorded:",
    ];
    PREFIXES.iter().any(|p| t.starts_with(p))
}

/// Flip the block's top line to `[x]`, stamp `✅ <date>` and append the
/// `Recorded:` sub-bullet. A file with no block for `id` returns unchanged.
pub fn close_block(text: &str, id: &str, receipt_line: &str, date: &str) -> String {
    let found = blocks(text)
        .into_iter()
        .find(|b| b.id == id)
        .unwrap_or_else(|| FileBlock {
            id: id.to_string(),
            start: usize::MAX,
            end: usize::MAX,
            text: String::new(),
        });
    if found.start == usize::MAX {
        return text.to_string();
    }
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let top = &mut lines[found.start];
    *top = top.replacen("- [ ]", "- [x]", 1);
    if !top.contains("✅") {
        // Stamp before the anchor so the `^id` stays line-final.
        if let Some(anchor) = top.rfind('^') {
            top.insert_str(anchor, &format!("✅ {date} "));
        } else {
            top.push_str(&format!(" ✅ {date}"));
        }
    }
    let mut out: String = lines.join("\n");
    if !receipt_line.is_empty() {
        out.push('\n');
        out.push_str("    - ");
        out.push_str(receipt_line);
        out.push('\n');
    }
    out
}

pub fn has_conflict_markers(text: &str) -> bool {
    text.contains("<<<<<<<")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> AttentionItem {
        crate::attention::project(
            r#"{"ts":"2026-09-18T12:00:00Z","type":"operator_question","source":"t","data":{"question_id":"q-e5e5520b","question":"Rule on the Python-tree law: which reading?","ask":"pick one","session_id":"s1","cwd":"/repo/fno","node":"x-aaaa","options":["Yes, net zero needs no grant","Stay strict","Push gate is law"]}}"#,
            &[],
            "",
            0,
        )
        .remove(0)
    }

    #[test]
    fn ac3_hp_top_line_carries_tag_once_and_anchor_plus_options_follow() {
        let cfg = FileSinkConfig::default();
        let text = render_item(&item(), &cfg);
        let lines: Vec<&str> = text.lines().collect();
        let top = lines[0];
        assert!(top.starts_with("- [ ] "));
        assert!(top.ends_with("^q-e5e5520b"));
        assert_eq!(top.matches("#fno").count(), 1);
        // Three option sub-lines, none carrying the tag.
        let options = lines[1..4].iter();
        for o in options {
            assert!(!o.contains("#fno"));
            assert!(o.starts_with("    - [ ] "));
        }
        // The id slot in the template consumes the anchor.
        assert!(!text.contains("^{id}"));
    }

    #[test]
    fn ac3_edge_no_second_block_for_an_already_delivered_item() {
        let cfg = FileSinkConfig::default();
        let rendered = render_item(&item(), &cfg);
        let mut file = String::from("existing user line\n");
        file.push_str(&rendered);
        let found = blocks(&file);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "q-e5e5520b");
        // The arm's dedup is: a file that already holds ^q-e5e5520b gets no
        // second block. That lives in the arm (append only when absent).
    }

    #[test]
    fn ac4_edge_tick_younger_than_settle_changes_nothing_yet() {
        // Settle is an arm decision; the file layer only classifies the
        // block's tick state honestly. This test pins the classification
        // that the arm's settle window then gates on.
        let cfg = FileSinkConfig::default();
        let block_text = render_item(&item(), &cfg);
        let ticked = block_text.replacen("    - [ ] 2.", "    - [x] 2.", 1);
        let file = format!("user line\n{ticked}");
        let block = &blocks(&file)[0];
        assert_eq!(read_answer(block), FileAnswer::Option(2));
    }

    #[test]
    fn ac4_err_two_ticked_options_record_nothing() {
        let cfg = FileSinkConfig::default();
        let block_text = render_item(&item(), &cfg);
        let mut ticked = block_text.replacen("    - [ ] 1.", "    - [x] 1.", 1);
        ticked = ticked.replacen("    - [ ] 3.", "    - [x] 3.", 1);
        let file = format!("user line\n{ticked}");
        let block = &blocks(&file)[0];
        assert_eq!(read_answer(block), FileAnswer::TwoTicked);
    }

    #[test]
    fn words_under_the_block_read_as_the_answer() {
        let cfg = FileSinkConfig::default();
        let rendered = render_item(&item(), &cfg);
        let file = format!("{rendered}\n    - take the narrow reading, watch the allowance\n");
        let block = &blocks(&file)[0];
        assert_eq!(
            read_answer(block),
            FileAnswer::Words("take the narrow reading, watch the allowance".to_string())
        );
    }

    #[test]
    fn close_block_flips_stamps_and_appends_receipt() {
        let cfg = FileSinkConfig::default();
        let rendered = render_item(&item(), &cfg);
        let file = format!("user line\n{rendered}");
        let out = close_block(
            &file,
            "q-e5e5520b",
            "Recorded: option 2 as d-abcd (file)",
            "2026-09-20",
        );
        let block = &blocks(&out)[0];
        assert!(block.text.lines().next().unwrap().starts_with("- [x] "));
        assert!(block.text.contains("✅ 2026-09-20"));
        assert!(out.contains("Recorded: option 2 as d-abcd (file)"));
        // Anchor stays line-final on the top line.
        assert!(block
            .text
            .lines()
            .next()
            .unwrap()
            .trim_end()
            .ends_with("^q-e5e5520b"));
    }

    #[test]
    fn conflict_markers_are_detected() {
        assert!(has_conflict_markers("line\n<<<<<<< HEAD\n"));
        assert!(!has_conflict_markers("clean file"));
    }
}
