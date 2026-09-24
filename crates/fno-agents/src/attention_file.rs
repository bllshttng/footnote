//! The question page format: one page per question in the vault's questions
//! folder, an index of wikilinks, and an Obsidian Base over the folder. The
//! renderer escapes `<`/`>` in every text field it writes, because Obsidian
//! reads an unescaped `<stage>` as an open HTML tag and every checkbox after
//! it stops working. The answer reader unescapes them. Every function here is
//! pure; the arm owns the disk.

use crate::attention::AttentionItem;
use crate::attention_route::Routing;

/// Angle brackets never reach the page raw: they become HTML entities the
/// answer reader unescapes, never tags Obsidian can eat.
pub fn escape_text(text: &str) -> String {
    text.replace('<', "&lt;").replace('>', "&gt;")
}

pub fn unescape_text(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">")
}

/// The page's title: the question's first line, cut to 120 characters, with
/// the characters that break wikilinks or headings removed.
pub fn page_title(question: &str) -> String {
    let first_line = question.lines().next().unwrap_or("").trim();
    let cleaned: String = first_line
        .chars()
        .filter(|c| !matches!(c, '|' | '[' | ']' | '\n' | '\r'))
        .collect();
    cleaned.chars().take(120).collect()
}

/// The file-name slug for a page: a short kebab of the question, for the
/// `<ask date>-<id>-<slug>-<node>` page names.
pub fn page_slug(question: &str) -> String {
    let title = page_title(question);
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 24 {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

/// Frontmatter keys for one page. Unknown keys a vault plugin stamps (an
/// `updated:` line) survive a close through the flattened map. `answer`,
/// `answered_at` and `recorded_by` are absent until close.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PageFront {
    pub question_id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub title: String,
    /// The question as a Base column (the ask cell), the user's wording.
    #[serde(default)]
    pub ask: String,
    /// The recommended option as a letter (a, b, c), a Base column.
    #[serde(default)]
    pub recommend: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub asked_at: String,
    /// Base columns beside the routing facts; both carry the ask time at
    /// render, and a vault plugin may stamp `updated` later.
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub project: String,
    #[serde(default, rename = "harness_session_id")]
    pub harness_session_id: String,
    #[serde(default)]
    pub session_name: String,
    #[serde(default)]
    pub harness: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub blocks: Vec<String>,
    #[serde(default)]
    pub epic: String,
    #[serde(default)]
    pub crown: String,
    #[serde(default)]
    pub king: String,
    /// The user's typed answer cell: a letter (a, b, c), a number, or words.
    /// An empty value is present from render so the Base shows the cell, and
    /// is never an answer (rule 8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_by: Option<String>,
    #[serde(flatten)]
    pub extra: serde_yaml_ng::Mapping,
}

/// The Base column letter for option `n` (1 -> a, 2 -> b, ...).
pub fn option_letter(n: u32) -> String {
    let idx = n.wrapping_sub(1);
    if idx < 26 {
        ((b'a' + idx as u8) as char).to_string()
    } else {
        n.to_string()
    }
}

/// The user's answer read out of one page.
#[derive(Debug, Clone, PartialEq)]
pub enum FileAnswer {
    None,
    /// Exactly one option ticked.
    Option(u32),
    /// Words the user wrote under `## Answer`.
    Words(String),
    /// The pin's `- [x] Done` ticked.
    Done,
    /// Two or more options ticked; nothing records, one notice.
    TwoTicked,
}

fn fill_unknown(value: &str) -> String {
    if value.trim().is_empty() {
        "unknown".to_string()
    } else {
        value.to_string()
    }
}

fn fill_none(value: Option<&String>) -> String {
    match value {
        Some(v) if !v.trim().is_empty() => v.clone(),
        _ => "none".to_string(),
    }
}

/// Render one item to its page. Only what the asker gave renders: no filler
/// lines for an absent field. Routing facts freeze into the frontmatter at
/// write time.
pub fn render_page(item: &AttentionItem, routing: &Routing) -> String {
    let title = page_title(&item.title);
    let asker = item.asker.as_ref();
    let harness_session_id = asker.and_then(|a| a.session_id.clone()).unwrap_or_default();
    let session_name = routing
        .session_name
        .clone()
        .or_else(|| asker.map(|a| a.handle.clone()))
        .unwrap_or_default();
    let harness = routing
        .harness
        .clone()
        .or_else(|| asker.and_then(|a| a.harness.clone()))
        .unwrap_or_default();
    let node = item
        .node
        .as_deref()
        .filter(|n| !n.is_empty() && *n != "none")
        .map(str::to_string)
        .unwrap_or_default();

    let front = PageFront {
        question_id: item.id.clone(),
        kind: item.kind.clone(),
        status: "open".to_string(),
        title: escape_text(&title),
        ask: escape_text(&title),
        recommend: item
            .recommendation
            .as_ref()
            .map(|r| option_letter(r.option))
            .unwrap_or_default(),
        aliases: vec![escape_text(&title)],
        asked_at: item.created_at.clone(),
        created: item.created_at.clone(),
        updated: item.created_at.clone(),
        project: item.project.clone(),
        harness_session_id: fill_unknown(&harness_session_id),
        session_name: fill_unknown(&session_name),
        harness: fill_unknown(&harness),
        model: fill_unknown(&routing.model.as_deref().unwrap_or("")),
        node: fill_none(if node.is_empty() { None } else { Some(&node) }),
        blocks: item.blocks.clone(),
        epic: fill_none(routing.epic.as_ref()),
        crown: fill_none(routing.crown.as_ref()),
        king: fill_none(routing.king.as_ref()),
        answer: Some(String::new()),
        answered_at: None,
        recorded_by: None,
        extra: serde_yaml_ng::Mapping::new(),
    };
    // The Base's answer cell reads one column per option: `a`, `b`, `c`, ...
    let mut front = front;
    for option in &item.options {
        front.extra.insert(
            serde_yaml_ng::Value::String(option_letter(option.n)),
            serde_yaml_ng::Value::String(escape_text(&option.text)),
        );
    }
    let front_yaml = serde_yaml_ng::to_string(&front).unwrap_or_default();

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(front_yaml.trim_end());
    out.push_str("\n---\n\n");
    out.push_str("# ");
    out.push_str(&escape_text(&title));
    out.push('\n');

    if item.kind == "pin" {
        let ask = item
            .body
            .as_deref()
            .filter(|b| !b.is_empty())
            .unwrap_or(&item.title);
        out.push_str("\n## Action\n\n");
        out.push_str(&escape_text(ask));
        out.push_str(
            "\n\n## Answer\n\n- [ ] Done\n<!-- Tick Done, or write your answer below. -->\n",
        );
        return out;
    }

    if let Some(body) = item.body.as_deref().filter(|b| !b.is_empty()) {
        out.push('\n');
        out.push_str(&escape_text(body));
        out.push('\n');
    }

    if !item.options.is_empty() {
        out.push_str("\n## Options\n\n");
        for option in &item.options {
            out.push_str(&format!(
                "- [ ] {}. {}",
                option.n,
                escape_text(&option.text)
            ));
            if let Some(next) = option.next.as_deref().filter(|n| !n.is_empty()) {
                out.push_str(&format!(". Next: {}", escape_text(next)));
            }
            if !option.pros.is_empty() {
                out.push_str(&format!(". Pro: {}", escape_text(&option.pros.join("; "))));
            }
            if !option.cons.is_empty() {
                out.push_str(&format!(". Con: {}", escape_text(&option.cons.join("; "))));
            }
            out.push('\n');
        }
    }

    let mut context: Vec<String> = Vec::new();
    if let Some(b) = item.blocked_because.as_deref().filter(|b| !b.is_empty()) {
        context.push(format!("Blocked because: {}", escape_text(b)));
    }
    if let Some(r) = item.options_rationale.as_deref().filter(|r| !r.is_empty()) {
        context.push(format!("Why these options: {}", escape_text(r)));
    }
    if let Some(rec) = &item.recommendation {
        if !rec.why.is_empty() {
            let mut line = format!(
                "Recommended: {}, because {}",
                rec.option,
                escape_text(&rec.why)
            );
            if let Some(d) = rec.downside.as_deref().filter(|d| !d.is_empty()) {
                line.push_str(&format!(". Downside: {}", escape_text(d)));
            }
            line.push('.');
            context.push(line);
        }
    }
    if let Some(u) = item.unknowns.as_deref().filter(|u| !u.is_empty()) {
        context.push(format!("Not thought through: {}", escape_text(u)));
    }
    if let Some(rev) = item.reversible.as_deref().filter(|r| !r.is_empty()) {
        let mut line = format!("Reversible: {}", escape_text(rev));
        if let Some(c) = item.cost_if_wrong.as_deref().filter(|c| !c.is_empty()) {
            line.push_str(&format!(". Cost if wrong: {}", escape_text(c)));
        }
        context.push(line);
    }
    if let Some(m) = item.meanwhile.as_deref().filter(|m| !m.is_empty()) {
        context.push(format!("Meanwhile: {}", escape_text(m)));
    }
    if !context.is_empty() {
        out.push_str("\n## Context\n\n");
        for line in &context {
            out.push_str(line);
            out.push('\n');
        }
    }

    out.push_str("\n## Answer\n\n");
    out.push_str("<!-- Tick one option above, or write your answer below. -->\n");
    out
}

/// Split a page into its frontmatter and body. `None` when there is no
/// frontmatter or no `question_id`, so non-page files never parse as pages.
pub fn parse_page(text: &str) -> Option<(PageFront, String)> {
    let text = text.strip_prefix("\u{feff}").unwrap_or(text);
    let mut lines = text.lines();
    if !lines.next()?.trim().ends_with("---") {
        return None;
    }
    let mut front = String::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        front.push_str(line);
        front.push('\n');
    }
    if !closed {
        return None;
    }
    let rest: String = lines.collect::<Vec<_>>().join("\n");
    let front: PageFront = serde_yaml_ng::from_str(&front).ok()?;
    if front.question_id.trim().is_empty() {
        return None;
    }
    Some((front, rest))
}

/// Hash of the body only, so a frontmatter stamp never restarts a settle
/// window.
pub fn body_hash(text: &str) -> u64 {
    let body = match parse_page(text) {
        Some((_, body)) => body,
        None => text.to_string(),
    };
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in body.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// The settle key: the body hash mixed with the typed `answer` cell, so a
/// Base edit restarts the settle window but a vault plugin's frontmatter
/// stamp does not.
pub fn settle_key(text: &str) -> u64 {
    let cell = parse_page(text)
        .and_then(|(front, _)| front.answer)
        .unwrap_or_default();
    let mut key = body_hash(text);
    for byte in cell.as_bytes() {
        key ^= u64::from(*byte).wrapping_mul(0x9e3779b97f4a7c15);
        key = key.rotate_left(7);
    }
    key
}

/// One section of the body, as its trimmed lines after the `## <name>`
/// heading and before the next heading.
fn section<'a>(body: &'a str, name: &str) -> Option<Vec<&'a str>> {
    let mut lines = body.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim() == format!("## {name}") {
            let mut out: Vec<&str> = Vec::new();
            for line in lines.by_ref() {
                if line.trim_start().starts_with("## ") {
                    break;
                }
                out.push(line);
            }
            return Some(out);
        }
    }
    None
}

/// The user's answer read out of one page. The typed `answer` cell in the
/// frontmatter reads first: a letter (a, b, c) or a number maps to its
/// option, any other words read as the answer, and an empty cell is never an
/// answer (rule 8). Then the body: an unticked `- [ ]` line, option or not,
/// is never an answer. Precedence: cell, then `Done`, then one tick, then
/// body words; two or more ticks record nothing.
pub fn read_page_answer(text: &str) -> FileAnswer {
    let Some((front, body)) = parse_page(text) else {
        return FileAnswer::None;
    };
    if let Some(cell) = front
        .answer
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        let lowered = cell.to_ascii_lowercase();
        if lowered.len() == 1 {
            let c = lowered.as_bytes()[0];
            if c.is_ascii_lowercase() {
                return FileAnswer::Option(u32::from(c - b'a' + 1));
            }
            if c.is_ascii_digit() {
                return cell
                    .parse::<u32>()
                    .map(FileAnswer::Option)
                    .unwrap_or(FileAnswer::Words(unescape_text(cell)));
            }
        }
        return FileAnswer::Words(unescape_text(cell));
    }
    let mut ticked: Vec<u32> = Vec::new();
    if let Some(lines) = section(&body, "Options") {
        for line in lines {
            let t = line.trim_start();
            if let Some(rest) = t
                .strip_prefix("- [x] ")
                .or_else(|| t.strip_prefix("- [X] "))
            {
                if let Some(n) = rest
                    .split('.')
                    .next()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                {
                    ticked.push(n);
                }
            }
            // An unticked `- [ ]` line is never an answer.
        }
    }
    if ticked.len() >= 2 {
        return FileAnswer::TwoTicked;
    }
    let mut done = false;
    let mut words: Vec<String> = Vec::new();
    if let Some(lines) = section(&body, "Answer") {
        for line in lines {
            let t = line.trim();
            if t.is_empty() || t.starts_with("<!--") || t.starts_with("Recorded:") {
                continue;
            }
            if t.starts_with("- [x] Done") || t.starts_with("- [X] Done") {
                done = true;
                continue;
            }
            if t.starts_with("- [ ] Done") {
                continue;
            }
            if let Some(rest) = t.strip_prefix("- ") {
                // A ticked sub-task the user added: words.
                let w = unescape_text(rest.trim());
                if !w.is_empty() {
                    words.push(w);
                }
                continue;
            }
            words.push(unescape_text(t));
        }
    }
    if done {
        return FileAnswer::Done;
    }
    if ticked.len() == 1 {
        return FileAnswer::Option(ticked[0]);
    }
    if words.is_empty() {
        FileAnswer::None
    } else {
        FileAnswer::Words(words.join(" "))
    }
}

/// Set the four close keys in the frontmatter, keep every other key, and
/// append `Recorded: <receipt>` as the last line under `## Answer`.
pub fn close_page(
    text: &str,
    status: &str,
    answer: &str,
    answered_at: &str,
    recorded_by: &str,
    receipt: &str,
) -> String {
    let Some((mut front, body)) = parse_page(text) else {
        return text.to_string();
    };
    front.status = status.to_string();
    front.answer = (!answer.is_empty()).then(|| answer.to_string());
    front.answered_at = (!answered_at.is_empty()).then(|| answered_at.to_string());
    front.recorded_by = (!recorded_by.is_empty()).then(|| recorded_by.to_string());
    if let Ok(yaml) = serde_yaml_ng::to_string(&front) {
        let mut out = String::from("---\n");
        out.push_str(yaml.trim_end());
        out.push_str("\n---\n");
        out.push_str(&body);
        let out = out.trim_end().to_string();
        let mut out = out;
        out.push_str("\n\nRecorded: ");
        out.push_str(receipt);
        out.push('\n');
        return out;
    }
    text.to_string()
}

/// One open row for the index.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexEntry {
    /// The file stem, so the wikilink resolves in Obsidian.
    pub stem: String,
    pub id: String,
    pub title: String,
    pub kind: String,
    pub blocks: Vec<String>,
    pub king: String,
    /// The page's ask time; the index sorts newest first.
    pub created: String,
}

/// One closed row for the index.
#[derive(Debug, Clone, PartialEq)]
pub struct DoneEntry {
    pub stem: String,
    pub id: String,
    pub title: String,
    pub status: String,
    pub answered_at: String,
    pub answer: String,
}

fn index_line(entry: &IndexEntry) -> String {
    let mut line = format!("- [[{}|{}]] · {}", entry.stem, entry.title, entry.kind);
    if !entry.blocks.is_empty() {
        line.push_str(&format!(" · blocks {}", entry.blocks.join(", ")));
    }
    line.push_str(&format!(" · {}", entry.king));
    line
}

/// The generated index page: one wikilink per open page, then the 20 most
/// recent closed pages.
pub fn render_index(open: &[IndexEntry], done: &[DoneEntry]) -> String {
    let mut open: Vec<&IndexEntry> = open.iter().collect();
    open.sort_by(|a, b| b.created.cmp(&a.created));
    let mut out = String::new();
    out.push_str("---\nfno_generated: questions-index\n---\n");
    out.push_str("<!-- GENERATED by the fno attention arm - edits are overwritten. -->\n\n");
    out.push_str(&format!("## Open ({})\n\n", open.len()));
    for entry in open {
        out.push_str(&index_line(entry));
        out.push('\n');
    }
    out.push_str("\n## Done\n\n");
    for entry in done.iter().take(20) {
        let date = entry.answered_at.chars().take(10).collect::<String>();
        let answer: String = entry.answer.chars().take(60).collect();
        out.push_str(&format!(
            "- [[{}|{}]] · {} {} · {}\n",
            entry.stem, entry.title, entry.status, date, answer
        ));
    }
    out
}

/// The Obsidian Base over the questions folder.
pub const BASE: &str = r#"# GENERATED by the fno attention arm - edits are overwritten.
filters:
  and:
    - 'file.hasProperty("question_id")'
    - 'file.inFolder(this.file.folder)'
formulas:
  question: 'file.asLink(if(ask, ask, title))'
  age_days: 'if(asked_at, (now() - date(asked_at)).days.round(0), "")'
properties:
  formula.question:
    displayName: Question
  formula.age_days:
    displayName: "Age (d)"
  recommend:
    displayName: Rec
  a:
    displayName: "Option a"
  b:
    displayName: "Option b"
  c:
    displayName: "Option c"
views:
  - type: table
    name: "Needs you"
    filters:
      and:
        - 'status == "open"'
    order:
      - formula.question
      - recommend
      - a
      - b
      - c
      - node
      - king
      - formula.age_days
    sort:
      - property: asked_at
        direction: DESC
  - type: table
    name: "Open by king"
    filters:
      and:
        - 'status == "open"'
    groupBy:
      property: king
      direction: ASC
    order:
      - formula.question
      - recommend
      - node
      - formula.age_days
    sort:
      - property: asked_at
        direction: DESC
  - type: table
    name: "Open by node"
    filters:
      and:
        - 'status == "open"'
    groupBy:
      property: node
      direction: ASC
    order:
      - formula.question
      - recommend
      - king
      - formula.age_days
    sort:
      - property: asked_at
        direction: DESC
  - type: table
    name: Answered
    filters:
      and:
        - 'status != "open"'
    limit: 50
    order:
      - formula.question
      - answer
      - answered_at
      - node
    sort:
      - property: answered_at
        direction: DESC
  - type: cards
    name: Board
    groupBy:
      property: status
      direction: ASC
    order:
      - formula.question
      - recommend
      - node
      - formula.age_days
    sort:
      - property: asked_at
        direction: DESC
"#;

pub fn has_conflict_markers(text: &str) -> bool {
    text.contains("<<<<<<<")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::{Asker, ItemOption, Recommendation};

    fn item() -> AttentionItem {
        AttentionItem {
            id: "q-e5e5520b".into(),
            kind: "question".into(),
            title: "Rule on the Python-tree law: which reading?".into(),
            body: Some("The law blocks three plans while it waits.".into()),
            project: "fno".into(),
            priority: "normal".into(),
            created_at: "2026-09-22T12:00:00Z".into(),
            deadline: None,
            on_silence: None,
            class: None,
            blocks: vec!["x-aaaa".into()],
            subject: None,
            asker: Some(Asker {
                handle: "s1".into(),
                session_id: Some("s1-full-uuid".into()),
                harness: Some("claude".into()),
                rank: None,
                live: None,
                reach: None,
            }),
            node: Some("x-bbbb".into()),
            blocked_because: Some("the ruling decides which gate ships".into()),
            options_rationale: Some("both readings ship the same tests".into()),
            recommendation: Some(Recommendation {
                option: 2,
                why: "the strict reading blocks the least".into(),
                downside: Some("one release carries the shim".into()),
            }),
            options: vec![
                ItemOption {
                    n: 1,
                    text: "Yes, net zero needs no grant".into(),
                    next: Some("open the gate".into()),
                    pros: vec!["fast".into()],
                    cons: vec![],
                },
                ItemOption {
                    n: 2,
                    text: "Stay strict".into(),
                    next: None,
                    pros: vec![],
                    cons: vec![],
                },
                ItemOption {
                    n: 3,
                    text: "Push gate is law".into(),
                    next: None,
                    pros: vec![],
                    cons: vec![],
                },
            ],
            unknowns: Some("whether the gate reads the store".into()),
            reversible: Some("yes".into()),
            cost_if_wrong: Some("one wasted release".into()),
            meanwhile: Some("proceeds on the strict reading".into()),
            ready: true,
            missing: vec![],
            state: "open".into(),
        }
    }

    fn routing() -> Routing {
        Routing {
            session_name: Some("worker-1".into()),
            harness: Some("claude".into()),
            model: None,
            epic: Some("x-aaaa".into()),
            crown: None,
            king: Some("king-fno".into()),
        }
    }

    #[test]
    fn ac3_hp_full_question_renders_frontmatter_and_body() {
        let page = render_page(&item(), &routing());
        let (front, body) = parse_page(&page).unwrap();
        assert_eq!(front.question_id, "q-e5e5520b");
        assert_eq!(front.kind, "question");
        assert_eq!(front.status, "open");
        assert_eq!(front.title, "Rule on the Python-tree law: which reading?");
        assert_eq!(
            front.aliases,
            vec!["Rule on the Python-tree law: which reading?"]
        );
        assert_eq!(front.asked_at, "2026-09-22T12:00:00Z");
        assert_eq!(front.harness_session_id, "s1-full-uuid");
        assert_eq!(front.session_name, "worker-1");
        assert_eq!(front.harness, "claude");
        assert_eq!(
            front.model, "unknown",
            "unmeasured asker fact reads unknown"
        );
        assert_eq!(front.node, "x-bbbb");
        assert_eq!(front.blocks, vec!["x-aaaa"]);
        assert_eq!(front.epic, "x-aaaa");
        assert_eq!(front.crown, "none", "absent routing fact reads none");
        assert_eq!(front.king, "king-fno");
        assert_eq!(front.answer.as_deref(), Some(""));
        assert_eq!(front.ask, front.title);
        assert_eq!(
            front.recommend, "b",
            "recommendation 2 renders as its letter"
        );
        for column in ["a", "b", "c"] {
            assert!(
                front.extra.contains_key(column),
                "option column {column} present: {:?}",
                front.extra
            );
        }
        assert!(body.contains("# Rule on the Python-tree law: which reading?"));
        assert!(
            body.contains("- [ ] 1. Yes, net zero needs no grant. Next: open the gate. Pro: fast")
        );
        assert!(body.contains("- [ ] 2. Stay strict"));
        assert!(body.contains("Blocked because: the ruling decides which gate ships"));
        assert!(body.contains("Recommended: 2, because the strict reading blocks the least. Downside: one release carries the shim."));
        assert!(body.contains("Reversible: yes. Cost if wrong: one wasted release"));
        assert!(body.contains("Meanwhile: proceeds on the strict reading"));
        assert!(body.contains("<!-- Tick one option above"));
    }

    #[test]
    fn rule8b_empty_pros_and_cons_omit_the_part() {
        let it = item();
        let page = render_page(&it, &routing());
        assert!(
            page.contains("- [ ] 2. Stay strict\n"),
            "no pros/cons renders bare: {page}"
        );
        assert!(page
            .contains("- [ ] 1. Yes, net zero needs no grant. Next: open the gate. Pro: fast\n"));
        assert!(!page.contains("Pro: fast. Con:"));
        assert!(!page.contains("Stay strict. Next:"));
    }

    #[test]
    fn rule9_angle_brackets_escape_and_the_next_checkbox_survives() {
        let mut it = item();
        it.title = "Route <stage> by <handle>".into();
        it.options[0].text = "run <stage> first".into();
        it.recommendation.as_mut().unwrap().why = "matches <id>".into();
        let page = render_page(&it, &routing());
        assert!(!page.contains("<stage>"), "raw bracket leaked: {page}");
        assert!(page.contains("&lt;stage&gt;"));
        assert!(page.contains("&lt;id&gt;"));
        // The option line after the escaped text still parses as an unticked
        // checkbox.
        let (front, body) = parse_page(&page).unwrap();
        assert_eq!(front.title, "Route &lt;stage&gt; by &lt;handle&gt;");
        assert!(body.contains("- [ ] 2. Stay strict"));
        assert_eq!(read_page_answer(&page), FileAnswer::None);
    }

    #[test]
    fn ac3_edge_answer_precedence_done_tick_words() {
        let page = render_page(&item(), &routing());
        // One ticked option.
        let ticked = page.replacen("- [ ] 2.", "- [x] 2.", 1);
        assert_eq!(read_page_answer(&ticked), FileAnswer::Option(2));
        // Two ticked options.
        let two = ticked.replacen("- [ ] 1.", "- [x] 1.", 1);
        assert_eq!(read_page_answer(&two), FileAnswer::TwoTicked);
        // An unticked option is never an answer.
        assert_eq!(read_page_answer(&page), FileAnswer::None);
        // Words under ## Answer.
        let words = page.replace(
            "<!-- Tick one option above, or write your answer below. -->",
            "take the narrow reading",
        );
        assert_eq!(
            read_page_answer(&words),
            FileAnswer::Words("take the narrow reading".into())
        );
        // The writer's comment never reads as words; multi-line words join.
        let commented = words.replace(
            "take the narrow reading",
            "<!-- Tick one option above -->\nfirst line\nsecond line",
        );
        assert_eq!(
            read_page_answer(&commented),
            FileAnswer::Words("first line second line".into())
        );
    }

    #[test]
    fn pin_page_renders_action_and_done() {
        let mut it = item();
        it.kind = "pin".into();
        it.options.clear();
        it.body = Some("publish the crate".into());
        let page = render_page(&it, &routing());
        assert!(page.contains("## Action"));
        assert!(page.contains("publish the crate"));
        assert!(page.contains("- [ ] Done"));
        assert_eq!(read_page_answer(&page), FileAnswer::None);
        let done = page.replace("- [ ] Done", "- [x] Done");
        assert_eq!(read_page_answer(&done), FileAnswer::Done);
    }

    #[test]
    fn ac3_err_plugin_stamp_survives_close() {
        let page = render_page(&item(), &routing());
        // A vault plugin's stamp rewrites the `updated` property in place;
        // the close must keep it.
        let stamped = page.replacen(
            "updated: 2026-09-22T12:00:00Z",
            "updated: 2026-09-22T09:05",
            1,
        );
        let closed = close_page(
            &stamped,
            "answered",
            "Stay strict",
            "2026-09-23T00:00:00Z",
            "file_edit",
            "option 2 (file)",
        );
        let (front, _) = parse_page(&closed).unwrap();
        assert_eq!(front.status, "answered");
        assert_eq!(front.answer.as_deref(), Some("Stay strict"));
        assert_eq!(front.answered_at.as_deref(), Some("2026-09-23T00:00:00Z"));
        assert_eq!(front.recorded_by.as_deref(), Some("file_edit"));
        assert!(
            closed.contains("updated: 2026-09-22T09:05"),
            "the plugin stamp survives: {closed}"
        );
        assert!(closed.trim_end().ends_with("Recorded: option 2 (file)"));
    }

    #[test]
    fn body_hash_ignores_a_frontmatter_stamp() {
        let page = render_page(&item(), &routing());
        // A vault plugin's stamp adds an unknown key; the body is untouched.
        let stamped = page.replacen(
            "king: king-fno",
            "king: king-fno\nplugin_stamp: 2026-09-22T09:05",
            1,
        );
        assert_eq!(body_hash(&page), body_hash(&stamped));
    }

    #[test]
    fn index_and_base_render() {
        let open = vec![
            IndexEntry {
                stem: "20260922-q-e5e5520b-rule-on-the-python-tree-x-bbbb".into(),
                id: "q-e5e5520b".into(),
                title: "Rule on the Python-tree law: which reading?".into(),
                kind: "question".into(),
                blocks: vec!["x-aaaa".into()],
                king: "king-fno".into(),
                created: "2026-09-22T12:00:00Z".into(),
            },
            IndexEntry {
                stem: "20260923-q-99999999-newer-page-x-bbbb".into(),
                id: "q-99999999".into(),
                title: "a newer page".into(),
                kind: "question".into(),
                blocks: vec![],
                king: "none".into(),
                created: "2026-09-23T09:00:00Z".into(),
            },
        ];
        let done = vec![DoneEntry {
            stem: "20260921-q-11111111-done-item-x-none".into(),
            id: "q-11111111".into(),
            title: "an older one".into(),
            status: "answered".into(),
            answered_at: "2026-09-21T08:00:00Z".into(),
            answer: "narrow".into(),
        }];
        let index = render_index(&open, &done);
        assert!(index.contains("fno_generated: questions-index"));
        assert!(index.contains("## Open (2)"));
        // Newest ask first.
        let newer = index.find("q-99999999").unwrap();
        let older = index.find("q-e5e5520b").unwrap();
        assert!(newer < older, "the index lists newest first: {index}");
        assert!(index.contains(
            "- [[20260922-q-e5e5520b-rule-on-the-python-tree-x-bbbb|Rule on the Python-tree law: which reading?]] · question · blocks x-aaaa · king-fno"
        ));
        assert!(index.contains("- [[20260921-q-11111111-done-item-x-none|an older one]] · answered 2026-09-21 · narrow"));
        assert!(BASE.starts_with("# GENERATED by the fno attention arm"));
        assert!(BASE.contains("'file.hasProperty(\"question_id\")'"));
        assert!(BASE.contains("'file.inFolder(this.file.folder)'"));
        assert!(BASE.contains("question: 'file.asLink(if(ask, ask, title))'"));
        assert!(
            BASE.contains("age_days: 'if(asked_at, (now() - date(asked_at)).days.round(0), \"\")'")
        );
        assert!(BASE.contains("name: \"Needs you\""));
        assert!(BASE.contains("name: \"Open by king\""));
        assert!(BASE.contains("name: Answered"));
        assert!(BASE.contains("name: \"Board\""));
        assert!(BASE.contains("displayName: Age (d)"));
        for column in [
            "formula.question",
            "recommend",
            "a",
            "b",
            "c",
            "node",
            "king",
            "formula.age_days",
        ] {
            assert!(
                BASE.contains(&format!("- {column}\n")),
                "Needs you order names {column}: {BASE}"
            );
        }
    }

    #[test]
    fn the_typed_answer_cell_records_and_the_empty_cell_does_not() {
        let page = render_page(&item(), &routing());
        // The open page carries an empty answer cell; it is never an answer.
        assert!(
            page.contains("answer: ''"),
            "cell present but empty: {page}"
        );
        assert_eq!(read_page_answer(&page), FileAnswer::None);
        // A typed letter records its option.
        let b = page.replacen("answer: ''", "answer: b", 1);
        assert_eq!(read_page_answer(&b), FileAnswer::Option(2));
        let c = page.replacen("answer: ''", "answer: C", 1);
        assert_eq!(read_page_answer(&c), FileAnswer::Option(3));
        // A typed number too.
        let n = page.replacen("answer: ''", "answer: '1'", 1);
        assert_eq!(read_page_answer(&n), FileAnswer::Option(1));
        // Other words read as the answer.
        let w = page.replacen("answer: ''", "answer: take the narrow reading", 1);
        assert_eq!(
            read_page_answer(&w),
            FileAnswer::Words("take the narrow reading".into())
        );
        // A body tick still answers.
        assert_eq!(
            read_page_answer(&page.replacen("- [ ] 2.", "- [x] 2.", 1)),
            FileAnswer::Option(2)
        );
        // Typing restarts the settle window; a frontmatter stamp does not.
        let key0 = settle_key(&page);
        let typed = page.replacen("answer: ''", "answer: b", 1);
        assert_ne!(key0, settle_key(&typed));
        let stamped = page.replacen(
            "king: king-fno",
            "king: king-fno\nplugin_stamp: 2026-09-23T10:00:00Z",
            1,
        );
        assert_eq!(key0, settle_key(&stamped));
    }

    #[test]
    fn title_and_slug_cleaning() {
        assert_eq!(
            page_title("First | line [x] and more"),
            "First  line x and more"
        );
        assert_eq!(page_title("multi\nline").chars().count(), 5);
        assert!(page_slug("Rule on the Python-tree law!").starts_with("rule-on-the-python-tree"));
        assert!(page_slug("shed").len() <= 24);
        let long = "a".repeat(200);
        assert_eq!(page_title(&long).chars().count(), 120);
    }

    #[test]
    fn conflict_markers_are_detected() {
        assert!(has_conflict_markers("line\n<<<<<<< HEAD\n"));
        assert!(!has_conflict_markers("clean file"));
    }
}
