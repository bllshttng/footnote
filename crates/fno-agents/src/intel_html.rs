//! `fno-agents intel --render <report.md>` - the operator-facing HTML copy.
//!
//! Reads the markdown report and the fold JSON its frontmatter names in
//! `fold:`, scrubs secrets, home paths, quoted blocks and quotes outside
//! Operator corrections, draws the fold's counters as inline SVG, and
//! writes `<report stem>.html` beside the markdown plus `latest.html`,
//! mode 0600. The markdown stays the machine-read source; the HTML is the
//! copy the operator hands to others. Print-to-PDF is the browser's job,
//! so no PDF library rides along.

use crate::claude_ask::html_escape_quote;
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// The fold keys the renderer reads. The names are report-shape.md's
// contract, so a renamed key breaks this one const block and nothing else.
const KEY_DAYS: &str = "days";
const KEY_SCOPE: &str = "scope";
const KEY_HARNESSES: &str = "harnesses";
const KEY_POPULATIONS: &str = "populations";
const KEY_SCANNED: &str = "scanned";
const KEY_JUDGED: &str = "judged";
const KEY_ELIGIBLE: &str = "eligible";
const KEY_SAMPLED: &str = "sampled";
const KEY_CATEGORIES: &str = "categories";
const KEY_ITEMS: &str = "items";
const KEY_NAME: &str = "name";
const KEY_SHARE: &str = "share_pct";
const KEY_DAILY: &str = "daily";
const KEY_DATE: &str = "date";
const KEY_HARNESS: &str = "harness";
const KEY_SESSIONS: &str = "sessions";
const KEY_TURNS: &str = "operator_turns";
const KEY_HOURS: &str = "hours";
const KEY_UTC_OFFSET: &str = "utc_offset";
const KEY_RESPONSE_TIME: &str = "response_time";
const KEY_BUCKETS: &str = "buckets";
const KEY_ACTIVITY: &str = "activity";
const KEY_TOOL_ERRORS: &str = "tool_errors";

/// One rendered report: the stamped path, the stderr lines, and the count
/// of stamped files pruned after `latest.html` landed.
pub(crate) struct RenderOutcome {
    stamped: PathBuf,
    /// Read by the AC12 test; render_report's receipt is the stamped path.
    #[allow(dead_code)]
    latest: PathBuf,
    notes: Vec<String>,
}

/// `fno-agents intel --render <report.md>`: the CLI shell. Refusals exit 2
/// with one stderr line naming the fault; success prints the stamped path
/// and nothing else.
pub fn run_render(args: &[String]) -> i32 {
    let mut report: Option<&String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--render" => match args.get(i + 1) {
                Some(v) => {
                    report = Some(v);
                    i += 1;
                }
                None => {
                    eprintln!("fno-agents intel --render: needs a report path");
                    return 2;
                }
            },
            other => {
                eprintln!("fno-agents intel --render: unknown flag {other}");
                return 2;
            }
        }
        i += 1;
    }
    let Some(report) = report else {
        eprintln!("fno-agents intel --render: needs a report path");
        return 2;
    };
    if !report.ends_with(".md") {
        eprintln!("fno-agents intel --render: {report} is not a .md report");
        return 2;
    }
    match render_report(Path::new(report)) {
        Ok(out) => {
            for note in &out.notes {
                eprintln!("{note}");
            }
            println!("{}", out.stamped.display());
            0
        }
        Err(msg) => {
            eprintln!("{msg}");
            2
        }
    }
}

/// The real work over one report path. `Err` carries the one refusal line;
/// `Ok` carries the outcome and its stderr notes.
pub(crate) fn render_report(md_path: &Path) -> Result<RenderOutcome, String> {
    let text = std::fs::read_to_string(md_path)
        .map_err(|e| format!("fno-agents intel --render: {}: {e}", md_path.display()))?;
    let (frontmatter, body_text) = split_frontmatter(&text);
    let mut notes: Vec<String> = Vec::new();

    // The fold named in the frontmatter `fold:` field. Absent, unreadable
    // or invalid: no chart is drawn, one line names the reason, exit 0.
    let fold: Option<Value> = match frontmatter.and_then(|fm| fold_field(fm)) {
        None => {
            notes.push("no fold: no frontmatter fold: field".to_string());
            None
        }
        Some(name) => match read_fold(&md_path.parent().unwrap_or(Path::new(".")), &name) {
            Ok(v) => Some(v),
            Err(reason) => {
                notes.push(format!("no fold: {reason}"));
                None
            }
        },
    };

    let mut render = Renderer::new(fold.as_ref());
    let body = render.render_body(body_text);
    let meta = header_meta(fold.as_ref());
    // The h1 title is already escaped by text_node; only the file-stem
    // fallback still needs it.
    let title = render.title.clone().unwrap_or_else(|| {
        html_escape_quote(
            &md_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Intel report".to_string()),
        )
    });
    let footer = format!(
        "Generated by fno-agents intel --render from {} on {}",
        md_path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    );

    let page = template()
        .replace("__TITLE__", &title)
        .replace("__META__", &meta)
        .replace("__BODY__", &body)
        .replace("__FOOTER__", &html_escape_quote(&footer));

    notes.push(format!(
        "scrubbed: {} secrets, {} paths, {} blocks, {} quotes",
        render.scrub.secrets, render.scrub.paths, render.scrub.blocks, render.scrub.quotes
    ));
    notes.append(&mut render.chart_notes);

    let dir = md_path.parent().unwrap_or(Path::new("."));
    let stamped = md_path.with_extension("html");
    let latest = dir.join("latest.html");
    crate::king_ledger::write_atomic(&stamped, &page)
        .map_err(|e| format!("fno-agents intel --render: {}: {e}", stamped.display()))?;
    crate::king_ledger::write_atomic(&latest, &page)
        .map_err(|e| format!("fno-agents intel --render: {}: {e}", latest.display()))?;

    let pruned = prune_stamped(dir);
    if pruned > 0 {
        notes.push(format!("pruned: {pruned}"));
    }
    Ok(RenderOutcome {
        stamped,
        latest,
        notes,
    })
}

/// Split a leading `---` fenced YAML frontmatter block off the text.
/// Returns `(Some(frontmatter_text), rest)` when the first line is `---`
/// and a closing `---` line follows.
fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    const FENCE: usize = "---\n".len();
    if !text.starts_with("---\n") {
        return (None, text);
    }
    let rest = &text[FENCE..];
    let mut walked = 0usize;
    for line in rest.lines() {
        if line.trim_end() == "---" {
            return (
                Some(&text[FENCE..FENCE + walked]),
                &text[(FENCE + walked + FENCE).min(text.len())..],
            );
        }
        walked += line.len() + 1;
    }
    (None, text)
}

/// The `fold:` value from frontmatter text.
fn fold_field(frontmatter: &str) -> Option<String> {
    frontmatter.lines().find_map(|l| {
        l.strip_prefix("fold:")
            .map(|v| v.trim().trim_matches('"').to_string())
            .filter(|v| !v.is_empty())
    })
}

fn read_fold(dir: &Path, name: &str) -> Result<Value, String> {
    let path = dir.join(name);
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("{}: invalid JSON: {e}", path.display()))
}

/// What the scrub replaced, for the one stderr line.
#[derive(Default)]
struct Scrub {
    secrets: usize,
    paths: usize,
    blocks: usize,
    quotes: usize,
}

/// The per-report render state: the fold reference, the scrub counters, the
/// collected title, and the section context the quote rule reads.
struct Renderer<'a> {
    fold: Option<&'a Value>,
    scrub: Scrub,
    title: Option<String>,
    /// The current `##` section title, for the corrections quote rule.
    section: Option<String>,
    /// Set once the Categories section is open, so `###` becomes a
    /// drill-down `<details>`.
    in_categories: bool,
    /// The usage section HTML, drawn once and inserted before Categories.
    usage_html: Option<String>,
    /// Chart stderr notes, drained after the body renders.
    chart_notes: Vec<String>,
}

impl<'a> Renderer<'a> {
    fn new(fold: Option<&'a Value>) -> Self {
        Renderer {
            fold,
            scrub: Scrub::default(),
            title: None,
            section: None,
            in_categories: false,
            usage_html: None,
            chart_notes: Vec::new(),
        }
    }
}

/// The secret shapes the scrub replaces with `[redacted]`. Leftmost-first:
/// the named shapes win before the generic 40-character run can touch them.
const SECRET_SHAPES: &str = concat!(
    "(?i)(",
    "-----BEGIN [A-Z ]*PRIVATE KEY-----",
    "|eyJ[A-Za-z0-9_-]+\\.[A-Za-z0-9_-]+\\.[A-Za-z0-9_-]+",
    "|sk-[A-Za-z0-9_-]{20,}",
    "|gh[pousr]_[A-Za-z0-9]{20,}",
    "|github_pat_[A-Za-z0-9_]{20,}",
    "|AKIA[0-9A-Z]{16}",
    "|xox[abprs]-[A-Za-z0-9-]{10,}",
    "|bearer\\s+\\S{16,}",
    "|((?:password|passwd|secret|token|api[_-]?key)\\s*[=:]\\s*)(\\S+)",
    "|[A-Za-z0-9+/_=-]{40,}",
    ")"
);

fn secret_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(SECRET_SHAPES).expect("static regex"))
}

fn path_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(r"(?:/Users|/home)/[A-Za-z0-9._-]+").expect("static regex"))
}

/// Straight and curly double-quoted spans.
fn quote_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| {
        Regex::new("\u{201c}[^\u{201c}\u{201d}\n]*\u{201d}|\"[^\"\n]*\"").expect("static regex")
    })
}

/// Scrub one text node. Corrections quotes are the one allowed quote; the
/// caller says whether the node sits in that section.
fn scrub_text(text: &str, in_corrections: bool, scrub: &mut Scrub) -> String {
    let mut out = text.to_string();
    let secrets = secret_rx().find_iter(&out).count();
    if secrets > 0 {
        out = secret_rx()
            .replace_all(&out, |caps: &regex::Captures| match caps.get(2) {
                Some(val) => format!("{}[redacted]", val.as_str()),
                None => "[redacted]".to_string(),
            })
            .into_owned();
        scrub.secrets += secrets;
    }
    let paths = path_rx().find_iter(&out).count();
    if paths > 0 {
        out = path_rx().replace_all(&out, "~").into_owned();
        scrub.paths += paths;
    }
    if !in_corrections {
        let quotes = quote_rx().find_iter(&out).count();
        if quotes > 0 {
            out = quote_rx().replace_all(&out, "[quote omitted]").into_owned();
            scrub.quotes += quotes;
        }
    }
    out
}

/// Inline grammar over already-escaped text: `**bold**`, backtick code,
/// and `#`/https links only. Every other target renders as its text.
fn render_inline(escaped: &str) -> String {
    static CODE: OnceLock<Regex> = OnceLock::new();
    static BOLD: OnceLock<Regex> = OnceLock::new();
    static LINK: OnceLock<Regex> = OnceLock::new();
    let code = CODE.get_or_init(|| Regex::new("`([^`]+)`").expect("static regex"));
    let bold = BOLD.get_or_init(|| Regex::new(r"\*\*([^*]+)\*\*").expect("static regex"));
    let link = LINK.get_or_init(|| Regex::new(r"\[([^\]]+)\]\(([^)\s]*)\)").expect("static regex"));
    let mut s = code.replace_all(escaped, "<code>$1</code>").into_owned();
    s = bold.replace_all(&s, "<strong>$1</strong>").into_owned();
    s = link
        .replace_all(&s, |caps: &regex::Captures| {
            let text = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let target = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let hrefable = target.starts_with('#') || target.starts_with("https://");
            if hrefable {
                format!("<a href=\"{target}\">{text}</a>")
            } else {
                text.to_string()
            }
        })
        .into_owned();
    s
}

/// A stable anchor slug for a category name.
fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    s.trim_matches('-').to_string()
}

const OMITTED: &str = "<p class=\"omitted\">[omitted: quoted block]</p>";

impl<'a> Renderer<'a> {
    /// Escape a scrubbed text node and apply the inline grammar over it.
    /// Escaping runs first, so raw HTML in the report prints as text.
    fn text_node(&mut self, raw: &str) -> String {
        let in_corrections = self.section.as_deref() == Some("Operator corrections");
        let scrubbed = scrub_text(raw, in_corrections, &mut self.scrub);
        render_inline(&html_escape_quote(&scrubbed))
    }
}

impl<'a> Renderer<'a> {
    /// Render the markdown body to HTML. Fenced blocks and blockquotes drop
    /// whole; headings, bullets and paragraphs map to section, details,
    /// list and paragraph elements; the usage section is inserted before
    /// Categories and the category bars right after the Categories heading.
    fn render_body(&mut self, text: &str) -> String {
        self.usage_html = self
            .fold
            .and_then(|f| usage_section(f, &mut self.chart_notes));
        let mut out: Vec<String> = Vec::new();
        let mut para: Vec<String> = Vec::new();
        let mut in_ul = false;
        let mut in_details = false;
        let mut in_fence = false;
        let mut quote_run = false;
        let mut open_section = false;
        let mut usage_done = false;
        for line in text.lines() {
            if in_fence {
                if line.trim_end().starts_with("```") {
                    in_fence = false;
                    out.push(OMITTED.to_string());
                }
                continue;
            }
            if line.starts_with("```") {
                in_fence = true;
                self.scrub.blocks += 1;
                flush_para(&mut para, &mut out, self);
                close_ul(&mut in_ul, &mut out);
                continue;
            }
            if line.starts_with("> ") || line == ">" {
                if !quote_run {
                    self.scrub.blocks += 1;
                    out.push(OMITTED.to_string());
                    quote_run = true;
                }
                flush_para(&mut para, &mut out, self);
                close_ul(&mut in_ul, &mut out);
                continue;
            }
            quote_run = false;
            if line.trim().is_empty() {
                flush_para(&mut para, &mut out, self);
                close_ul(&mut in_ul, &mut out);
                continue;
            }
            if let Some(h1) = line.strip_prefix("# ") {
                flush_para(&mut para, &mut out, self);
                close_ul(&mut in_ul, &mut out);
                if self.title.is_none() {
                    self.title = Some(self.text_node(h1));
                }
                continue;
            }
            if let Some(h2) = line.strip_prefix("## ") {
                flush_para(&mut para, &mut out, self);
                close_ul(&mut in_ul, &mut out);
                if in_details {
                    out.push("</details>".to_string());
                    in_details = false;
                }
                let title_text = self.text_node(h2);
                self.section = Some(h2.trim().to_string());
                if h2.trim() == "Categories" && !usage_done {
                    if let Some(html) = self.usage_html.take() {
                        out.push(html);
                    }
                    usage_done = true;
                }
                if open_section {
                    out.push("</section>".to_string());
                }
                out.push("<section>".to_string());
                open_section = true;
                let is_categories = h2.trim() == "Categories";
                self.in_categories = is_categories;
                out.push(format!("<h2>{title_text}</h2>"));
                if is_categories {
                    if let Some(bars) = self
                        .fold
                        .and_then(|f| category_bars(f, &mut self.chart_notes))
                    {
                        out.push(bars);
                    }
                }
                continue;
            }
            if let Some(h3) = line.strip_prefix("### ") {
                flush_para(&mut para, &mut out, self);
                close_ul(&mut in_ul, &mut out);
                if in_details {
                    out.push("</details>".to_string());
                }
                let title_text = self.text_node(h3);
                if self.in_categories {
                    in_details = true;
                    out.push(format!(
                        "<details open id=\"c-{}\"><summary>{title_text}</summary>",
                        slug(h3)
                    ));
                } else {
                    out.push(format!("<h3>{title_text}</h3>"));
                }
                continue;
            }
            if let Some(bullet) = line.strip_prefix("- ") {
                flush_para(&mut para, &mut out, self);
                if !in_ul {
                    out.push("<ul>".to_string());
                    in_ul = true;
                }
                if let Some(sub) = bullet.strip_prefix("#### ") {
                    let text = self.text_node(sub);
                    out.push(format!("<li><strong>{text}</strong></li>"));
                    continue;
                }
                if let Some((id, at)) = anchor_id(bullet) {
                    let text = self.text_node(&bullet[at..]);
                    out.push(format!("<li id=\"{id}\">{text}</li>"));
                    continue;
                }
                let text = self.text_node(bullet);
                out.push(format!("<li>{text}</li>"));
                continue;
            }
            para.push(line.to_string());
        }
        flush_para(&mut para, &mut out, self);
        close_ul(&mut in_ul, &mut out);
        if in_details {
            out.push("</details>".to_string());
        }
        if open_section {
            out.push("</section>".to_string());
        }
        if !usage_done {
            if let Some(html) = self.usage_html.take() {
                out.push(html);
            }
        }
        out.join("\n")
    }
}

/// Emit buffered paragraph lines as one `<p>`, each line scrubbed and
/// rendered at flush time.
fn flush_para(para: &mut Vec<String>, out: &mut Vec<String>, render: &mut Renderer) {
    if para.is_empty() {
        return;
    }
    let raw: Vec<String> = para.drain(..).collect();
    let rendered: Vec<String> = raw.iter().map(|l| render.text_node(l)).collect();
    out.push(format!("<p>{}</p>", rendered.join(" ")));
}

fn close_ul(in_ul: &mut bool, out: &mut Vec<String>) {
    if *in_ul {
        out.push("</ul>".to_string());
        *in_ul = false;
    }
}

/// A Sessions bullet's `<a id="s-<8 hex>"></a>` prefix: the anchor id and
/// the offset just past `</a>`. A malformed anchor reads as plain text.
fn anchor_id(text: &str) -> Option<(String, usize)> {
    static RX: OnceLock<Regex> = OnceLock::new();
    let rx =
        RX.get_or_init(|| Regex::new("^<a id=\"(s-[0-9a-f]{8})\"></a>").expect("static regex"));
    let caps = rx.captures(text)?;
    let end = caps.get(0)?.end();
    Some((caps.get(1)?.as_str().to_string(), end))
}

fn template() -> &'static str {
    include_str!("intel_page.html")
}

fn display_opt(v: Option<u64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "unknown".to_string(),
    }
}

/// The header meta line: two labeled populations, the window, the sample
/// receipt, and the scope. A key that is absent prints `unknown`, never 0.
fn header_meta(fold: Option<&Value>) -> String {
    let pops = fold.and_then(|f| f.get(KEY_POPULATIONS));
    let scanned = pops
        .and_then(|p| p.get(KEY_SCANNED))
        .and_then(Value::as_u64);
    let judged = fold
        .and_then(|f| f.get(KEY_CATEGORIES))
        .and_then(|c| c.get(KEY_JUDGED))
        .and_then(Value::as_u64)
        .or_else(|| pops.and_then(|p| p.get(KEY_JUDGED)).and_then(Value::as_u64));
    let mut parts = vec![
        format!(
            "<strong>Sessions scanned: {}</strong>",
            display_opt(scanned)
        ),
        format!("<strong>Sessions judged: {}</strong>", display_opt(judged)),
        window_text(fold),
    ];
    if let Some(p) = pops {
        if let (Some(s), Some(e)) = (
            p.get(KEY_SAMPLED).and_then(Value::as_u64),
            p.get(KEY_ELIGIBLE).and_then(Value::as_u64),
        ) {
            parts.push(format!("sampled {s} of {e} eligible"));
        }
    }
    if let Some(scope) = fold.and_then(|f| f.get(KEY_SCOPE)) {
        let harnesses: Vec<&str> = scope
            .get(KEY_HARNESSES)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let all = scope
            .get("all_projects")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let project = if all {
            "all".to_string()
        } else {
            scope
                .get("projects")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_else(|| "all".to_string())
        };
        parts.push(format!(
            "harness={} project={}",
            html_escape_quote(&harnesses.join(",")),
            html_escape_quote(&project)
        ));
    }
    parts.join(" · ")
}

fn window_text(fold: Option<&Value>) -> String {
    let Some(f) = fold else {
        return "window unknown".to_string();
    };
    let (first, last) = day_range(f.get(KEY_DAILY));
    match f.get(KEY_DAYS).and_then(Value::as_u64) {
        None => "window unknown".to_string(),
        Some(0) => match (first, last) {
            (Some(a), Some(b)) => format!("all time ({a} to {b})"),
            _ => "all time".to_string(),
        },
        Some(d) => match (first, last) {
            (Some(a), Some(b)) => format!("last {d} days ({a} to {b})"),
            _ => format!("last {d} days"),
        },
    }
}

fn day_range(daily: Option<&Value>) -> (Option<String>, Option<String>) {
    let Some(list) = daily.and_then(Value::as_array) else {
        return (None, None);
    };
    let mut dates: Vec<&str> = list
        .iter()
        .filter_map(|e| e.get(KEY_DATE))
        .filter_map(Value::as_str)
        .collect();
    dates.sort_unstable();
    (
        dates.first().map(|s| (*s).to_string()),
        dates.last().map(|s| (*s).to_string()),
    )
}

/// One track-plus-fill bar row (category distribution, tool errors).
fn bar_row(label: &str, aria: &str, width: f64) -> String {
    format!(
        "<div class=\"cat\"><div class=\"label\">{label}</div><svg role=\"img\" \
         aria-label=\"{aria}\" viewBox=\"0 0 100 4\" preserveAspectRatio=\"none\">\
         <rect class=\"track\" x=\"0\" y=\"0\" width=\"100\" height=\"4\"/>\
         <rect class=\"bar\" x=\"0\" y=\"0\" width=\"{width:.1}\" height=\"4\"/></svg></div>"
    )
}

/// The category distribution bars, drawn from `categories.items`. Missing
/// block, empty items: one note, no chart.
fn category_bars(fold: &Value, notes: &mut Vec<String>) -> Option<String> {
    let Some(items) = fold
        .get(KEY_CATEGORIES)
        .and_then(|c| c.get(KEY_ITEMS))
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    else {
        notes.push("no category chart: the fold holds no categories items".to_string());
        return None;
    };
    let judged = fold
        .get(KEY_CATEGORIES)
        .and_then(|c| c.get(KEY_JUDGED))
        .and_then(Value::as_u64);
    let mut rows = String::new();
    for item in items {
        let name = item.get(KEY_NAME).and_then(Value::as_str).unwrap_or("?");
        // The fold carries the share under `metrics` (category_metrics);
        // a top-level `share_pct` is the tolerated fallback.
        let share = item
            .get("metrics")
            .and_then(|m| m.get(KEY_SHARE))
            .and_then(Value::as_f64)
            .or_else(|| item.get(KEY_SHARE).and_then(Value::as_f64))
            .unwrap_or(0.0);
        let esc = html_escape_quote(name);
        rows.push_str(&bar_row(
            &format!("{esc} ({share}%)"),
            &format!("{esc}: {share}%"),
            share.clamp(0.0, 100.0),
        ));
    }
    Some(format!(
        "<figure><figcaption>Category distribution, {} judged sessions</figcaption>\
         {rows}</figure>",
        display_opt(judged)
    ))
}

fn harness_class(name: &str) -> &'static str {
    match name {
        "claude" => "col-c",
        "codex" => "col-x",
        _ => "col-o",
    }
}

/// One stacked-column figure. `slots` holds one entry per column, each a
/// list of (harness class, value) segments; a slot with no segments reads
/// as the empty slot a zero day draws.
fn columns_svg(
    slots: &[Vec<(&str, u64)>],
    legend: &str,
    aria: &str,
    figcaption: &str,
    table: &str,
    max: u64,
) -> String {
    let n = slots.len().max(1);
    let slot = 100.0 / n as f64;
    let width = (slot - 0.6).max(0.4);
    let mut rects = String::new();
    for (i, segs) in slots.iter().enumerate() {
        let x = i as f64 * slot;
        rects.push_str(&format!(
            "<rect class=\"track\" x=\"{x:.2}\" y=\"0\" width=\"{width:.2}\" height=\"40\"/>"
        ));
        let mut cum = 0u64;
        for (cls, v) in segs {
            if *v == 0 {
                continue;
            }
            let h = (*v as f64 / max as f64 * 40.0).max(0.5);
            let y = 40.0 - (cum as f64 / max as f64 * 40.0) - h;
            rects.push_str(&format!(
                "<rect class=\"{cls}\" x=\"{x:.2}\" y=\"{y:.2}\" width=\"{width:.2}\" height=\"{h:.2}\"/>"
            ));
            cum += v;
        }
    }
    format!(
        "<figure>{legend}<svg role=\"img\" aria-label=\"{aria}\" viewBox=\"0 0 100 40\" \
         preserveAspectRatio=\"none\">{rects}</svg><figcaption>{figcaption}</figcaption>\
         <details><summary>Data</summary>{table}</details></figure>"
    )
}

/// One per-day stacked chart from the fold's `daily` rows, keyed by
/// `value_key` (sessions or operator_turns). The renderer computes no
/// bucket: it draws the rows the fold holds, one slot per distinct date.
fn daily_chart(
    fold: &Value,
    value_key: &str,
    label: &str,
    pop: &str,
    notes: &mut Vec<String>,
) -> Option<String> {
    let chart = if value_key == KEY_SESSIONS {
        "sessions-per-day"
    } else {
        "operator-turns-per-day"
    };
    let Some(rows) = fold.get(KEY_DAILY).and_then(Value::as_array) else {
        notes.push(format!("no {chart} chart: the fold holds no daily series"));
        return None;
    };
    let mut by_date: BTreeMap<&str, BTreeMap<&str, u64>> = BTreeMap::new();
    for row in rows {
        let date = row.get(KEY_DATE).and_then(Value::as_str);
        let harness = row.get(KEY_HARNESS).and_then(Value::as_str);
        let value = row.get(value_key).and_then(Value::as_u64);
        match (date, harness, value) {
            (Some(d), Some(h), Some(v)) => {
                *by_date.entry(d).or_default().entry(h).or_insert(0) += v;
            }
            _ => {
                notes.push(format!(
                    "no {chart} chart: daily row is missing or mistyped"
                ));
                return None;
            }
        }
    }
    let max = by_date
        .values()
        .map(|h| h.values().sum::<u64>())
        .max()
        .unwrap_or(0);
    if max == 0 {
        notes.push(format!("no {chart} chart: the daily series is all zero"));
        return None;
    }
    let mut harnesses: Vec<&str> = by_date.values().flat_map(|h| h.keys().copied()).collect();
    harnesses.sort_unstable();
    harnesses.dedup();
    let mut slots: Vec<Vec<(&str, u64)>> = Vec::new();
    let mut table = String::from("<table><tr><th>date</th>");
    for h in &harnesses {
        table.push_str(&format!("<th>{}</th>", html_escape_quote(h)));
    }
    table.push_str("</tr>");
    for (date, by_harness) in &by_date {
        let mut segs: Vec<(&str, u64)> = Vec::new();
        table.push_str(&format!("<tr><td>{}</td>", html_escape_quote(date)));
        for h in &harnesses {
            let v = by_harness.get(h).copied().unwrap_or(0);
            segs.push((harness_class(h), v));
            table.push_str(&format!("<td>{v}</td>"));
        }
        table.push_str("</tr>");
        slots.push(segs);
    }
    table.push_str("</table>");
    let mut legend = String::from("<div class=\"legend\">");
    for h in &harnesses {
        legend.push_str(&format!(
            "<i class=\"{}\"></i>{}",
            harness_class(h),
            html_escape_quote(h)
        ));
    }
    legend.push_str("</div>");
    let figcaption = format!("{label}, {pop}");
    Some(columns_svg(
        &slots,
        &legend,
        &format!("{label}; largest day {max}"),
        &figcaption,
        &table,
        max,
    ))
}

/// Operator messages by hour of day: the 24 buckets the fold holds, in the
/// fold's timezone. The renderer never re-buckets an hour.
fn hours_chart(fold: &Value, pop: &str, notes: &mut Vec<String>) -> Option<String> {
    // A mistyped bucket drops the whole chart (the AC5 rule): no bucket
    // ever reads as a silent zero.
    let buckets = fold
        .get(KEY_HOURS)
        .and_then(|h| h.get(KEY_TURNS))
        .and_then(Value::as_array);
    let good = buckets.is_some_and(|b| b.len() == 24 && b.iter().all(|v| v.as_u64().is_some()));
    if !good {
        notes.push("no hour-of-day chart: the fold holds no 24-bucket hours series".to_string());
        return None;
    }
    let vals: Vec<u64> = buckets.unwrap().iter().filter_map(Value::as_u64).collect();
    let max = vals.iter().max().copied().unwrap_or(0);
    if max == 0 {
        notes.push("no hour-of-day chart: the hours series is all zero".to_string());
        return None;
    }
    let tz = fold
        .get(KEY_HOURS)
        .and_then(|h| h.get(KEY_UTC_OFFSET))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let mut simple: Vec<Vec<(&str, u64)>> = Vec::new();
    for v in &vals {
        simple.push(vec![("bar", *v)]);
    }
    let mut table = String::from("<table><tr><th>hour</th><th>operator turns</th></tr>");
    for (hour, v) in vals.iter().enumerate() {
        table.push_str(&format!("<tr><td>{hour:02}</td><td>{v}</td></tr>"));
    }
    table.push_str("</table>");
    let figcaption = format!("Operator messages by hour of day, hours in {tz}, {pop}");
    Some(columns_svg(
        &simple,
        "",
        &format!("Operator messages by hour of day; largest hour {max}"),
        &figcaption,
        &table,
        max,
    ))
}

/// Operator response-time histogram: the buckets in the order the JSON
/// gives, labels as given.
fn response_chart(fold: &Value, pop: &str, notes: &mut Vec<String>) -> Option<String> {
    let Some(buckets) = fold
        .get(KEY_RESPONSE_TIME)
        .and_then(|r| r.get(KEY_BUCKETS))
        .and_then(Value::as_object)
    else {
        notes.push("no response-time chart: the fold holds no response_time buckets".to_string());
        return None;
    };
    let vals: Vec<(&String, &Value)> = buckets.iter().collect();
    if vals.is_empty() || vals.iter().all(|(_, v)| v.as_u64().unwrap_or(0) == 0) {
        notes.push("no response-time chart: the buckets are empty or all zero".to_string());
        return None;
    }
    let n = fold
        .get(KEY_RESPONSE_TIME)
        .and_then(|r| r.get("n"))
        .and_then(Value::as_u64);
    let mut slots: Vec<Vec<(&str, u64)>> = Vec::new();
    let mut table = String::from("<table><tr><th>gap</th><th>gaps</th></tr>");
    for (label, v) in &vals {
        let count = v.as_u64().unwrap_or(0);
        slots.push(vec![("bar", count)]);
        table.push_str(&format!("<tr><td>{label}</td><td>{count}</td></tr>"));
    }
    table.push_str("</table>");
    let largest = vals
        .iter()
        .map(|(_, v)| v.as_u64().unwrap_or(0))
        .max()
        .unwrap_or(0);
    let gaps = display_opt(n);
    let figcaption = format!("Operator response time ({gaps} gaps), {pop}");
    Some(columns_svg(
        &slots,
        "",
        &format!("Operator response time; largest bucket {largest}"),
        &figcaption,
        &table,
        largest.max(1),
    ))
}

/// Tool errors by class: horizontal bars, sorted by count descending, then
/// class.
fn tool_errors_chart(fold: &Value, pop: &str, notes: &mut Vec<String>) -> Option<String> {
    let Some(errs) = fold
        .get(KEY_ACTIVITY)
        .and_then(|a| a.get(KEY_TOOL_ERRORS))
        .and_then(Value::as_object)
        .filter(|m| !m.is_empty())
    else {
        notes.push("no tool-error chart: the fold holds no tool_errors map".to_string());
        return None;
    };
    let mut list: Vec<(&str, u64)> = errs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_u64().unwrap_or(0)))
        .collect();
    if list.iter().all(|(_, v)| *v == 0) {
        notes.push("no tool-error chart: the tool_errors map is all zero".to_string());
        return None;
    }
    list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let max = list.first().map(|(_, v)| *v).unwrap_or(1).max(1);
    let mut rows = String::new();
    let mut table = String::from("<table><tr><th>class</th><th>errors</th></tr>");
    for (class, v) in &list {
        let esc = html_escape_quote(class);
        rows.push_str(&bar_row(
            &format!("{esc} ({v})"),
            &format!("{esc}: {v} errors"),
            *v as f64 / max as f64 * 100.0,
        ));
    }
    for (class, v) in &list {
        table.push_str(&format!("<tr><td>{class}</td><td>{v}</td></tr>"));
    }
    table.push_str("</table>");
    Some(format!(
        "<figure><figcaption>Tool errors by class, {pop}</figcaption>{rows}\
         <details><summary>Data</summary>{table}</details></figure>"
    ))
}

/// The five usage charts in one section, or none when nothing drew.
fn usage_section(fold: &Value, notes: &mut Vec<String>) -> Option<String> {
    let scanned = fold
        .get(KEY_POPULATIONS)
        .and_then(|p| p.get(KEY_SCANNED))
        .and_then(Value::as_u64);
    let pop = format!("all {} scanned sessions", display_opt(scanned));
    let mut charts = String::new();
    let mut any = false;
    for html in [
        daily_chart(
            fold,
            KEY_SESSIONS,
            "Sessions per day by harness",
            &pop,
            notes,
        ),
        daily_chart(
            fold,
            KEY_TURNS,
            "Operator turns per day by harness",
            &pop,
            notes,
        ),
        hours_chart(fold, &pop, notes),
        response_chart(fold, &pop, notes),
        tool_errors_chart(fold, &pop, notes),
    ]
    .into_iter()
    .flatten()
    {
        charts.push_str(&html);
        any = true;
    }
    if !any {
        return None;
    }
    Some(format!("<section id=\"usage\">{charts}</section>"))
}

/// Keep the 12 newest stamped HTML files (`<date>-<8 hex>.html`) in the
/// intel directory; delete older ones. The markdown, the fold JSON,
/// `latest.html` and any other name are never candidates. A failed delete
/// names itself on stderr and never fails the render.
fn prune_stamped(dir: &Path) -> usize {
    static RX: OnceLock<Regex> = OnceLock::new();
    let rx = RX.get_or_init(|| {
        Regex::new(r"^\d{4}-\d{2}-\d{2}-[0-9a-f]{8}\.html$").expect("static regex")
    });
    let Ok(read) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut stamped: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| rx.is_match(n))
                .unwrap_or(false)
        })
        .collect();
    stamped.sort_unstable();
    stamped.reverse();
    let mut pruned = 0;
    for old in stamped.into_iter().skip(12) {
        match std::fs::remove_file(&old) {
            Ok(()) => pruned += 1,
            Err(e) => eprintln!("prune: {}: {e}", old.display()),
        }
    }
    pruned
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_MD: &str = include_str!("../tests/fixtures/intel_report.md");
    const FIXTURE_FOLD: &str = include_str!("../tests/fixtures/intel_report.json");
    const BARE_FOLD: &str = include_str!("../tests/fixtures/intel_report_bare.json");

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fno-intel-html-{}-{tag}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The fixture report under `fold_name`, rendered. Returns the dir and
    /// the outcome.
    fn render_fixture(tag: &str, fold_name: &str, fold_body: &str) -> (PathBuf, RenderOutcome) {
        let dir = temp_dir(tag);
        let md = FIXTURE_MD.replace("fold: intel_report.json", &format!("fold: {fold_name}"));
        std::fs::write(dir.join("2026-09-21-0a1b2c3d.md"), md).unwrap();
        std::fs::write(dir.join(fold_name), fold_body).unwrap();
        let out = render_report(&dir.join("2026-09-21-0a1b2c3d.md")).unwrap();
        (dir, out)
    }

    fn rendered_page(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("2026-09-21-0a1b2c3d.html")).unwrap()
    }

    #[test]
    fn ac1_categories_drilldowns_and_anchors_render() {
        let (dir, _out) = render_fixture("ac1", "intel_report.json", FIXTURE_FOLD);
        let page = rendered_page(&dir);
        assert_eq!(page.matches("<details open").count(), 3, "3 drill-downs");
        assert_eq!(page.matches("<li id=\"s-").count(), 3, "3 session anchors");
        for (name, share) in [
            ("Relay handoffs", "48.8"),
            ("Review gate", "4.9"),
            ("Coordination", "46.3"),
        ] {
            let expected = format!("aria-label=\"{name}: {share}%\"");
            assert!(page.contains(&expected), "missing bar label {expected}");
        }
        for m in ["prefers-color-scheme: dark", "@media print"] {
            assert!(page.contains(m), "css missing {m}");
        }
        // Every Executive-summary anchor href resolves to a session li id.
        let ids: Vec<String> = page
            .match_indices("<li id=\"s-")
            .map(|(i, _)| {
                let rest = &page[i + 8..];
                rest[..rest.find('"').unwrap()].to_string()
            })
            .collect();
        let hrefs: Vec<String> = page
            .match_indices("href=\"#s-")
            .map(|(i, _)| {
                let rest = &page[i + 7..];
                let rest = &rest[..rest.find('"').unwrap()];
                rest.to_string()
            })
            .collect();
        assert!(!hrefs.is_empty());
        for href in hrefs {
            assert!(ids.contains(&href), "href {href} resolves to no session");
        }
        let _ = dir;
    }

    #[test]
    fn ac2_self_contained_no_script_no_external_assets() {
        let (dir, _out) = render_fixture("ac2", "intel_report.json", FIXTURE_FOLD);
        let page = rendered_page(&dir);
        for banned in ["<script", "<link", "<img", "@import", "url(", "src="] {
            assert!(!page.contains(banned), "page must not hold {banned}");
        }
    }

    #[test]
    fn ac3_planted_secrets_paths_blocks_and_quotes_never_reach_the_page() {
        let (dir, out) = render_fixture("ac3", "intel_report.json", FIXTURE_FOLD);
        let page = rendered_page(&dir);
        for planted in [
            "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8",
            "sk-ant-api03-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t0",
            "AKIAIOSFODNN7EXAMPLE",
            "hunter2",
            "/Users/alice",
            "<script>alert(1)",
            "a fenced transcript paste",
        ] {
            assert!(
                !page.contains(planted),
                "planted {planted} reached the page"
            );
        }
        for marker in [
            "[redacted]",
            "~/code",
            "[omitted: quoted block]",
            "[quote omitted]",
        ] {
            assert!(page.contains(marker), "page lacks {marker}");
        }
        let correction = "rotate the [redacted] before you push";
        assert!(
            page.contains(correction),
            "correction line lost: {correction}"
        );
        assert!(out.notes.iter().any(|n| n.starts_with("scrubbed: ")));
    }

    #[test]
    fn ac4_refusals_exit_two_and_write_nothing() {
        let dir = temp_dir("ac4");
        let md = dir.join("report.txt");
        std::fs::write(&md, "not a report").unwrap();
        assert_eq!(run_render(&[]), 2);
        assert_eq!(run_render(&["--render".to_string()]), 2);
        assert_eq!(
            run_render(&[vec!["--render".to_string(), md.display().to_string()].join(" ")]),
            2
        );
        let missing = dir.join("gone.md");
        assert_eq!(
            run_render(&["--render".to_string(), missing.display().to_string()]),
            2
        );
        let extra = dir.join("report.md");
        std::fs::write(&extra, "x").unwrap();
        assert_eq!(
            run_render(&[
                "--render".to_string(),
                extra.display().to_string(),
                "--days".to_string(),
                "7".to_string(),
            ]),
            2
        );
        assert_eq!(
            run_render(&["--render".to_string(), md.display().to_string()]),
            2
        );
        assert!(
            !dir.join("report.html").exists(),
            "no file for a refused run"
        );
    }

    #[test]
    fn ac5_missing_or_absent_fold_renders_without_chart() {
        // No frontmatter at all.
        let dir = temp_dir("ac5a");
        std::fs::write(dir.join("plain.md"), "# Intel: plain\n\nBody text.\n").unwrap();
        let out = render_report(&dir.join("plain.md")).unwrap();
        let page = std::fs::read_to_string(dir.join("plain.html")).unwrap();
        assert!(!page.contains("<svg"), "no chart without a fold");
        assert!(out.notes.iter().any(|n| n.starts_with("no fold:")));
        // A frontmatter fold that does not exist on disk: the md names it,
        // the file is never written.
        let dir = temp_dir("ac5b");
        let md = FIXTURE_MD.replace("fold: intel_report.json", "fold: gone.json");
        std::fs::write(dir.join("2026-09-21-0a1b2c3d.md"), md).unwrap();
        let out = render_report(&dir.join("2026-09-21-0a1b2c3d.md")).unwrap();
        let page = rendered_page(&dir);
        assert!(!page.contains("<svg"));
        assert!(out.notes.iter().any(|n| n.contains("gone.json")));
    }

    #[test]
    fn ac6_raw_html_bad_anchors_and_file_links_print_as_text() {
        let (dir, _out) = render_fixture("ac6", "intel_report.json", FIXTURE_FOLD);
        let page = rendered_page(&dir);
        assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(page.contains("&lt;a id=\"s-zz\"&gt;"));
        assert!(
            !page.contains("href=\"file"),
            "file links never become hrefs"
        );
    }

    #[test]
    fn ac9_five_usage_charts_with_slots_tables_and_order() {
        let (dir, _out) = render_fixture("ac9", "intel_report.json", FIXTURE_FOLD);
        let page = rendered_page(&dir);
        let usage = page.split("<section id=\"usage\">").nth(1).unwrap();
        let usage = &usage[..usage.find("</section>").unwrap()];
        assert_eq!(usage.matches("<figure>").count(), 5, "five usage charts");
        // Per-day charts: one slot per distinct date (30, incl. the zero
        // day); hours hold 24; the response histogram draws 4 bucket
        // columns and the tool-error bars 3 tracks.
        let track = usage.matches("class=\"track\"").count();
        assert_eq!(
            track,
            30 + 30 + 24 + 4 + 3,
            "30 + 30 slots + 24 hours + 4 buckets + 3 bars"
        );
        assert!(usage.contains("hours in +02:00"), "tz caption");
        // Tool-error bars run in descending count order: Bash, Edit, WebFetch.
        let bash = usage.find("Bash (9)").unwrap();
        let edit = usage.find("Edit (4)").unwrap();
        let web = usage.find("WebFetch (1)").unwrap();
        assert!(
            bash < edit && edit < web,
            "tool errors sorted by count desc"
        );
        for part in ["<figcaption>", "<details><summary>Data</summary><table>"] {
            assert_eq!(usage.matches(part).count(), 5, "each chart holds {part}");
        }
        assert!(
            usage.contains("<td>90</td>"),
            "response bucket numbers in table"
        );
    }

    #[test]
    fn ac10_two_labeled_populations_and_per_chart_captions() {
        let (dir, _out) = render_fixture("ac10", "intel_report.json", FIXTURE_FOLD);
        let page = rendered_page(&dir);
        assert!(page.contains("<strong>Sessions scanned: 132</strong>"));
        assert!(page.contains("<strong>Sessions judged: 41</strong>"));
        assert!(page.contains("Category distribution, 41 judged sessions"));
        assert!(page.contains("all 132 scanned sessions"));
    }

    #[test]
    fn ac11_bare_fold_drops_each_chart_and_judged_reads_unknown() {
        let (dir, out) = render_fixture("ac11", "intel_report_bare.json", BARE_FOLD);
        let page = rendered_page(&dir);
        assert!(!page.contains("<section id=\"usage\">"), "no usage section");
        assert!(page.contains("<strong>Sessions judged: unknown</strong>"));
        for chart in [
            "no sessions-per-day chart:",
            "no operator-turns-per-day chart:",
            "no hour-of-day chart:",
            "no response-time chart:",
            "no tool-error chart:",
            "no category chart:",
        ] {
            assert!(out.notes.iter().any(|n| n.contains(chart)), "note {chart}");
        }
    }

    #[test]
    fn ac12_stamped_and_latest_are_identical_mode_0600() {
        let (_dir, out) = render_fixture("ac12", "intel_report.json", FIXTURE_FOLD);
        let stamped = std::fs::read(&out.stamped).unwrap();
        let latest = std::fs::read(&out.latest).unwrap();
        assert_eq!(stamped, latest);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&out.stamped, &out.latest] {
                let mode = std::fs::metadata(p).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{p:?} not 0600");
            }
        }
    }

    #[test]
    fn ac13_retention_keeps_the_twelve_newest_stamped_files() {
        let (dir, out) = render_fixture("ac13", "intel_report.json", FIXTURE_FOLD);
        // 14 older stamped files: 2026-09-01 .. 2026-09-14 (newest first per
        // date, distinct hex tails).
        for i in 1..=14u32 {
            let name = format!("2026-09-{:02}-{:08x}.html", i, 0x1111_0000u32 + i);
            std::fs::write(dir.join(name), "old").unwrap();
            std::fs::write(dir.join(format!("2026-09-{:02}-pair.md", i)), "pair").unwrap();
            std::fs::write(dir.join(format!("2026-09-{:02}-pair.json", i)), "{}").unwrap();
        }
        std::fs::write(dir.join("notes.html"), "stray").unwrap();
        let out2 = render_report(&dir.join("2026-09-21-0a1b2c3d.md")).unwrap();
        assert_eq!(out2.stamped, out.stamped);
        let pruned_note = out2
            .notes
            .iter()
            .find(|n| n.starts_with("pruned: "))
            .unwrap();
        assert_eq!(pruned_note, "pruned: 3");
        let stamped: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                regex::Regex::new(r"^\d{4}-\d{2}-\d{2}-[0-9a-f]{8}\.html$")
                    .unwrap()
                    .is_match(&n)
            })
            .collect();
        assert_eq!(stamped.len(), 12, "12 newest stamped remain");
        assert!(dir.join("latest.html").exists());
        assert!(dir.join("notes.html").exists());
        assert!(dir.join("2026-09-01-pair.md").exists());
        assert!(dir.join("2026-09-14-pair.json").exists());
        assert!(
            !dir.join("2026-09-01-11110001.html").exists(),
            "oldest pruned"
        );
        assert!(
            !dir.join("2026-09-03-11110003.html").exists(),
            "three oldest gone"
        );
    }
}
