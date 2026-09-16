//! `fno-agents judge` -- the grading half of the advisory five-question
//! blueprint judge. Daemon-free, like `graph-get`/`bash-census`: never a
//! routable `fno agents <verb>` invocation, though listed in
//! `ALL_CLIENT_ACTIONS` for the binary's own verb-surface probe.
//!
//! Ported off `cli/src/fno/observer/judge.py` per the flag-registry ratchet
//! (operator ruling 2026-09-12): a new CLI flag is Rust work.
//! This verb owns the lens prompts, the model spawn, and verdict parsing; it
//! emits no events and mints no run_id. The Python wrapper
//! (`cli/src/fno/observer/cli.py::judge_cmd`/`sweep`) still does the
//! pre-existing plan-parsing gate (`has_section("Five questions")`) and event
//! emission through `fno.events` -- reusing that single-sourced path rather
//! than growing a second one here (AGENTS.md principle 9).
//!
//! One JSON object on stdout, always exit 0 for the single-plan mode
//! (advisory, never fails); calibration mode exits 1 when any control
//! disagrees, matching the retired Python `judge_cmd --labels` contract.

use crate::evidence::truncate_chars;
use crate::graph_get::{default_graph_path, find_entry};
use crate::graph_store::{read_defaulted, s_str};
use crate::paths::worktree_repo_root;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const JUDGE_DIMENSIONS: [&str; 9] = [
    "persona",
    "surface_fit",
    "uncovered_case",
    "deletable",
    "duplication",
    "epic_fit",
    "mission_fit",
    "customer_fit",
    "code_truth",
];

/// The dimensions whose reader grades against one outside source, each
/// served by [`source_bundle`]; the rest grade the plan text alone.
const SOURCE_DIMENSIONS: [&str; 4] = ["epic_fit", "mission_fit", "customer_fit", "code_truth"];
pub const JUDGE_MODEL: &str = "sonnet";

/// Lean readers answered in 7 to 80 seconds (measured 2026-09-15); 240
/// covers a slow one and nine readers stay under the Python wrapper's
/// 3,600-second bound. The full session load timed out at 600.
const READER_TIMEOUT_SECS: u64 = 240;

/// The lens directory under the repo root or the deployed plugin root: one
/// file per dimension plus preamble.md, read by the judge, never by the
/// planner (`disable-model-invocation` keeps it that way).
const LENS_SUBDIR: &str = "skills/pm-plan-review/lenses";

type Lenses = (String, HashMap<String, String>);

/// The checkout's own lens directory wins (a footnote worktree is fresher
/// than the deployed stage); else the deployed plugin root's; else None.
fn lens_dir(repo_root: &Path, plugin_root: Option<&Path>) -> Option<PathBuf> {
    let own = repo_root.join(LENS_SUBDIR);
    if own.is_dir() {
        return Some(own);
    }
    let deployed = plugin_root?.join(LENS_SUBDIR);
    deployed.is_dir().then_some(deployed)
}

/// Drop a lens file's leading `# ` title line and trim; the title is for the
/// person browsing the directory, never for the prompt.
fn drop_title(text: &str) -> String {
    match text.strip_prefix("# ") {
        Some(rest) => match rest.split_once('\n') {
            Some((_, body)) => body.trim().to_string(),
            None => String::new(),
        },
        None => text.trim().to_string(),
    }
}

/// (shared preamble, {dimension: section body}). A missing directory or an
/// unreadable file is simply absent, and a missing lens is a gap the row
/// reports, never a silent default.
fn load_lenses(dir: Option<&Path>) -> Lenses {
    let Some(dir) = dir else {
        return (String::new(), HashMap::new());
    };
    let mut preamble = String::new();
    if let Ok(text) = std::fs::read_to_string(dir.join("preamble.md")) {
        preamble = drop_title(&text);
    }
    let mut sections = HashMap::new();
    for name in JUDGE_DIMENSIONS {
        if let Ok(text) = std::fs::read_to_string(dir.join(format!("{name}.md"))) {
            sections.insert(name.to_string(), drop_title(&text));
        }
    }
    (preamble, sections)
}

/// Code context for surface_fit/duplication; "" for other lenses or any fault.
fn gather_context(dimension: &str, plan_text: &str, cwd: &Path) -> String {
    if dimension == "surface_fit" {
        return match Command::new("fno").args(["help", "--all"]).output() {
            Ok(o) if o.status.success() => {
                truncate_chars(&String::from_utf8_lossy(&o.stdout), 4000)
            }
            _ => String::new(),
        };
    }
    if dimension != "duplication" {
        return String::new();
    }
    let sym_re = Regex::new(r"`([\w./-]{4,60})`").unwrap();
    let mut seen = HashSet::new();
    let mut syms: Vec<String> = Vec::new();
    for cap in sym_re.captures_iter(plan_text) {
        let s = cap[1].to_string();
        if syms.len() >= 12 {
            break;
        }
        if seen.insert(s.clone()) {
            syms.push(s);
        }
    }
    let mut chunks: Vec<String> = Vec::new();
    if !syms.is_empty() {
        let mut args: Vec<String> = vec!["-l".into(), "-F".into()];
        for s in &syms {
            args.push("-e".into());
            args.push(s.clone());
        }
        args.push("--glob".into());
        args.push("!graphify-out/**".into());
        args.push(".".into());
        if let Ok(h) = Command::new("rg").args(&args).current_dir(cwd).output() {
            if h.status.success() {
                let stdout = String::from_utf8_lossy(&h.stdout);
                let files: Vec<&str> = stdout
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .take(12)
                    .collect();
                if !files.is_empty() {
                    chunks.push(format!(
                        "symbols {} appear in: {}",
                        syms.join(", "),
                        files.join(", ")
                    ));
                }
            }
        }
    }
    if let Ok(inv) = std::fs::read_to_string(
        worktree_repo_root(cwd).join("docs/architecture/dual-implementation-inventory.md"),
    ) {
        let rows: Vec<&str> = inv
            .lines()
            .filter(|ln| syms.iter().any(|s| ln.contains(s.as_str())))
            .take(10)
            .collect();
        if !rows.is_empty() {
            chunks.push(format!("inventory rows:\n{}", rows.join("\n")));
        }
    }
    truncate_chars(&chunks.join("\n"), 4000)
}

/// The LAST `VERDICT:` line wins; `unknown` and unparseable are both `None`.
fn parse_verdict(text: &str) -> (Option<String>, String) {
    let re = Regex::new(r"(?im)^VERDICT:\s*(pass|fail|unknown)\s*$").unwrap();
    match re.captures_iter(text).last() {
        None => (None, truncate_chars(text.trim(), 500)),
        Some(cap) => {
            let verdict = cap[1].to_lowercase();
            let start = cap.get(0).unwrap().start();
            let reason = truncate_chars(text[..start].trim(), 500);
            (
                if verdict == "unknown" {
                    None
                } else {
                    Some(verdict)
                },
                reason,
            )
        }
    }
}

/// Collapse whitespace runs so a quote wrapped differently still matches.
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True when at least one `EVIDENCE: "<quote>"` line of 12+ chars quotes the
/// bundle (whitespace-run-insensitive). The plan is in the prompt but not in
/// the bundle, so quoting the plan back does not count.
fn verify_evidence(reply: &str, bundle: &str) -> bool {
    let re = Regex::new(r#"(?im)^\s*EVIDENCE:\s*"([^"]+)"\s*$"#).unwrap();
    let squashed = squash(bundle);
    let quotes: Vec<String> = re
        .captures_iter(reply)
        .map(|cap| cap[1].to_string())
        .collect();
    quotes
        .iter()
        .any(|quote| quote.chars().count() >= 12 && squashed.contains(&squash(quote)))
}

/// One `fno agents spawn --substrate headless` round-trip. Matches the
/// retired Python `_default_spawn`: never a bare `claude -p`.
fn default_spawn(
    name: &str,
    prompt: &str,
    cwd: &Path,
    timeout_secs: u64,
    model: &str,
) -> Result<(i32, String, String), String> {
    let out = Command::new("fno")
        .args(reader_argv(name, prompt, cwd, timeout_secs, model))
        .output()
        .map_err(|e| e.to_string())?;
    Ok((
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// The reader's argv, split from `default_spawn` so a test can read it. The
/// flags after the `--` fence arrive as harness_args (not prompt words), so
/// the reader loads no settings, MCP servers, tools or chrome and answers
/// inside the bound instead of running out the full session load.
fn reader_argv(
    name: &str,
    prompt: &str,
    cwd: &Path,
    timeout_secs: u64,
    model: &str,
) -> Vec<String> {
    [
        "agents",
        "spawn",
        "--name",
        name,
        prompt,
        "--harness",
        "claude",
        "--substrate",
        "headless",
        "--model",
        model,
        "--cwd",
        &cwd.display().to_string(),
        "--timeout",
        &timeout_secs.to_string(),
        "--",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--tools",
        "",
        "--disable-slash-commands",
        "--no-chrome",
        "--no-session-persistence",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

type Spawn<'a> = &'a dyn Fn(&str, &str, &Path, u64, &str) -> Result<(i32, String, String), String>;

fn judge_plan(
    plan_text: &str,
    node_text: &str,
    dimension: &str,
    cwd: &Path,
    lenses: &Lenses,
    bundle: Option<&str>,
    spawn: Spawn,
) -> (Option<String>, String) {
    if !JUDGE_DIMENSIONS.contains(&dimension) {
        return (None, format!("unknown judge dimension {dimension:?}"));
    }
    let (preamble, sections) = lenses;
    let Some(section) = sections.get(dimension).filter(|s| !s.is_empty()) else {
        // Grading a question the reader was never given is the silent
        // failure the lens move exists to end: name the gap, spawn nobody.
        return (None, format!("no lens file {LENS_SUBDIR}/{dimension}.md"));
    };
    let node_trimmed = node_text.trim();
    let mut parts = vec![
        if preamble.is_empty() {
            "Grade one question. Quote the plan lines you rely on, reason briefly, end with VERDICT: pass, fail or unknown.".to_string()
        } else {
            preamble.clone()
        },
        format!(
            "## The node\n{}",
            if node_trimmed.is_empty() {
                "(none supplied)"
            } else {
                node_trimmed
            }
        ),
        format!("## The plan\n{plan_text}"),
    ];
    let ctx = gather_context(dimension, plan_text, cwd);
    if !ctx.is_empty() {
        parts.push(format!("## Context from code\n{ctx}"));
    }
    if SOURCE_DIMENSIONS.contains(&dimension) {
        let Some(bundle) = bundle.filter(|b| !b.trim().is_empty()) else {
            return (None, format!("no {dimension} source"));
        };
        parts.push(format!("## Source: {dimension}\n{bundle}"));
    }
    parts.push(format!("## Your question: {dimension}\n{section}"));
    let prompt = format!("{}\n", parts.join("\n\n"));
    match spawn(
        &format!("blueprint-judge-{dimension}"),
        &prompt,
        cwd,
        READER_TIMEOUT_SECS,
        JUDGE_MODEL,
    ) {
        Err(e) => (
            None,
            truncate_chars(&format!("judge spawn fault: {e}"), 500),
        ),
        Ok((rc, out, err)) if rc != 0 => {
            let msg = if err.trim().is_empty() {
                out.trim()
            } else {
                err.trim()
            };
            (
                None,
                truncate_chars(
                    &format!("judge spawn rc={rc}: {}", truncate_chars(msg, 200)),
                    500,
                ),
            )
        }
        Ok((_, out, _)) => {
            let (verdict, reason) = parse_verdict(&out);
            // A source reader told to argue against the plan can invent the
            // line it graded. A fail that quotes nothing real from the source
            // is not evidence: it stays a gap, never a fabricated verdict.
            if verdict.as_deref() == Some("fail") && SOURCE_DIMENSIONS.contains(&dimension) {
                let verified = bundle.is_some_and(|b| verify_evidence(&out, b));
                if !verified {
                    return (
                        None,
                        truncate_chars(&format!("unverified evidence: {reason}"), 500),
                    );
                }
            }
            (verdict, reason)
        }
    }
}

fn node_text_of(node_id: Option<&str>, entries: &[Value]) -> String {
    let Some(id) = node_id else {
        return String::new();
    };
    let Some(entry) = find_entry(entries, id) else {
        return String::new();
    };
    let title = s_str(entry, "title").unwrap_or("");
    let details = s_str(entry, "details").unwrap_or("");
    [title, details]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn entry_line(entry: &Value) -> String {
    format!(
        "{} {} {}",
        s_str(entry, "id").unwrap_or("?"),
        s_str(entry, "status").unwrap_or("?"),
        s_str(entry, "title").unwrap_or("")
    )
    .trim_end()
    .to_string()
}

/// Read a file that may be absolute or repo-root-relative, capped.
fn readable_under(repo_root: &Path, path: &str, cap: usize) -> Option<String> {
    let p = Path::new(path);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else {
        repo_root.join(p)
    };
    std::fs::read_to_string(resolved)
        .ok()
        .map(|t| truncate_chars(&t, cap))
}

/// The one outside source a source reader grades against, or None when the
/// source is absent: a coverage gap at zero cost, never a fabricated
/// verdict. `overrides` carries a calibration row's `sources:` text
/// verbatim, pinning the world a control's reader must quote; code_truth
/// always reads disk. The graph is read once per run and handed in.
fn source_bundle(
    dimension: &str,
    node_id: Option<&str>,
    entries: &[Value],
    plan_text: &str,
    cwd: &Path,
    overrides: Option<&serde_json::Map<String, Value>>,
) -> Option<String> {
    if let Some(text) = overrides
        .and_then(|m| m.get(dimension))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        return Some(text.to_string());
    }
    match dimension {
        "epic_fit" => epic_bundle(node_id, entries, cwd),
        "mission_fit" => mission_bundle(node_id, entries, cwd),
        "customer_fit" => customer_bundle(cwd),
        "code_truth" => code_bundle(plan_text, cwd),
        _ => None,
    }
}

/// The parent epic: id, status, title, details, its plan text, then one
/// `<id> <status> <title>` line per sibling. None when there is no parent.
fn epic_bundle(node_id: Option<&str>, entries: &[Value], cwd: &Path) -> Option<String> {
    let id = node_id?;
    let parent_id = s_str(find_entry(entries, id)?, "parent")?.to_string();
    let parent = find_entry(entries, &parent_id)?;
    let mut out = entry_line(parent);
    if let Some(details) = s_str(parent, "details").filter(|d| !d.is_empty()) {
        out.push('\n');
        out.push_str(details);
    }
    if let Some(plan_rel) = s_str(parent, "plan_path").filter(|p| !p.is_empty()) {
        if let Some(text) = readable_under(&worktree_repo_root(cwd), plan_rel, 16_000) {
            out.push_str("\n\n## Epic plan\n");
            out.push_str(&text);
        }
    }
    let mut siblings: Vec<String> = entries
        .iter()
        .filter(|e| s_str(e, "parent") == Some(parent_id.as_str()) && s_str(e, "id") != Some(id))
        .map(entry_line)
        .collect();
    siblings.truncate(80);
    if !siblings.is_empty() {
        out.push_str("\n\n## Siblings\n");
        out.push_str(&siblings.join("\n"));
    }
    Some(truncate_chars(&out, 32_000))
}

/// Walk `parent` up to three hops collecting each `vision_path` file's text,
/// then the `[project] vision` config value. None when there is neither.
fn mission_bundle(node_id: Option<&str>, entries: &[Value], cwd: &Path) -> Option<String> {
    let mut texts: Vec<String> = Vec::new();
    if let Some(mut current) = node_id.and_then(|id| find_entry(entries, id)) {
        for _ in 0..3 {
            let Some(parent_id) = s_str(current, "parent").map(str::to_string) else {
                break;
            };
            let Some(parent) = find_entry(entries, &parent_id) else {
                break;
            };
            if let Some(vp) = s_str(parent, "vision_path").filter(|v| !v.is_empty()) {
                if let Some(text) = readable_under(&worktree_repo_root(cwd), vp, 24_000) {
                    texts.push(text);
                }
            }
            current = parent;
        }
    }
    let mut out = texts.join("\n\n");
    if let Some(vision) = crate::agents_config::config_lookup(cwd, &["project", "vision"])
        .and_then(|t| t.as_str().map(str::to_string))
        .filter(|v| !v.is_empty())
    {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&vision);
    }
    (!out.is_empty()).then(|| truncate_chars(&out, 32_000))
}

/// The first readable PRODUCT.md at the repo root, `.agents/context/` or
/// `docs/`. None when the project names no customer.
fn customer_bundle(cwd: &Path) -> Option<String> {
    let root = worktree_repo_root(cwd);
    for rel in [
        "PRODUCT.md",
        ".agents/context/PRODUCT.md",
        "docs/PRODUCT.md",
    ] {
        if let Ok(text) = std::fs::read_to_string(root.join(rel)) {
            return Some(format!("{rel}\n\n{}", truncate_chars(&text, 16_000)));
        }
    }
    None
}

/// Every backticked `path:N` or `path:N-M` citation in the plan, resolved
/// under the repo root: lines N-2 to M+2 with line numbers, `MISSING` for
/// an absent file, `OUT OF RANGE` past the end. None when it cites no line.
fn code_bundle(plan_text: &str, cwd: &Path) -> Option<String> {
    let re = Regex::new(r"`([^`\n]+):(\d+)(?:-(\d+))?`").unwrap();
    let root = worktree_repo_root(cwd);
    let mut out = String::new();
    let mut count = 0usize;
    for cap in re.captures_iter(plan_text) {
        if count >= 40 {
            break;
        }
        let cited = cap[1].to_string();
        let start: usize = match cap[2].parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let end: usize = cap
            .get(3)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(start);
        out.push_str(&resolve_citation(&root, &cited, start, end));
        count += 1;
    }
    (count > 0).then(|| truncate_chars(&out, 24_000))
}

/// Lines N-2 to M+2 of the cited file with line numbers, `MISSING <path>`
/// for an absent file, `OUT OF RANGE <path>:N (K lines)` past the end.
fn resolve_citation(root: &Path, cited: &str, start: usize, end: usize) -> String {
    let p = Path::new(cited);
    let path = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return format!("MISSING {cited}\n");
    };
    let count = text.lines().count();
    if count == 0 || start > count {
        return format!("OUT OF RANGE {cited}:{start} ({count} lines)\n");
    }
    let lo = start.saturating_sub(2).max(1);
    let hi = (end + 2).min(count).max(lo);
    let mut out = format!("{cited}:\n");
    for (i, line) in text.lines().enumerate().skip(lo - 1).take(hi - lo + 1) {
        out.push_str(&format!("{}\t{}\n", i + 1, line));
    }
    out
}

/// One row per judge dimension, each carrying the reader's wall clock. The
/// graph is read once here; each source reader gets the bundle its
/// dimension names.
fn judge_rows(
    plan_text: &str,
    node_id: Option<&str>,
    cwd: &Path,
    lenses: &Lenses,
    spawn: Spawn,
) -> Vec<Value> {
    let entries: Vec<Value> = read_defaulted(&default_graph_path(), true).unwrap_or_default();
    let node_text = node_text_of(node_id, &entries);
    JUDGE_DIMENSIONS
        .iter()
        .map(|dimension| {
            let started = std::time::Instant::now();
            let bundle = if SOURCE_DIMENSIONS.contains(dimension) {
                source_bundle(dimension, node_id, &entries, plan_text, cwd, None)
            } else {
                None
            };
            let (verdict, reason) = judge_plan(
                plan_text,
                &node_text,
                dimension,
                cwd,
                lenses,
                bundle.as_deref(),
                spawn,
            );
            json!({
                "dimension": dimension,
                "verdict": verdict,
                "reason": reason,
                "secs": started.elapsed().as_secs(),
            })
        })
        .collect()
}

fn run_single_plan(
    plan_path: &Path,
    node_id: Option<&str>,
    cwd: &Path,
    lenses: &Lenses,
    spawn: Spawn,
) -> i32 {
    let plan_text = match std::fs::read_to_string(plan_path) {
        Ok(t) => t,
        Err(e) => {
            println!(
                "{}",
                json!({"error": format!("no plan at {}: {e}", plan_path.display())})
            );
            return 0;
        }
    };
    let rows = judge_rows(&plan_text, node_id, cwd, lenses, spawn);
    println!("{}", json!({"rows": rows}));
    0
}

fn run_calibration(
    labels_path: &Path,
    split: &str,
    cwd: &Path,
    lenses: &Lenses,
    spawn: Spawn,
) -> i32 {
    let text = match std::fs::read_to_string(labels_path) {
        Ok(t) => t,
        Err(e) => {
            println!(
                "{}",
                json!({"error": format!("cannot read {}: {e}", labels_path.display())})
            );
            return 1;
        }
    };
    let rows: Vec<Value> = match serde_yaml_ng::from_str(&text) {
        Ok(Value::Array(v)) => v,
        _ => {
            println!(
                "{}",
                json!({"error": "labels.yaml did not parse to a list"})
            );
            return 1;
        }
    };
    let base = labels_path.parent().unwrap_or(Path::new("."));

    // (n, n_fail_labeled, n_pass_labeled, tp, tn) per dimension. tp/tn count
    // correct judge verdicts on the rows actually labeled that class, so the
    // rate below is a per-class recall, not a fraction of every row.
    let mut dims: HashMap<String, (u32, u32, u32, u32, u32)> = HashMap::new();
    let mut disagreements: Vec<Value> = Vec::new();
    let mut controls_wrong = 0u32;
    let entries: Vec<Value> = read_defaulted(&default_graph_path(), true).unwrap_or_default();

    for row in &rows {
        let row_split = row.get("split").and_then(Value::as_str).unwrap_or("dev");
        if row_split != split {
            continue;
        }
        let Some(plan_rel) = row.get("plan").and_then(Value::as_str) else {
            continue;
        };
        let plan_text = std::fs::read_to_string(base.join(plan_rel)).unwrap_or_default();
        let node_text = row
            .get("node_text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let control = row.get("control").and_then(Value::as_bool).unwrap_or(false);
        let Some(labels) = row.get("labels").and_then(Value::as_object) else {
            continue;
        };
        for (dimension, label_v) in labels {
            if !JUDGE_DIMENSIONS.contains(&dimension.as_str()) {
                continue; // an unknown dimension key is a labels.yaml finding enum, not a score
            }
            let Some(label) = label_v.as_str() else {
                continue;
            };
            let bundle = if SOURCE_DIMENSIONS.contains(&dimension.as_str()) {
                source_bundle(
                    dimension,
                    None,
                    &entries,
                    &plan_text,
                    cwd,
                    row.get("sources").and_then(Value::as_object),
                )
            } else {
                None
            };
            let (verdict, reason) = judge_plan(
                &plan_text,
                &node_text,
                dimension,
                cwd,
                &lenses,
                bundle.as_deref(),
                spawn,
            );
            let s = dims.entry(dimension.clone()).or_insert((0, 0, 0, 0, 0));
            s.0 += 1;
            if label == "fail" {
                s.1 += 1;
                if verdict.as_deref() == Some("fail") {
                    s.3 += 1;
                }
            }
            if label == "pass" {
                s.2 += 1;
                if verdict.as_deref() == Some("pass") {
                    s.4 += 1;
                }
            }
            // A None verdict is a judge fault or an unparseable answer, not a
            // scored disagreement (matching the Python side's "gap, never a
            // fabricated verdict" rule) -- it never spends a control.
            if let Some(v) = verdict.as_deref() {
                if v != label {
                    disagreements.push(json!({
                        "plan": plan_rel, "dimension": dimension, "label": label,
                        "judge": verdict, "reason": truncate_chars(&reason, 120),
                    }));
                    if control {
                        controls_wrong += 1;
                    }
                }
            }
        }
    }

    let dimensions: serde_json::Map<String, Value> = dims
        .into_iter()
        .map(|(d, (n, n_fail, n_pass, tp, tn))| {
            let rate = |k: u32, of: u32| {
                if of > 0 {
                    Some((k as f64 / of as f64 * 1000.0).round() / 1000.0)
                } else {
                    None
                }
            };
            (
                d,
                json!({"n": n, "tp_rate": rate(tp, n_fail), "tn_rate": rate(tn, n_pass)}),
            )
        })
        .collect();
    println!(
        "{}",
        json!({"dimensions": dimensions, "disagreements": disagreements, "controls_wrong": controls_wrong})
    );
    if controls_wrong > 0 {
        1
    } else {
        0
    }
}

pub fn run_judge(args: &[String]) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut plan: Option<PathBuf> = None;
    let mut node: Option<String> = None;
    let mut labels: Option<PathBuf> = None;
    let mut split = "dev".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--plan" => {
                i += 1;
                plan = args.get(i).map(PathBuf::from);
            }
            "--node" => {
                i += 1;
                node = args.get(i).cloned();
            }
            "--labels" => {
                i += 1;
                labels = args.get(i).map(PathBuf::from);
            }
            "--split" => {
                i += 1;
                split = args.get(i).cloned().unwrap_or_else(|| "dev".to_string());
            }
            "--force" | "-F" => {
                // Accepted, ignored: the level=report gate is a Python-side
                // config read (fno.loops.loop_level) and already ran before
                // this verb was invoked.
            }
            other => {
                eprintln!("fno-agents judge: unknown flag {other}");
                return 2;
            }
        }
        i += 1;
    }

    let lenses = load_lenses(
        lens_dir(
            &worktree_repo_root(&cwd),
            crate::provider::plugin_root().as_deref(),
        )
        .as_deref(),
    );
    if let Some(labels_path) = labels {
        return run_calibration(&labels_path, &split, &cwd, &lenses, &default_spawn);
    }
    let Some(plan_path) = plan else {
        eprintln!("fno-agents judge: give --plan or --labels");
        return 2;
    };
    run_single_plan(&plan_path, node.as_deref(), &cwd, &lenses, &default_spawn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_verdict_takes_the_last_line_and_caps_reason() {
        let (v, reason) = parse_verdict("some reasoning\nVERDICT: fail\nnoise ignored");
        assert_eq!(v.as_deref(), Some("fail"));
        assert_eq!(reason, "some reasoning");
    }

    #[test]
    fn parse_verdict_unknown_is_none() {
        let (v, _) = parse_verdict("VERDICT: unknown");
        assert_eq!(v, None);
    }

    #[test]
    fn parse_verdict_unparseable_is_none() {
        let (v, reason) = parse_verdict("no verdict line here");
        assert_eq!(v, None);
        assert_eq!(reason, "no verdict line here");
    }

    #[test]
    fn load_lenses_reads_the_directory_shape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("preamble.md"), "shared preamble\n").unwrap();
        std::fs::write(dir.path().join("persona.md"), "# persona\npersona body\n").unwrap();
        std::fs::write(
            dir.path().join("surface_fit.md"),
            "# surface_fit\nfit body\n",
        )
        .unwrap();
        let (preamble, sections) = load_lenses(Some(dir.path()));
        assert_eq!(preamble, "shared preamble");
        assert_eq!(
            sections.get("persona").map(String::as_str),
            Some("persona body")
        );
        assert_eq!(
            sections.get("surface_fit").map(String::as_str),
            Some("fit body")
        );
    }

    #[test]
    fn load_lenses_missing_dir_is_empty() {
        let (preamble, sections) = load_lenses(None);
        assert_eq!(preamble, "");
        assert!(sections.is_empty());
        let (preamble, sections) = load_lenses(Some(Path::new("/nonexistent/lenses")));
        assert_eq!(preamble, "");
        assert!(sections.is_empty());
    }

    #[test]
    fn lens_dir_falls_back_to_the_plugin_root() {
        let repo = tempfile::tempdir().unwrap();
        let plugin = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(plugin.path().join(LENS_SUBDIR)).unwrap();
        assert!(lens_dir(repo.path(), None).is_none());
        let fallback = lens_dir(repo.path(), Some(plugin.path())).unwrap();
        assert_eq!(fallback, plugin.path().join(LENS_SUBDIR));
        // the checkout's own copy wins over the deployed one
        std::fs::create_dir_all(repo.path().join(LENS_SUBDIR)).unwrap();
        assert_eq!(
            lens_dir(repo.path(), Some(plugin.path())).unwrap(),
            repo.path().join(LENS_SUBDIR)
        );
    }

    #[test]
    fn missing_lens_file_is_a_gap_with_no_spawn() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("preamble.md"), "shared").unwrap();
        std::fs::write(dir.path().join("persona.md"), "# persona\nbody\n").unwrap();
        let lenses = load_lenses(Some(dir.path()));
        let calls = std::cell::Cell::new(0u32);
        let spawn = |_name: &str, _: &str, _: &Path, _: u64, _: &str| {
            calls.set(calls.get() + 1);
            Ok((0, "VERDICT: pass".to_string(), String::new()))
        };
        let (verdict, reason) =
            judge_plan("plan", "", "deletable", dir.path(), &lenses, None, &spawn);
        assert_eq!(calls.get(), 0, "no lens, no spawn");
        assert_eq!(verdict, None);
        assert_eq!(
            reason,
            "no lens file skills/pm-plan-review/lenses/deletable.md"
        );
        let (verdict, _) = judge_plan("plan", "", "persona", dir.path(), &lenses, None, &spawn);
        assert_eq!(verdict.as_deref(), Some("pass"));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn run_judge_refuses_an_unknown_flag() {
        assert_eq!(run_judge(&["--bogus".to_string()]), 2);
    }

    #[test]
    fn run_judge_refuses_with_neither_plan_nor_labels() {
        assert_eq!(run_judge(&[]), 2);
    }

    /// Always returns `VERDICT: pass` for persona, `VERDICT: fail` for
    /// everything else -- exercises the tp_rate/tn_rate/disagreement math
    /// without a real model spawn.
    fn fake_spawn(
        name: &str,
        _: &str,
        _: &Path,
        _: u64,
        _: &str,
    ) -> Result<(i32, String, String), String> {
        let out = if name.ends_with("persona") {
            "VERDICT: pass"
        } else {
            "VERDICT: fail"
        };
        Ok((0, out.to_string(), String::new()))
    }

    #[test]
    fn run_single_plan_reports_all_five_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("p.md");
        std::fs::write(&plan, "a plan").unwrap();
        let lenses = load_lenses(None);
        let rc = run_single_plan(&plan, None, dir.path(), &lenses, &fake_spawn);
        assert_eq!(rc, 0);
    }

    #[test]
    fn reader_argv_carries_the_substrate_bound_and_lean_flags() {
        let argv = reader_argv("bp-persona", "grade this", Path::new("/w"), 240, "sonnet");
        let s: Vec<&str> = argv.iter().map(String::as_str).collect();
        let sub = s.iter().position(|a| *a == "--substrate").unwrap();
        assert_eq!(s[sub + 1], "headless");
        let t = s.iter().position(|a| *a == "--timeout").unwrap();
        assert_eq!(s[t + 1], "240");
        // the prompt rides before the fence, as the message
        let fence = s.iter().position(|a| *a == "--").unwrap();
        assert!(s.contains(&"grade this"));
        assert!(s.iter().position(|a| *a == "grade this").unwrap() < fence);
        // after the fence: the lean flags, keeping the empty values
        assert_eq!(
            &s[fence + 1..],
            &[
                "--setting-sources",
                "",
                "--strict-mcp-config",
                "--tools",
                "",
                "--disable-slash-commands",
                "--no-chrome",
                "--no-session-persistence",
            ]
        );
    }

    #[test]
    fn one_faulting_dimension_is_a_gap_and_the_rest_still_run() {
        let dir = tempfile::tempdir().unwrap();
        let plan_text = "a plan".to_string();
        let spawn = |name: &str, _: &str, _: &Path, _: u64, _: &str| {
            if name.ends_with("persona") {
                Err("spawn went sideways".to_string())
            } else {
                Ok((0, "VERDICT: pass".to_string(), String::new()))
            }
        };
        let lenses = (
            String::new(),
            HashMap::from([
                ("persona".to_string(), "who is hit".to_string()),
                ("duplication".to_string(), "existing module".to_string()),
            ]),
        );
        let rows = judge_rows(&plan_text, None, dir.path(), &lenses, &spawn);
        let persona = rows
            .iter()
            .find(|r| r["dimension"] == "persona")
            .expect("persona row");
        assert!(persona["verdict"].is_null());
        assert!(persona["reason"]
            .as_str()
            .unwrap()
            .contains("spawn went sideways"));
        let duplication = rows
            .iter()
            .find(|r| r["dimension"] == "duplication")
            .expect("duplication row");
        assert_eq!(duplication["verdict"], "pass");
        for r in &rows {
            assert!(r["secs"].is_u64(), "every row carries secs: {r}");
        }
    }

    #[test]
    fn tally_math_counts_rates_and_control_disagreements() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.md"), "a plan").unwrap();
        let labels_path = dir.path().join("labels.yaml");
        std::fs::write(
            &labels_path,
            "- plan: p.md\n  split: test\n  control: true\n  labels: {persona: pass, surface_fit: pass}\n",
        )
        .unwrap();

        // fake_spawn always answers persona=pass, surface_fit=fail: the
        // persona row agrees with its label, the surface_fit row disagrees
        // and (being a control) counts against controls_wrong. Lenses are
        // built with sections here: a missing lens is a gap that never
        // disagrees, so an empty map would score nothing.
        let lenses = (
            String::new(),
            HashMap::from([
                ("persona".to_string(), "who is hit".to_string()),
                ("surface_fit".to_string(), "surface".to_string()),
            ]),
        );
        let rc = run_calibration(&labels_path, "test", dir.path(), &lenses, &fake_spawn);
        assert_eq!(rc, 1);
    }

    #[test]
    fn a_missing_source_spawns_nothing_and_names_the_gap() {
        let dir = tempfile::tempdir().unwrap();
        let lenses = (
            String::new(),
            HashMap::from([("epic_fit".to_string(), "argue from the source".to_string())]),
        );
        let calls = std::cell::Cell::new(0u32);
        let spawn = |_name: &str, _: &str, _: &Path, _: u64, _: &str| {
            calls.set(calls.get() + 1);
            Ok((0, "VERDICT: fail".to_string(), String::new()))
        };
        let (verdict, reason) =
            judge_plan("a plan", "", "epic_fit", dir.path(), &lenses, None, &spawn);
        assert_eq!(calls.get(), 0, "no source, no spawn");
        assert_eq!(verdict, None);
        assert_eq!(reason, "no epic_fit source");
    }

    #[test]
    fn a_fail_that_quotes_the_bundle_stands() {
        let dir = tempfile::tempdir().unwrap();
        let lenses = (
            String::new(),
            HashMap::from([("epic_fit".to_string(), "argue from the source".to_string())]),
        );
        let bundle = "x-7868 done Board queue for undriven PRs";
        let reply = "the epic already covers this\nEVIDENCE: \"Board queue for undriven PRs\"\nVERDICT: fail";
        let spawn = move |_name: &str, _: &str, _: &Path, _: u64, _: &str| {
            Ok((0, reply.to_string(), String::new()))
        };
        let (verdict, reason) = judge_plan(
            "a plan",
            "",
            "epic_fit",
            dir.path(),
            &lenses,
            Some(bundle),
            &spawn,
        );
        assert_eq!(verdict.as_deref(), Some("fail"));
        assert!(reason.contains("the epic already covers this"));
    }

    #[test]
    fn a_fail_quoting_invented_text_is_not_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let lenses = (
            String::new(),
            HashMap::from([("epic_fit".to_string(), "argue from the source".to_string())]),
        );
        let bundle = "x-7868 done Board queue for undriven PRs";
        let reply = "made up\nEVIDENCE: \"a wholly invented line that proves it\"\nVERDICT: fail";
        let spawn = move |_name: &str, _: &str, _: &Path, _: u64, _: &str| {
            Ok((0, reply.to_string(), String::new()))
        };
        let (verdict, reason) = judge_plan(
            "a plan",
            "",
            "epic_fit",
            dir.path(),
            &lenses,
            Some(bundle),
            &spawn,
        );
        assert_eq!(verdict, None);
        assert!(reason.starts_with("unverified evidence: "));
    }

    #[test]
    fn code_truth_resolves_and_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n").unwrap();
        let plan = format!("cites `a.rs:5` and `ghost.rs:3` and `a.rs:99`\n");
        let bundle = code_bundle(&plan, dir.path()).expect("citations resolved");
        assert!(bundle.contains("4\tl4"), "context line above: {bundle}");
        assert!(bundle.contains("5\tl5"));
        assert!(bundle.contains("6\tl6"));
        assert!(bundle.contains("MISSING ghost.rs"), "{bundle}");
        assert!(
            bundle.contains("OUT OF RANGE a.rs:99 (8 lines)"),
            "{bundle}"
        );
    }

    #[test]
    fn epic_fit_lists_siblings_from_a_fixture_graph() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            json!({"id": "x-e1", "status": "live", "title": "Epic", "details": "outcome A"}),
            json!({"id": "x-a", "parent": "x-e1", "status": "ready", "title": "Node A"}),
            json!({"id": "x-b", "parent": "x-e1", "status": "done", "title": "Node B"}),
            json!({"id": "x-c", "parent": "x-other", "status": "ready", "title": "Node C"}),
        ];
        let bundle = epic_bundle(Some("x-a"), &entries, dir.path()).expect("parent exists");
        assert!(bundle.contains("x-e1 live Epic"), "{bundle}");
        assert!(bundle.contains("outcome A"), "{bundle}");
        assert!(bundle.contains("## Siblings"), "{bundle}");
        assert!(bundle.contains("x-b done Node B"), "{bundle}");
        assert!(!bundle.contains("Node A"), "self excluded: {bundle}");
        assert!(
            !bundle.contains("Node C"),
            "other-parent excluded: {bundle}"
        );
        // a node without a parent has no epic source
        assert_eq!(epic_bundle(Some("x-e1"), &entries, dir.path()), None);
    }

    #[test]
    fn mission_fit_reads_project_vision_from_a_local_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".fno");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("config.toml"),
            "[project]\nvision = \"Keep one monorepo and one brand.\"\n",
        )
        .unwrap();
        let bundle =
            mission_bundle(None, &[], dir.path()).expect("config vision counts as a source");
        assert!(
            bundle.contains("Keep one monorepo and one brand."),
            "{bundle}"
        );
    }
}
