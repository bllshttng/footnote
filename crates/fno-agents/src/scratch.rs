//! Scratch-shape sweep (x-caf8): notice a recurring job-tmp script shape and
//! file ONE p1 node for it.
//!
//! Agents author throwaway scripts under `<jobs>/<job>/tmp/` to answer
//! questions the fno verbs answer badly. The census in
//! `internal/fno/briefs/20260908-scratch-script-shapes-findings.md` split
//! 4422 py/sh files by git blob hash (2675 are exact repo copies) and named
//! recurring shapes over the 1727 authored ones. This module walks the jobs
//! dir, classifies authored scratch with the brief's ordered rule table,
//! folds recurrences as (job, shape) pairs into the global events journal,
//! and files at most one node per run once a shape crosses the
//! jobs-in-window threshold. Reads the census brief before changing the rule
//! table: the table is content rules, first hit wins, and the ordering IS
//! the taxonomy.
//!
//! Anti-silence: state words on stdout, one per line (`ok`, `filed:<id>`,
//! `folded:<id>`, `suppressed:<id>`, `would-file:<shape>`,
//! `insufficient: <reason>`). A sweep whose positive control (the shipped
//! fixture specimens) classifies to nothing emits nothing and says
//! `insufficient: control failed`, so a broken classifier can never read as
//! a quiet machine.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use regex::Regex;
use serde_json::json;

/// Compile-once helper for module-static regexes. Must precede its uses
/// (macro_rules resolves in source order).
macro_rules! regex {
    ($pat:expr) => {{
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new($pat).expect("static regex"))
    }};
}

/// At most one NEW node per run: the operator's rule is that a long list of
/// new nodes is the failure. The rest wait for tomorrow's sweep.
pub const MAX_FILES_PER_SWEEP: usize = 1;
pub const DEFAULT_THRESHOLD: usize = 3;
pub const DEFAULT_WINDOW_DAYS: i64 = 28;

const BRIEF_PATH: &str = "internal/fno/briefs/20260908-scratch-script-shapes-findings.md";

// ---------------------------------------------------------------------------
// The ordered rule table (census brief section 6, first hit wins)
// ---------------------------------------------------------------------------

struct Rule {
    name: &'static str,
    /// Any one of these matching classifies as the shape.
    any: &'static [&'static str],
    /// All of these matching classifies as the shape (rule 1 and rule 9 need
    /// both halves).
    all: &'static [&'static str],
}

macro_rules! rule {
    ($name:literal, [$($any:literal),* $(,)?] $(, [$($all:literal),* $(,)?])?) => {
        Rule { name: $name, any: &[$($any),*], all: &[$($($all),*)?] }
    };
}

const RULES: &[Rule] = &[
    // 1. Long text through a positional arg: a heredoc / read -d marker AND
    //    an fno verb that takes the long body. Both halves required.
    rule!("longtext_arg", [], [
        "read -r -d ''|<<-?\\s*['\"]?[A-Za-z_]|<<-?\\s*\\w",
        "agents mail (send|reply)|backlog (note|idea|update|encounter|add)|inbox (law|decide|outstanding|operator ack)|agents (spawn|ask)|mux pane send",
    ]),
    // 2. Wrap an fno verb to parse or bound its output.
    rule!("fno_wrap_json", [
        "fno \\S+[^\\n]*\\|\\s*(python3?|jq)\\b",
        "subprocess\\.run\\(\\s*\\[\\s*['\"]fno['\"]",
        "index\\('\\{'\\)|find\\('\\{'\\)",
        "json\\.loads\\((out|raw|txt|r\\.stdout)\\b",
    ]),
    // 3. CI state for a commit, main, or PR.
    rule!("ci_probe", [
        "check-runs|check-suites|statusCheckRollup|gh pr checks|gh run (view|list|rerun|watch)|actions/runs|actions/jobs|fno do pr (wait|status|logs)",
    ]),
    // 4. PR review threads: list, reply, resolve.
    rule!("pr_threads", [
        "reviewThreads|resolveReviewThread|pulls/\\d+/comments|pulls/\\d+/reviews|gh pr review|PRRT_|gh api graphql|gh pr comment",
    ]),
    // 5. Wait for the spawn gate, then spawn.
    rule!("gate_wait", [
        "\\buptime\\b|getloadavg|loadavg|sysctl .*load|spawn\\.?gate|load gate|refusing to spawn",
    ]),
    // 6. Is this worker alive.
    rule!("liveness", [
        "getmtime|stat -f|ps (aux|-o|-p|-ef|ax|-A|-eo)|pgrep|kill -0|os\\.kill|lsof|claude agents --json|registry\\.json|registry-json|/claims/|agents/registry|fno agents (top|list|court|status|reap|rm|stop)",
    ]),
    // 7. Transcript search and tail.
    rule!("transcript", [
        "\\.claude/projects|\\.codex/sessions|\\.claude/sessions|transcript|last_assistant|tool-results",
    ]),
    // 8. Events journal query.
    rule!("events", [
        "events\\.jsonl|doctor event|review_attestation|attestation",
    ]),
    // 9. Programmatic source patch: a write together with a text mutation,
    //    either order.
    rule!("file_patch", [], [
        "write_text|\\.write\\(|open\\([^)]*['\"]w['\"]\\)",
        "\\.replace\\(|re\\.sub|\\.sub\\(|splitlines|\\.index\\(|\\.insert\\(",
    ]),
    // 10. Git in a worktree.
    rule!("git_wrap", [
        "\\bgit (commit|push|rebase|add|status|diff|fetch|checkout|merge|log|show|worktree|rev-parse|ls-files|stash|reset|branch|cherry-pick)\\b",
    ]),
    // 11. Test or lint runner with an honest exit code.
    rule!("test_lint", [
        "pytest|cargo (test|build|clippy|check)|mypy|ruff|fno test|fno doctor (test|lint|bundle)|shellcheck|scripts/ci/|check-file-budget|lint style",
    ]),
    // 12. PR state for several PRs.
    rule!("pr_state", [
        "gh pr (view|list|merge|create|ready|diff)|fno do pr (info|merge|create|rebase)|pulls/\\d+|mergeable|gh pr status|--body-file",
    ]),
    // 13. Graph read: subtree walk or batch status.
    rule!("graph", [
        "graph\\.json|fno backlog (get|find|ready|next|board|rank|contain|maintain|groom|reconcile|decisions|demand)|fno inbox board|_kanban_column",
    ]),
    // 14. Import fno internals to probe one function.
    rule!("fno_internal", [
        "(?m)^\\s*(from fno\\.|import fno)\\b",
    ]),
    // 15. Vault doc append.
    rule!("vault_doc", [
        "c3po/internal|/handoffs/|/briefs/|/memory/",
    ]),
    // 16. Config probe.
    rule!("config", [
        "fno config|config\\.toml|settings\\.json|tomllib|schema",
    ]),
];

fn rules_compiled() -> &'static Vec<(String, Vec<Regex>, Vec<Regex>)> {
    static COMPILED: OnceLock<Vec<(String, Vec<Regex>, Vec<Regex>)>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        RULES
            .iter()
            .map(|r| {
                let compile = |pats: &[&str]| -> Vec<Regex> {
                    pats.iter()
                        .filter_map(|p| Regex::new(&format!("(?i){p}")).ok())
                        .collect()
                };
                (r.name.to_string(), compile(r.any), compile(r.all))
            })
            .collect()
    })
}

/// The first-hit shape for a scratch file's content, or None (residual:
/// genuinely one-off, never filed).
pub fn classify(content: &str) -> Option<&'static str> {
    let compiled = rules_compiled();
    for (i, (_, any, all)) in compiled.iter().enumerate() {
        let hit = if !any.is_empty() {
            any.iter().any(|re| re.is_match(content))
        } else {
            !all.is_empty() && all.iter().all(|re| re.is_match(content))
        };
        if hit {
            return Some(RULES[i].name);
        }
    }
    None
}

/// Cross-cutting attributes riding beside whatever shape the file is:
/// (monitor, oneoff_probe). Monitors are the reign-mandated emit-on-change
/// loops; oneoff probes announce themselves in their first comment.
pub fn attributes(content: &str) -> (bool, bool) {
    let monitor = {
        let loop_re = regex!(r"(?im)^[^\n]*?(while true|while :|until \[|for i in \$\(seq)");
        match loop_re.find(content) {
            Some(m) => {
                let rest = &content[m.end()..];
                regex!(r"(?i)\bsleep\b").is_match(rest)
            }
            None => false,
        }
    };
    let oneoff = {
        let head: String = content.lines().take(15).collect::<Vec<_>>().join("\n");
        regex!(r"(?i)\b(probe|control|repro|prove|canary|falsify|smoke|differential)\b")
            .is_match(&head)
    };
    (monitor, oneoff)
}

// ---------------------------------------------------------------------------
// Fingerprint: sha256 of the token stream with literals/numbers/paths blanked,
// so byte-near copies in different jobs fold to one recurrence.
// ---------------------------------------------------------------------------

pub fn fingerprint(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let strings = regex!(r#""[^"\n]*"|'[^'\n]*'"#);
    let paths = regex!(r"/[A-Za-z0-9_.\-/]+");
    let numbers = regex!(r"\b\d+(\.\d+)?\b");
    let ws = regex!(r"\s+");
    let blanked = strings.replace_all(content, "S");
    let blanked = paths.replace_all(&blanked, "P");
    let blanked = numbers.replace_all(&blanked, "N");
    let normalized = ws.replace_all(blanked.trim(), " ");
    let mut h = Sha256::new();
    h.update(normalized.as_bytes());
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// Verb hints: the census brief section 4 remedy per shape, verified against
// the live CLI surface on 2026-09-11. A node that says which verb and which
// flag is a blueprint input; a node that says "agents write scripts" is noise.
// ---------------------------------------------------------------------------

/// (shape, title, hint, difficulty). Difficulty: low = a flag/lint on an
/// existing verb, medium = a new verb or none.
const VERB_HINTS: &[(&str, &str, &str, &str)] = &[
    ("longtext_arg", "Long text through a positional arg",
     "flags missing on existing verbs: --body-file on mail send/reply, --details-file on backlog idea/update/note, --prompt-file on spawn/ask (rank 3)", "low"),
    ("fno_wrap_json", "Wrap an fno verb to parse its output",
     "JSON-stdout contract: every -J verb prints exactly one document, [] on empty; documented envelope keys; --timeout on mail send/ask (rank 1)", "low"),
    ("ci_probe", "CI state for a commit, main, or PR",
     "no verb today: fno do ci status <sha|main|pr> --watch is the asked remedy; do pr status is PR-keyed and carries no per-check names (rank 4)", "medium"),
    ("pr_threads", "PR review threads: list, reply, resolve",
     "no verb today: fno do pr threads list|reply|resolve (rank 11)", "medium"),
    ("gate_wait", "Wait for the spawn gate, then spawn",
     "existing (hidden): fno agents gate-status -J already prints the gate reading; still missing: spawn --wait polling the gate probe (rank 8)", "low"),
    ("liveness", "Is this worker alive",
     "no verb today: fno agents liveness <handle> -J with transcript mtime + pid and a self-row positive control (rank 2)", "medium"),
    ("transcript", "Transcript search and tail",
     "existing: fno agents list -J carries observed_model; still missing: peek --grep across transcripts (rank 9)", "low"),
    ("events", "Events journal query",
     "flag: --last / field-exclude on doctor event find; fno review prior-head as the named verb the review skill calls (rank 7)", "low"),
    ("file_patch", "Programmatic source patch",
     "none: harness bypass-mode policy, not an fno cause", "medium"),
    ("git_wrap", "Git in a worktree",
     "none: worktree Bash isolation; doc remedy in worktree-mechanics", "medium"),
    ("test_lint", "Test or lint runner with an honest exit",
     "flag: fno doctor test --log <path> that writes EXIT=<rc> itself, unflattened (rank 5)", "low"),
    ("pr_state", "PR state for several PRs",
     "existing: fno do pr info and fno do pr list already answer this (cause two)", "low"),
    ("graph", "Graph read: subtree walk or batch status",
     "no verb today: fno backlog tree <id> -J [--status !done] (rank 10)", "medium"),
    ("fno_internal", "Import fno internals to probe one function",
     "none: one-off probe targeting code under change", "medium"),
    ("vault_doc", "Vault doc append", "none: vault writes", "medium"),
    ("config", "Config probe", "none: one-off", "medium"),
    ("copy_for_diff", "Copy a file at a revision to compare sides",
     "no verb today: fno do show <rev>:<path> and fno do diff <rev-a> <rev-b> -- <path>, worktree-Bash safe (rank 12)", "medium"),
];

fn verb_hint(shape: &str) -> (&'static str, &'static str, &'static str) {
    VERB_HINTS
        .iter()
        .find(|(s, ..)| *s == shape)
        .map(|(_, t, h, d)| (*h, *d, *t))
        .unwrap_or((
            "no verb: unknown recurring shape; see the census brief",
            "medium",
            "Recurring scratch shape",
        ))
}

/// Shapes whose remedy names a leaf HIDDEN from the curated menu. Before
/// filing, the sweep re-probes `fno help <group> --all` (which lists hidden
/// leaves; `fno <group> --help` does not), so a node never claims a verb is
/// missing when it ships hidden - two of the census brief's four flag ranks
/// were already built when the detector was written (2026-09-11 correction
/// from the x-0e24 plan). If the probe cannot confirm the leaf, the fallback
/// line below is filed instead and the difficulty rises to medium.
const HIDDEN_PROBES: &[(&str, &str, &str, &str)] = &[
    (
        "gate_wait",
        "agents",
        "gate-status",
        "no verb today: fno agents spawn --wait <duration> polling the gate's own probe; the gate trigger value is not exposed (rank 8)",
    ),
];

fn hidden_probe(shape: &str) -> Option<(&'static str, &'static str, &'static str)> {
    HIDDEN_PROBES
        .iter()
        .find(|(s, ..)| *s == shape)
        .map(|(_, g, l, fb)| (*g, *l, *fb))
}

/// True when the leaf shows up in the group's full (hidden-inclusive) help.
fn hidden_verb_present(
    fno: &mut dyn FnMut(&[&str]) -> Result<String, String>,
    group: &str,
    leaf: &str,
) -> bool {
    match fno(&["help", group, "--all"]) {
        Ok(out) => {
            // Built per call, not via the module macro: the pattern names a
            // probe argument, so a shared call-site static would pin the
            // first leaf ever probed here.
            let re = Regex::new(&format!("(?im)^\\s*{}\\s", regex::escape(leaf))).ok();
            re.is_some_and(|re| re.is_match(&out))
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Jobs-dir walker + copied-source filter
// ---------------------------------------------------------------------------

pub struct CandidateFile {
    pub path: PathBuf,
    pub job_id: String,
    /// Directly under `<job>/tmp/` (a flat exact copy is shape 18's evidence;
    /// a nested one is checkout residue).
    pub flat: bool,
    pub lines: usize,
    pub mtime: SystemTime,
}

fn skip_dir_name(name: &str) -> bool {
    matches!(
        name,
        "node_modules" | ".venv" | "site-packages" | "vendor" | "target"
    )
}

fn is_checkout_dir(dir: &Path) -> bool {
    dir.join("__init__.py").exists()
        || dir.join(".git").exists()
        || dir.join("pyproject.toml").exists()
}

/// Walk `<jobs>/<job>/tmp/**` for `.py`/`.sh` files with mtime inside the
/// window.
pub fn walk_jobs(jobs_dir: &Path, window_days: i64, now: SystemTime) -> Vec<CandidateFile> {
    let mut out = Vec::new();
    let window = Duration::from_secs((window_days.max(0) as u64).saturating_mul(86_400));
    let Ok(job_entries) = std::fs::read_dir(jobs_dir) else {
        return out;
    };
    for job in job_entries.flatten() {
        let job_path = job.path();
        if !job_path.is_dir() {
            continue;
        }
        let job_id = job.file_name().to_string_lossy().into_owned();
        let tmp = job_path.join("tmp");
        let mut stack = vec![(tmp, 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            if !dir.is_dir() || is_checkout_dir(&dir) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if path.is_dir() {
                    if !skip_dir_name(&name) {
                        stack.push((path, depth + 1));
                    }
                    continue;
                }
                if !matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("py") | Some("sh")
                ) {
                    continue;
                }
                let Ok(meta) = entry.metadata() else { continue };
                let Ok(mtime) = meta.modified() else { continue };
                if now.duration_since(mtime).unwrap_or(Duration::ZERO) > window {
                    continue;
                }
                let lines = std::fs::read_to_string(&path)
                    .map(|t| t.lines().count())
                    .unwrap_or(0);
                out.push(CandidateFile {
                    path,
                    job_id: job_id.clone(),
                    flat: depth == 0,
                    lines,
                    mtime,
                });
            }
        }
    }
    out
}

/// Every blob sha1 in the repo's object store (one call, ~1s at 48k blobs).
/// Empty on any git trouble: the copied-source filter then lets everything
/// through as authored, which misfiles at most the flat-copy slice of shape
/// 18 and never invents a recurrence.
pub fn repo_blob_set(repo_root: &Path) -> HashSet<String> {
    let Ok(out) = std::process::Command::new("git")
        .args(["cat-file", "--batch-all-objects", "--batch-check"])
        .current_dir(repo_root)
        .output()
    else {
        return HashSet::new();
    };
    if !out.status.success() {
        return HashSet::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let sha = parts.next()?;
            let kind = parts.next()?;
            (kind == "blob").then(|| sha.to_string())
        })
        .collect()
}

/// Blob sha1 per candidate path (`git hash-object --stdin-paths`), keyed by
/// path. Absent entries = git refused; the caller treats them as authored.
pub fn blob_hashes(repo_root: &Path, paths: &[&Path]) -> HashMap<PathBuf, String> {
    let mut map = HashMap::new();
    if paths.is_empty() {
        return map;
    }
    let Ok(mut child) = std::process::Command::new("git")
        .args(["hash-object", "--stdin-paths"])
        .current_dir(repo_root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
    else {
        return map;
    };
    if let Some(stdin) = child.stdin.as_mut() {
        for p in paths {
            let _ = writeln!(stdin, "{}", p.display());
        }
    }
    let Ok(out) = child.wait_with_output() else {
        return map;
    };
    if !out.status.success() {
        return map;
    }
    let shas: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    for (p, sha) in paths.iter().zip(shas) {
        map.insert((*p).to_path_buf(), sha);
    }
    map
}

// ---------------------------------------------------------------------------
// Journal + graph reads (the dedupe index)
// ---------------------------------------------------------------------------

/// Tolerant per-line parse of a JSONL events journal.
fn read_events(path: &Path) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

fn event_ts(e: &serde_json::Value) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(e.get("ts")?.as_str()?).ok()
}

fn observed_pairs(
    events: &[serde_json::Value],
    cut: chrono::DateTime<chrono::Utc>,
) -> HashSet<(String, String)> {
    let mut set = HashSet::new();
    for e in events {
        if e.get("type").and_then(|v| v.as_str()) != Some("scratch_shape_observed") {
            continue;
        }
        let Some(ts) = event_ts(e) else { continue };
        if ts.with_timezone(&chrono::Utc) < cut {
            continue;
        }
        let data = e.get("data");
        let shape = data.and_then(|d| d.get("shape")).and_then(|v| v.as_str());
        let job = data.and_then(|d| d.get("job_id")).and_then(|v| v.as_str());
        if let (Some(s), Some(j)) = (shape, job) {
            set.insert((s.to_string(), j.to_string()));
        }
    }
    set
}

struct FiledRow {
    node_id: String,
    ts: chrono::DateTime<chrono::FixedOffset>,
    #[allow(dead_code)]
    outcome: String,
}

/// Newest `scratch_shape_filed` row per shape.
fn newest_filed(events: &[serde_json::Value]) -> HashMap<String, FiledRow> {
    let mut map: HashMap<String, FiledRow> = HashMap::new();
    for e in events {
        if e.get("type").and_then(|v| v.as_str()) != Some("scratch_shape_filed") {
            continue;
        }
        let Some(ts) = event_ts(e) else { continue };
        let Some(data) = e.get("data") else { continue };
        let (Some(shape), Some(node)) = (
            data.get("shape").and_then(|v| v.as_str()),
            data.get("node_id").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let row = FiledRow {
            node_id: node.to_string(),
            ts,
            outcome: data
                .get("outcome")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        };
        match map.get(shape) {
            Some(existing) if existing.ts >= ts => {}
            _ => {
                map.insert(shape.to_string(), row);
            }
        }
    }
    map
}

/// id -> (status, completed_at) for every node in graph.json. Read-only; the
/// sweep never mutates the graph (node birth goes through `fno backlog idea`,
/// the one seam crossing).
fn node_statuses(graph: &Path) -> HashMap<String, (String, Option<String>)> {
    let mut map = HashMap::new();
    let Ok(text) = std::fs::read_to_string(graph) else {
        return map;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return map;
    };
    let entries = v.get("entries").unwrap_or(&v);
    let list: Vec<&serde_json::Value> = match entries {
        serde_json::Value::Array(items) => items.iter().collect(),
        serde_json::Value::Object(obj) => obj.values().collect(),
        _ => return map,
    };
    for node in list {
        let (Some(id), Some(status)) = (
            node.get("id").and_then(|v| v.as_str()),
            node.get("status").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        map.insert(
            id.to_string(),
            (
                status.to_string(),
                node.get("completed_at")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            ),
        );
    }
    map
}

fn is_live_status(status: &str) -> bool {
    // Unknown statuses fold too: filing a duplicate is the worse failure.
    !matches!(status, "done" | "superseded")
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

pub struct SweepPaths {
    pub jobs_dir: PathBuf,
    pub journal: PathBuf,
    pub graph: PathBuf,
    /// Root the git blob-set calls run at (canonical checkout for the daily
    /// stage; any worktree of the repo shares the object store).
    pub repo_root: PathBuf,
    pub fixtures_dir: Option<PathBuf>,
}

pub struct SweepOpts {
    pub threshold: usize,
    pub window_days: i64,
    pub dry_run: bool,
}

struct Pair {
    shape: &'static str,
    job_id: String,
    files: usize,
    monitors: usize,
    first_mtime: SystemTime,
    last_mtime: SystemTime,
    /// (mtime, path) for every file in the pair; truncated to the three
    /// newest when the filed node's details are built.
    specimens: Vec<(SystemTime, String)>,
    fingerprint: String,
}

/// One positive control before anything else: classify the shipped fixture
/// specimens with an empty blob set. Every shape fixture must return its
/// directory's name; the monitor fixture must carry the monitor attribute. A
/// classifier that finds nothing here is broken, and a broken classifier
/// must never read as a quiet machine.
fn run_control(fixtures: Option<&Path>) -> Result<(), String> {
    let Some(dir) = fixtures else {
        return Err("control failed (no fixtures dir resolved; set FNO_AGENTS_FIXTURES or run from the repo)".to_string());
    };
    let mut checked = 0usize;
    let entries = std::fs::read_dir(dir).map_err(|e| {
        format!(
            "control failed (fixtures not readable at {}: {e})",
            dir.display()
        )
    })?;
    for shape_dir in entries.flatten() {
        let shape_name = shape_dir.file_name().to_string_lossy().into_owned();
        let Ok(files) = std::fs::read_dir(shape_dir.path()) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            if !path.is_file() {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            if shape_name == "monitor" {
                let (monitor, _) = attributes(&content);
                if !monitor {
                    return Err(format!(
                        "control failed ({} lost the monitor attribute)",
                        path.display()
                    ));
                }
                checked += 1;
                continue;
            }
            if shape_name == "copy_for_diff" {
                // Blob classification is the walker's job with a real blob
                // set; with the control's empty set the rule table may
                // legitimately return None. Presence is the marker here.
                checked += 1;
                continue;
            }
            match classify(&content) {
                Some(hit) if hit == shape_name => checked += 1,
                other => {
                    return Err(format!(
                        "control failed ({} classified {other:?}, dir says {shape_name})",
                        path.display()
                    ));
                }
            }
        }
    }
    if checked == 0 {
        return Err("control failed (no fixture specimens found)".to_string());
    }
    Ok(())
}

fn rfc3339(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The sweep proper. Returns state lines for stdout (the caller prints
/// them); `emit` is skipped under `--dry-run`. `fno` is the one seam
/// crossing (node birth + the hidden-leaf probe), injectable for tests.
pub fn run_sweep(
    paths: &SweepPaths,
    opts: &SweepOpts,
    emit: Option<&crate::events::EventEmitter>,
    fno: &mut dyn FnMut(&[&str]) -> Result<String, String>,
) -> Vec<String> {
    let mut lines = Vec::new();
    if let Err(reason) = run_control(paths.fixtures_dir.as_deref()) {
        lines.push(format!("insufficient: {reason}"));
        return lines;
    }

    let now = SystemTime::now();
    let files = walk_jobs(&paths.jobs_dir, opts.window_days, now);
    if files.is_empty() {
        lines.push("insufficient: 0 candidates".to_string());
        return lines;
    }

    // Copied-source filter, content not name: a blob-hit flat file is shape
    // 18's evidence; a blob-hit nested file is checkout residue.
    let blob_set = repo_blob_set(&paths.repo_root);
    let path_refs: Vec<&Path> = files.iter().map(|f| f.path.as_path()).collect();
    let hashes = blob_hashes(&paths.repo_root, &path_refs);
    let mut pairs: Vec<Pair> = Vec::new();
    for f in &files {
        let is_copy = hashes
            .get(&f.path)
            .is_some_and(|sha| blob_set.contains(sha));
        let shape = if is_copy {
            if f.flat {
                "copy_for_diff"
            } else {
                continue;
            }
        } else {
            match std::fs::read_to_string(&f.path) {
                Ok(content) => match classify(&content) {
                    Some(shape) => shape,
                    None => continue,
                },
                Err(_) => continue,
            }
        };
        let (monitor, _) = std::fs::read_to_string(&f.path)
            .map(|c| attributes(&c))
            .unwrap_or((false, false));
        if let Some(pair) = pairs
            .iter_mut()
            .find(|p| p.shape == shape && p.job_id == f.job_id)
        {
            pair.files += 1;
            pair.monitors += monitor as usize;
            pair.specimens.push((f.mtime, f.path.display().to_string()));
            pair.first_mtime = pair.first_mtime.min(f.mtime);
            pair.last_mtime = pair.last_mtime.max(f.mtime);
        } else {
            pairs.push(Pair {
                shape,
                job_id: f.job_id.clone(),
                files: 1,
                monitors: monitor as usize,
                first_mtime: f.mtime,
                last_mtime: f.mtime,
                specimens: vec![(f.mtime, f.path.display().to_string())],
                fingerprint: std::fs::read_to_string(&f.path)
                    .map(|c| fingerprint(&c))
                    .unwrap_or_default(),
            });
        }
    }

    // Fold to (job, shape) pairs, emit the new ones, dedupe on the journal.
    let now_utc = chrono::Utc::now();
    let cut = now_utc - chrono::Duration::days(opts.window_days);
    let events = read_events(&paths.journal);
    let already = observed_pairs(&events, cut);
    let mut new_pairs = Vec::new();
    for pair in pairs {
        if already.contains(&(pair.shape.to_string(), pair.job_id.clone())) {
            continue;
        }
        if let Some(emitter) = emit {
            let _ = emitter.emit(
                "scratch_shape_observed",
                &json!({
                    "shape": pair.shape,
                    "fingerprint": pair.fingerprint,
                    "job_id": pair.job_id,
                    "path": pair.specimens.first().map(|(_, p)| p.clone()).unwrap_or_default(),
                    "lines": pair.files,
                    "verb_hint": verb_hint(pair.shape).0,
                    "first_seen": rfc3339(pair.first_mtime),
                    "last_seen": rfc3339(pair.last_mtime),
                    "monitor": pair.monitors > 0,
                }),
            );
        }
        new_pairs.push(pair);
    }

    // Threshold: distinct jobs per shape in the window; rank by jobs then files.
    let mut by_shape: HashMap<&str, Vec<&Pair>> = HashMap::new();
    for pair in &new_pairs {
        by_shape.entry(pair.shape).or_default().push(pair);
    }
    let mut candidates: Vec<(&str, usize, usize)> = by_shape
        .iter()
        .map(|(shape, ps)| (*shape, ps.len(), ps.iter().map(|p| p.files).sum()))
        .filter(|(_, jobs, _)| *jobs >= opts.threshold)
        .collect();
    candidates.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));

    if candidates.is_empty() {
        if lines.is_empty() {
            lines.push("ok".to_string());
        }
        return lines;
    }

    // Re-read the journal: the observed rows this run just wrote are part of
    // the filed-node resolution window too.
    let events = read_events(&paths.journal);
    let filed = newest_filed(&events);
    let statuses = node_statuses(&paths.graph);
    let mut filed_this_run = 0usize;

    for (shape, jobs, files_n) in &candidates {
        let resolution = match filed.get(*shape) {
            None => Filing::File(None),
            Some(row) => match statuses.get(&row.node_id) {
                Some((status, completed_at)) if !is_live_status(status) => {
                    if filed_within(&row.ts, completed_at.as_deref(), cut) {
                        Filing::Suppress(row.node_id.clone())
                    } else {
                        Filing::File(Some(row.node_id.clone()))
                    }
                }
                Some(_) => Filing::Fold(row.node_id.clone()),
                // A filed row naming a node the graph no longer holds: the
                // graph read is the authority on liveness; nothing to fold
                // onto, so treat as unfiled.
                None => Filing::File(None),
            },
        };
        match resolution {
            Filing::Fold(node) => {
                let (hint, _, title) = verb_hint(shape);
                let details = details_body(shape, *jobs, *files_n, &new_pairs, hint);
                match fno(&[
                    "backlog",
                    "idea",
                    &format!("{title} recurs: {jobs} jobs wrote it"),
                    "--wave-of",
                    &node,
                    "--difficulty",
                    "low",
                    "-d",
                    &details,
                    "-J",
                ]) {
                    Ok(stdout) => {
                        if let Some(id) = receipt_node(&stdout) {
                            emit_filed(emit, shape, &id, "folded", *jobs, *files_n, None);
                            lines.push(format!("folded:{id}"));
                        }
                    }
                    Err(_) => lines.push(format!("insufficient: fold failed for {shape}")),
                }
            }
            Filing::Suppress(node) => {
                lines.push(format!("suppressed:{node}"));
            }
            Filing::File(caused_by) => {
                if filed_this_run >= MAX_FILES_PER_SWEEP {
                    continue; // waits for tomorrow; prints nothing by contract
                }
                let (mut hint, mut difficulty, title) = verb_hint(shape);
                if let Some((group, leaf, fallback)) = hidden_probe(shape) {
                    if hidden_verb_present(fno, group, leaf) {
                        // hint already names the hidden leaf; stays low.
                    } else {
                        hint = fallback;
                        difficulty = "medium";
                    }
                }
                let details = details_body(shape, *jobs, *files_n, &new_pairs, hint);
                let mut argv = vec![
                    "backlog".to_string(),
                    "idea".to_string(),
                    format!("{title} recurs: {jobs} jobs wrote it"),
                    "-p".to_string(),
                    "p1".to_string(),
                    "--source-kind".to_string(),
                    "from_observation".to_string(),
                    "--difficulty".to_string(),
                    difficulty.to_string(),
                    "--domain".to_string(),
                    "code".to_string(),
                    "-d".to_string(),
                    details.clone(),
                ];
                if let Some(old) = &caused_by {
                    argv.push("--caused-by".to_string());
                    argv.push(old.clone());
                }
                argv.push("-J".to_string());
                let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
                match fno(&argv_ref) {
                    Ok(stdout) => {
                        let mut node = receipt_node(&stdout);
                        if node.is_none() {
                            // The dedup net offered a fold choice instead of
                            // minting: fold onto its candidate like the
                            // skill-diff lane does.
                            if let Some(candidate) = receipt_candidate(&stdout) {
                                let fold_title = format!("{title} recurs: {jobs} jobs wrote it");
                                let fold_argv = vec![
                                    "backlog",
                                    "idea",
                                    fold_title.as_str(),
                                    "--wave-of",
                                    candidate.as_str(),
                                    "--difficulty",
                                    "low",
                                    "-d",
                                    details.as_str(),
                                    "-J",
                                ];
                                if let Ok(out2) = fno(&fold_argv) {
                                    node = receipt_node(&out2).or(Some(candidate));
                                } else {
                                    node = Some(candidate);
                                }
                            }
                        }
                        if let Some(id) = node {
                            filed_this_run += 1;
                            emit_filed(
                                emit,
                                shape,
                                &id,
                                "filed",
                                *jobs,
                                *files_n,
                                caused_by.as_deref(),
                            );
                            lines.push(format!("filed:{id}"));
                        } else {
                            lines
                                .push(format!("insufficient: unreadable idea receipt for {shape}"));
                        }
                    }
                    Err(_) => lines.push(format!("insufficient: idea call failed for {shape}")),
                }
            }
        }
    }
    if lines.is_empty() {
        lines.push("ok".to_string());
    }
    lines
}

enum Filing {
    File(Option<String>),
    Fold(String),
    Suppress(String),
}

fn filed_within(
    filed_ts: &chrono::DateTime<chrono::FixedOffset>,
    completed_at: Option<&str>,
    cut: chrono::DateTime<chrono::Utc>,
) -> bool {
    let completed = completed_at
        .and_then(|c| chrono::DateTime::parse_from_rfc3339(c).ok())
        .map(|t| t.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| filed_ts.with_timezone(&chrono::Utc));
    completed >= cut
}

fn details_body(shape: &str, jobs: usize, files: usize, pairs: &[Pair], hint: &str) -> String {
    let specimens: Vec<&Pair> = pairs.iter().filter(|p| p.shape == shape).collect();
    let first = specimens
        .iter()
        .map(|p| rfc3339(p.first_mtime))
        .min()
        .unwrap_or_default();
    let last = specimens
        .iter()
        .map(|p| rfc3339(p.last_mtime))
        .max()
        .unwrap_or_default();
    let mut names: Vec<(SystemTime, String)> =
        specimens.iter().flat_map(|p| p.specimens.clone()).collect();
    names.sort_by(|a, b| b.0.cmp(&a.0));
    let names: Vec<String> = names.into_iter().take(3).map(|(_, p)| p).collect();
    format!(
        "Scratch-shape sweep (census {BRIEF_PATH}, section 6 rule table, first hit wins). \
         rule {shape}: {files} authored files across {jobs} jobs inside the window; first {first}, last {last}. \
         Newest specimens: {names}. \
         Verb hint: {hint}. \
         One recurrence is one (job, shape) pair: files per job measure one agent's habit, distinct jobs measure fleet need.",
        names = names.join(", ")
    )
}

fn emit_filed(
    emit: Option<&crate::events::EventEmitter>,
    shape: &str,
    node_id: &str,
    outcome: &str,
    jobs: usize,
    files: usize,
    caused_by: Option<&str>,
) {
    if let Some(emitter) = emit {
        let mut payload = json!({
            "shape": shape,
            "node_id": node_id,
            "outcome": outcome,
            "jobs": jobs,
            "files": files,
        });
        if let Some(old) = caused_by {
            payload["caused_by"] = json!(old);
        }
        let _ = emitter.emit("scratch_shape_filed", &payload);
    }
}

/// The last JSON object on the idea receipt's stdout (skill_diff's
/// `_idea_receipt` shape): a plain mint carries `id`, a wave carries
/// `outcome: "wave"` + `node_id`.
fn receipt_node(stdout: &str) -> Option<String> {
    let obj = last_json_object(stdout)?;
    if obj.get("outcome").and_then(|v| v.as_str()) == Some("wave") {
        return obj
            .get("node_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| obj.get("id").and_then(|v| v.as_str()).map(str::to_string));
    }
    obj.get("id").and_then(|v| v.as_str()).map(str::to_string)
}

/// The near-duplicate candidate a `choice_required` receipt offers.
fn receipt_candidate(stdout: &str) -> Option<String> {
    let obj = last_json_object(stdout)?;
    if obj.get("outcome").and_then(|v| v.as_str()) != Some("choice_required") {
        return None;
    }
    obj.get("candidates")?
        .get(0)?
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn last_json_object(stdout: &str) -> Option<serde_json::Value> {
    // Reverse scan: the receipt is the LAST stdout block, and an earlier
    // object (or a nested one) can parse from a prefix only when the JSON
    // extends to end-of-string, which the true last block does.
    for (i, _) in stdout.rmatch_indices('{') {
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&stdout[i..]) {
            return Some(obj);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// `fno-agents scratch report`: the ranked human table, read-only
// ---------------------------------------------------------------------------

pub fn run_report(journal: &Path, window_days: i64, json_out: bool) -> String {
    let cut = chrono::Utc::now() - chrono::Duration::days(window_days);
    let events = read_events(journal);
    struct Row {
        files: usize,
        jobs: HashSet<String>,
        monitors: usize,
        first: String,
        last: String,
        filed: Option<String>,
    }
    let mut rows: HashMap<String, Row> = HashMap::new();
    for e in &events {
        let Some(ts) = event_ts(e) else { continue };
        if ts.with_timezone(&chrono::Utc) < cut {
            continue;
        }
        let Some(data) = e.get("data") else { continue };
        match e.get("type").and_then(|v| v.as_str()) {
            Some("scratch_shape_observed") => {
                let (Some(shape), Some(job)) = (
                    data.get("shape").and_then(|v| v.as_str()),
                    data.get("job_id").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                let row = rows.entry(shape.to_string()).or_insert(Row {
                    files: 0,
                    jobs: HashSet::new(),
                    monitors: 0,
                    first: String::new(),
                    last: String::new(),
                    filed: None,
                });
                row.files += 1;
                row.jobs.insert(job.to_string());
                if data.get("monitor").and_then(|v| v.as_bool()) == Some(true) {
                    row.monitors += 1;
                }
                let ts_s = ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                if row.first.is_empty() || ts_s < row.first {
                    row.first = ts_s.clone();
                }
                if ts_s > row.last {
                    row.last = ts_s.clone();
                }
            }
            Some("scratch_shape_filed") => {
                let (Some(shape), Some(node)) = (
                    data.get("shape").and_then(|v| v.as_str()),
                    data.get("node_id").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                if let Some(row) = rows.get_mut(shape) {
                    row.filed = Some(node.to_string());
                }
            }
            _ => {}
        }
    }
    let mut ranked: Vec<(&String, &Row)> = rows.iter().collect();
    ranked.sort_by(|a, b| {
        b.1.jobs
            .len()
            .cmp(&a.1.jobs.len())
            .then(b.1.files.cmp(&a.1.files))
    });
    if json_out {
        let out: Vec<serde_json::Value> = ranked
            .iter()
            .map(|(shape, r)| {
                json!({
                    "shape": shape, "files": r.files, "jobs": r.jobs.len(),
                    "monitors": r.monitors, "first_seen": r.first,
                    "last_seen": r.last, "filed_node": r.filed,
                })
            })
            .collect();
        return format!(
            "{}",
            serde_json::to_string_pretty(&out).unwrap_or_else(|_| "[]".into())
        );
    }
    let mut out = String::from("shape                files jobs monitors first_seen            last_seen             filed\n");
    for (shape, r) in ranked {
        out.push_str(&format!(
            "{shape:<20} {files:>5} {jobs:>4} {monitors:>8} {first:<21} {last:<21} {filed}\n",
            files = r.files,
            jobs = r.jobs.len(),
            monitors = r.monitors,
            first = r.first,
            last = r.last,
            filed = r.filed.as_deref().unwrap_or("-"),
        ));
    }
    if rows.is_empty() {
        out.push_str("insufficient: no scratch_shape_observed rows in the window\n");
    }
    out
}

// ---------------------------------------------------------------------------
// CLI entry (`fno-agents scratch sweep|report`, binary-direct)
// ---------------------------------------------------------------------------

/// Default jobs dir: `$CLAUDE_CONFIG_DIR/jobs`, else `~/.claude/jobs`.
fn default_jobs_dir() -> Option<PathBuf> {
    if let Some(cfg) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(cfg).join("jobs"));
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".claude").join("jobs"))
}

/// The durable state root: `$FNO_AGENTS_HOME`'s parent (that env names the
/// spaces dir itself), else `$HOME/.fno` - byte-identical to
/// `paths::durable_spaces_root`'s derivation.
pub fn fno_state_root() -> PathBuf {
    if let Some(v) = std::env::var_os(crate::paths::HOME_ENV).filter(|v| !v.is_empty()) {
        let home = PathBuf::from(&v);
        return home.parent().map(|p| p.to_path_buf()).unwrap_or(home);
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".fno"))
        .unwrap_or_else(|| PathBuf::from(".fno"))
}

/// `[evals] scratch_threshold` / `scratch_window_days` from the project's
/// `.fno/config.toml`, falling back to the state root's, falling back to the
/// defaults. The Python side (`config._evals.EvalsBlock`) is the sanitizer;
/// this reader is the tolerant mirror.
fn load_evals_config(repo_root: &Path) -> (usize, i64) {
    fn table_at(p: &Path) -> Option<toml::Table> {
        std::fs::read_to_string(p).ok()?.parse::<toml::Table>().ok()
    }
    let table = table_at(&repo_root.join(".fno").join("config.toml"))
        .or_else(|| table_at(&fno_state_root().join("config.toml")))
        .unwrap_or_default();
    let evals = table.get("evals").and_then(|v| v.as_table());
    let threshold = evals
        .and_then(|e| e.get("scratch_threshold"))
        .and_then(|v| v.as_integer())
        .filter(|v| *v > 0)
        .map(|v| v as usize)
        .unwrap_or(DEFAULT_THRESHOLD);
    let window = evals
        .and_then(|e| e.get("scratch_window_days"))
        .and_then(|v| v.as_integer())
        .filter(|v| *v >= 0)
        .unwrap_or(DEFAULT_WINDOW_DAYS);
    (threshold, window)
}

/// `git rev-parse --show-toplevel` from `cwd`, or the cwd itself.
fn repo_root_of_cwd() -> PathBuf {
    if let Ok(out) = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        if out.status.success() {
            let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !top.is_empty() {
                return PathBuf::from(top);
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// Entry for the binary-direct `scratch` verb (client.rs). Never lazy-starts
/// a daemon; reads files and prints state words.
pub fn run_cli(args: &[String]) -> i32 {
    let Some(sub) = args.first() else {
        eprintln!("usage: fno-agents scratch <sweep|report> [--since-days N] [--threshold N] [--jobs-dir <path>] [--dry-run] [--json]");
        return 2;
    };
    let window: i64 = flag_value(args, "--since-days")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_WINDOW_DAYS);
    match sub.as_str() {
        "sweep" => {
            let jobs_dir = flag_value(args, "--jobs-dir")
                .map(PathBuf::from)
                .or_else(default_jobs_dir);
            let Some(jobs_dir) = jobs_dir else {
                println!("insufficient: no jobs dir resolved (set --jobs-dir)");
                return 0;
            };
            let repo_root = repo_root_of_cwd();
            let (cfg_threshold, cfg_window) = load_evals_config(&repo_root);
            let threshold = flag_value(args, "--threshold")
                .and_then(|v| v.parse().ok())
                .unwrap_or(cfg_threshold);
            let window_days = flag_value(args, "--since-days")
                .and_then(|v| v.parse().ok())
                .unwrap_or(cfg_window);
            let fixtures = std::env::var_os("FNO_AGENTS_FIXTURES")
                .map(PathBuf::from)
                .or_else(|| {
                    let p = repo_root.join("crates/fno-agents/tests/fixtures/scratch");
                    p.is_dir().then_some(p)
                });
            let paths = SweepPaths {
                jobs_dir,
                journal: fno_state_root().join("events.jsonl"),
                graph: fno_state_root().join("graph.json"),
                repo_root,
                fixtures_dir: fixtures,
            };
            let opts = SweepOpts {
                threshold,
                window_days,
                dry_run: has_flag(args, "--dry-run"),
            };
            // The one seam crossing: `fno` launches for node birth + the
            // hidden-leaf probe. The caller does not own the decision the
            // answer feeds, which is the kind docs/architecture/rust-python-seam.md
            // names legitimate.
            let mut fno = |argv: &[&str]| -> Result<String, String> {
                let out = std::process::Command::new("fno").args(argv).output();
                match out {
                    Ok(o) if o.status.success() => {
                        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
                    }
                    Ok(o) => Err(format!(
                        "fno {}: {}",
                        argv.first().unwrap_or(&""),
                        String::from_utf8_lossy(&o.stderr).trim()
                    )),
                    Err(e) => Err(format!("fno spawn failed: {e}")),
                }
            };
            let emit = (!opts.dry_run)
                .then(|| crate::events::EventEmitter::new(paths.journal.clone(), "agents"));
            let lines = run_sweep(&paths, &opts, emit.as_ref(), &mut fno);
            for line in &lines {
                println!("{line}");
            }
            0
        }
        "report" => {
            let journal = fno_state_root().join("events.jsonl");
            print!("{}", run_report(&journal, window, has_flag(args, "--json")));
            0
        }
        other => {
            eprintln!("fno-agents scratch: unknown subcommand {other:?} (sweep|report)");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventEmitter;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    fn repo_root() -> Option<PathBuf> {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let top = String::from_utf8(out.stdout).ok()?;
        let top = top.trim();
        (!top.is_empty()).then(|| PathBuf::from(top))
    }

    fn fixtures_dir() -> Option<PathBuf> {
        let dir = repo_root()?.join("crates/fno-agents/tests/fixtures/scratch");
        dir.is_dir().then_some(dir)
    }

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fno-scratch-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }

    fn write(dir: &Path, rel: &str, content: &str) -> PathBuf {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        std::fs::write(&path, content).expect("write");
        path
    }

    /// A fake `fno`: records every argv, answers `help` from
    /// `help_has_gate`, and mints wave/filed receipts.
    #[derive(Clone)]
    struct FakeFno {
        log: Rc<RefCell<Vec<Vec<String>>>>,
        help_has_gate: Rc<Cell<bool>>,
    }
    impl FakeFno {
        fn new() -> Self {
            FakeFno {
                log: Rc::new(RefCell::new(Vec::new())),
                help_has_gate: Rc::new(Cell::new(true)),
            }
        }
        fn runner(&self) -> impl FnMut(&[&str]) -> Result<String, String> + '_ {
            let log = self.log.clone();
            let gate = self.help_has_gate.clone();
            move |argv: &[&str]| -> Result<String, String> {
                log.borrow_mut()
                    .push(argv.iter().map(|s| s.to_string()).collect());
                if argv.first() == Some(&"help") {
                    return Ok(if gate.get() {
                        "fno agents full command surface\n\nCommands:\n  gate-status            Print the spawn gate's read-only capacity reading.\n  spawn                  Spawn a worker.\n".to_string()
                    } else {
                        "fno agents full command surface\n\nCommands:\n  spawn                  Spawn a worker.\n".to_string()
                    });
                }
                if argv.first() == Some(&"backlog") && argv.get(1) == Some(&"idea") {
                    if let Some(i) = argv.iter().position(|a| *a == "--wave-of") {
                        let node = argv.get(i + 1).cloned().unwrap_or_default();
                        return Ok(format!(r#"{{"outcome": "wave", "node_id": "{node}"}}"#));
                    }
                    return Ok(r#"{"id": "x-filed9", "title": "filed"}"#.to_string());
                }
                Err(format!("unexpected fno call: {argv:?}"))
            }
        }
        fn calls(&self) -> Vec<Vec<String>> {
            self.log.borrow().clone()
        }
    }

    fn sweep_paths(tmp: &Path) -> SweepPaths {
        SweepPaths {
            jobs_dir: tmp.join("jobs"),
            journal: tmp.join("events.jsonl"),
            graph: tmp.join("graph.json"),
            repo_root: repo_root().unwrap_or_else(|| tmp.to_path_buf()),
            fixtures_dir: fixtures_dir(),
        }
    }

    fn opts(threshold: usize) -> SweepOpts {
        SweepOpts {
            threshold,
            window_days: 28,
            dry_run: false,
        }
    }

    fn journal_count(journal: &Path, needle: &str) -> usize {
        std::fs::read_to_string(journal)
            .map(|t| t.lines().filter(|l| l.contains(needle)).count())
            .unwrap_or(0)
    }

    fn now_ts() -> String {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    fn filed_row(shape: &str, node: &str, ts: String, outcome: &str) -> serde_json::Value {
        json!({
            "ts": ts,
            "type": "scratch_shape_filed",
            "source": "agents",
            "data": {"shape": shape, "node_id": node, "outcome": outcome},
        })
    }

    fn observed_row(shape: &str, job: &str, ts: String) -> serde_json::Value {
        json!({
            "ts": ts,
            "type": "scratch_shape_observed",
            "source": "agents",
            "data": {"shape": shape, "fingerprint": "ab", "job_id": job, "path": "/j/tmp/a.sh", "lines": 12, "monitor": false},
        })
    }

    fn seed_journal(path: &Path, rows: &[serde_json::Value]) {
        let text: String = rows.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(path, text).unwrap();
    }

    const LONGTEXT: &str = "#!/usr/bin/env bash\nfno agents mail send t-x9 --kind inbox --body <<'BODY'\nlong body line\nBODY\n";
    const GATE_WAIT: &str = "#!/usr/bin/env python3\nimport os, time\n# wait for the load gate, then spawn\nwhile os.getloadavg()[0] > 88:\n    time.sleep(60)\n";
    const CI_PROBE: &str = "#!/usr/bin/env bash\n curl -s \"https://api.github.com/repos/o/r/commits/$SHA/check-runs\" | jq '.check_runs[] | select(.conclusion != \"success\")'\n";
    const LIVENESS: &str = "#!/usr/bin/env python3\nimport os, time\nfresh = time.time() - os.stat('/x/y').st_mtime < 600\nos.kill(1234, 0)\n";
    const TRANSCRIPT: &str = "#!/usr/bin/env python3\nimport glob, os, json\nfor p in glob.glob(os.path.expanduser('~/.claude/projects/*/x.jsonl')):\n    json.loads(open(p).readline())\n";

    #[test]
    fn debug_newest_filed_reads_the_seeded_row() {
        let tmp = temp_root("dbg");
        let journal = tmp.join("events.jsonl");
        std::fs::write(
            &journal,
            format!(
                "{}\n",
                json!({
                    "ts": now_ts(),
                    "type": "scratch_shape_filed",
                    "source": "agents",
                    "data": {"shape": "longtext_arg", "node_id": "x-live1", "outcome": "seeded"},
                })
            ),
        )
        .unwrap();
        eprintln!("RAW: {}", std::fs::read_to_string(&journal).unwrap());
        let events = read_events(&journal);
        eprintln!("PARSED {}/{} lines", events.len(), 1);
        let filed = newest_filed(&events);
        eprintln!("FILED: {:?}", filed.keys().collect::<Vec<_>>());
        assert!(filed.contains_key("longtext_arg"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rule_table_classifies_the_fixture_specimens() {
        let Some(fix) = fixtures_dir() else { return };
        let mut checked = 0;
        for shape_dir in std::fs::read_dir(&fix).unwrap().flatten() {
            let name = shape_dir.file_name().to_string_lossy().into_owned();
            for f in std::fs::read_dir(shape_dir.path()).unwrap().flatten() {
                let content = std::fs::read_to_string(f.path()).unwrap_or_default();
                if name == "monitor" {
                    assert!(
                        attributes(&content).0,
                        "m1-mail fixture lost the monitor attribute"
                    );
                    checked += 1;
                    continue;
                }
                if name == "copy_for_diff" {
                    continue; // blob classification: walker-level test below
                }
                assert_eq!(
                    classify(&content),
                    Some(name.as_str()),
                    "fixture {} classifies off its directory",
                    f.path().display()
                );
                checked += 1;
            }
        }
        assert!(checked >= 10, "only {checked} fixtures checked");
    }

    #[test]
    fn fingerprint_folds_string_and_path_literals() {
        let Some(fix) = fixtures_dir() else { return };
        let content = std::fs::read_to_string(fix.join("events/attest_prov.py")).expect("fixture");
        let twin = content.replace("5a22d327", "deadbeef");
        assert_eq!(fingerprint(&content), fingerprint(&twin));
        assert_ne!(
            fingerprint(&content),
            fingerprint(&format!("{content}\nextra = 1\n"))
        );
    }

    #[test]
    fn walker_reports_a_flat_blob_copy_and_skips_the_nested_one() {
        let Some(root) = repo_root() else { return };
        let tmp = temp_root("blobcopy");
        // A tracked .py file: a flat copy of it is a repo blob; the walker
        // only takes .py/.sh candidates, so the tracked file must be .py.
        let tracked = std::fs::read_to_string(root.join("cli/src/fno/paths.py")).unwrap();
        write(&tmp.join("jobs"), "joba/tmp/paths_copy.py", &tracked);
        // The plan's copied-source fixture: a slice of paths.py whose blob
        // hash is injected into the set.
        let slice = std::fs::read_to_string(
            root.join("crates/fno-agents/tests/fixtures/scratch/copy_for_diff/paths_slice.py"),
        )
        .unwrap();
        let flat_slice = write(&tmp.join("jobs"), "jobb/tmp/paths_slice.py", &slice);
        // The nested copy sits under a checkout dir (__init__.py) and is
        // skipped, not reported.
        write(
            &tmp.join("jobs"),
            "jobc/tmp/checkout/paths_nested.py",
            &tracked,
        );
        write(&tmp.join("jobs"), "jobc/tmp/checkout/__init__.py", "");

        let mut blob_set = repo_blob_set(&root);
        let hashes = blob_hashes(&root, &[&flat_slice]);
        if let Some(sha) = hashes.get(&flat_slice) {
            blob_set.insert(sha.clone());
        }
        assert!(!blob_set.is_empty(), "blob set empty; git unavailable?");

        let files = walk_jobs(&tmp.join("jobs"), 28, SystemTime::now());
        assert_eq!(
            files.len(),
            2,
            "the checkout dir's nested copy is not walked"
        );
        let hashes = blob_hashes(
            &root,
            &files.iter().map(|f| f.path.as_path()).collect::<Vec<_>>(),
        );
        let mut shapes = Vec::new();
        for f in &files {
            let is_copy = hashes.get(&f.path).is_some_and(|s| blob_set.contains(s));
            if is_copy {
                assert!(
                    f.flat,
                    "nested blob copy must have been a checkout skip: {}",
                    f.path.display()
                );
                shapes.push("copy_for_diff");
            } else {
                let content = std::fs::read_to_string(&f.path).unwrap();
                match classify(&content) {
                    Some(s) => shapes.push(s),
                    None => panic!("unclassified candidate {}", f.path.display()),
                }
            }
        }
        assert_eq!(
            shapes.iter().filter(|s| **s == "copy_for_diff").count(),
            2,
            "the flat tracked copy + the injected slice"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sweep_files_once_past_threshold() {
        let tmp = temp_root("files");
        for job in ["j1", "j2", "j3"] {
            write(
                &tmp.join("jobs"),
                &format!("{job}/tmp/mail_{job}.sh"),
                LONGTEXT,
            );
        }
        let paths = sweep_paths(&tmp);
        let fake = FakeFno::new();
        let emit = EventEmitter::new(paths.journal.clone(), "agents");
        let mut runner = fake.runner();
        let lines = run_sweep(&paths, &opts(3), Some(&emit), &mut runner);
        assert_eq!(lines, vec!["filed:x-filed9"], "got {lines:?}");

        let calls = fake.calls();
        let idea: Vec<_> = calls
            .iter()
            .filter(|c| c.get(1).map(String::as_str) == Some("idea"))
            .collect();
        assert_eq!(idea.len(), 1, "one idea call, got {calls:?}");
        let c = idea[0];
        assert_eq!(
            c.iter()
                .position(|a| a == "-p")
                .map(|i| &c[i + 1])
                .map(String::as_str),
            Some("p1")
        );
        assert_eq!(
            c.iter()
                .position(|a| a == "--source-kind")
                .map(|i| &c[i + 1])
                .map(String::as_str),
            Some("from_observation")
        );
        assert_eq!(journal_count(&paths.journal, "scratch_shape_observed"), 3);
        assert_eq!(journal_count(&paths.journal, "scratch_shape_filed"), 1);
        // Idempotent second run the same day: pairs are recorded, nothing re-fires.
        let mut runner2 = fake.runner();
        let lines2 = run_sweep(&paths, &opts(3), Some(&emit), &mut runner2);
        assert_eq!(
            lines2,
            vec!["ok"],
            "second run must be idempotent, got {lines2:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sweep_folds_when_a_live_node_holds_the_shape() {
        let tmp = temp_root("folds");
        for job in ["j1", "j2", "j3"] {
            write(
                &tmp.join("jobs"),
                &format!("{job}/tmp/mail_{job}.sh"),
                LONGTEXT,
            );
        }
        let paths = sweep_paths(&tmp);
        seed_journal(
            &paths.journal,
            &[filed_row("longtext_arg", "x-live1", now_ts(), "seeded")],
        );
        std::fs::write(
            &paths.graph,
            r#"{"entries":[{"id":"x-live1","status":"idea"}]}"#,
        )
        .unwrap();
        let fake = FakeFno::new();
        let emit = EventEmitter::new(paths.journal.clone(), "agents");
        let mut runner = fake.runner();
        let lines = run_sweep(&paths, &opts(3), Some(&emit), &mut runner);
        assert!(lines.iter().any(|l| l == "folded:x-live1"), "got {lines:?}");
        let idea: Vec<_> = fake
            .calls()
            .into_iter()
            .filter(|c| c.get(1).map(String::as_str) == Some("idea"))
            .collect();
        assert!(
            idea.iter().all(|c| !c.contains(&"-p".to_string())),
            "fold carries no -p: {idea:?}"
        );
        assert!(idea
            .iter()
            .any(|c| c.contains(&"--wave-of".to_string()) && c.contains(&"x-live1".to_string())));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sweep_suppresses_a_recently_done_node() {
        let tmp = temp_root("suppress");
        for job in ["j1", "j2", "j3"] {
            write(
                &tmp.join("jobs"),
                &format!("{job}/tmp/mail_{job}.sh"),
                LONGTEXT,
            );
        }
        let paths = sweep_paths(&tmp);
        seed_journal(
            &paths.journal,
            &[filed_row("longtext_arg", "x-done1", now_ts(), "filed")],
        );
        std::fs::write(
            &paths.graph,
            format!(
                "{{\"entries\":[{{\"id\":\"x-done1\",\"status\":\"done\",\"completed_at\":\"{}\"}}]}}",
                now_ts()
            ),
        )
        .unwrap();
        let fake = FakeFno::new();
        let emit = EventEmitter::new(paths.journal.clone(), "agents");
        let mut runner = fake.runner();
        let lines = run_sweep(&paths, &opts(3), Some(&emit), &mut runner);
        assert_eq!(lines, vec!["suppressed:x-done1"], "got {lines:?}");
        assert!(
            fake.calls().is_empty(),
            "suppress must not shell out: {:?}",
            fake.calls()
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sweep_files_anew_when_the_done_node_is_past_the_window() {
        let tmp = temp_root("refile");
        for job in ["j1", "j2", "j3"] {
            write(
                &tmp.join("jobs"),
                &format!("{job}/tmp/mail_{job}.sh"),
                LONGTEXT,
            );
        }
        let paths = sweep_paths(&tmp);
        let old = (chrono::Utc::now() - chrono::Duration::days(60))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        seed_journal(
            &paths.journal,
            &[filed_row("longtext_arg", "x-old1", old.clone(), "filed")],
        );
        std::fs::write(
            &paths.graph,
            format!(
                "{{\"entries\":[{{\"id\":\"x-old1\",\"status\":\"done\",\"completed_at\":\"{}\"}}]}}",
                old
            ),
        )
        .unwrap();
        let fake = FakeFno::new();
        let emit = EventEmitter::new(paths.journal.clone(), "agents");
        let mut runner = fake.runner();
        let lines = run_sweep(&paths, &opts(3), Some(&emit), &mut runner);
        assert_eq!(lines, vec!["filed:x-filed9"], "got {lines:?}");
        let idea = fake
            .calls()
            .into_iter()
            .find(|c| c.get(1).map(String::as_str) == Some("idea"))
            .expect("idea call");
        assert_eq!(
            idea.iter()
                .position(|a| a == "--caused-by")
                .map(|i| idea[i + 1].clone()),
            Some("x-old1".to_string()),
            "refile must carry --caused-by: {idea:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sweep_below_threshold_is_ok_with_observed_rows_only() {
        let tmp = temp_root("below");
        for job in ["j1", "j2"] {
            write(
                &tmp.join("jobs"),
                &format!("{job}/tmp/mail_{job}.sh"),
                LONGTEXT,
            );
        }
        let paths = sweep_paths(&tmp);
        let fake = FakeFno::new();
        let emit = EventEmitter::new(paths.journal.clone(), "agents");
        let mut runner = fake.runner();
        let lines = run_sweep(&paths, &opts(3), Some(&emit), &mut runner);
        assert_eq!(lines, vec!["ok"], "got {lines:?}");
        assert!(fake.calls().is_empty());
        assert_eq!(journal_count(&paths.journal, "scratch_shape_observed"), 2);
        assert_eq!(journal_count(&paths.journal, "scratch_shape_filed"), 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sweep_caps_one_file_per_run_and_rotates() {
        let tmp = temp_root("cap");
        for job in ["j1", "j2", "j3"] {
            write(
                &tmp.join("jobs"),
                &format!("{job}/tmp/mail_{job}.sh"),
                LONGTEXT,
            );
            write(
                &tmp.join("jobs"),
                &format!("{job}/tmp/gate_{job}.py"),
                GATE_WAIT,
            );
            write(
                &tmp.join("jobs"),
                &format!("{job}/tmp/ci_{job}.sh"),
                CI_PROBE,
            );
            write(
                &tmp.join("jobs"),
                &format!("{job}/tmp/live_{job}.py"),
                LIVENESS,
            );
            write(
                &tmp.join("jobs"),
                &format!("{job}/tmp/tr_{job}.py"),
                TRANSCRIPT,
            );
        }
        let paths = sweep_paths(&tmp);
        let fake = FakeFno::new();
        let emit = EventEmitter::new(paths.journal.clone(), "agents");
        let mut runner = fake.runner();
        let lines = run_sweep(&paths, &opts(3), Some(&emit), &mut runner);
        let filed: Vec<_> = lines.iter().filter(|l| l.starts_with("filed:")).collect();
        assert_eq!(filed.len(), 1, "one file per run, got {lines:?}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sweep_zero_candidates_is_insufficient() {
        let tmp = temp_root("empty");
        let paths = sweep_paths(&tmp);
        std::fs::create_dir_all(paths.jobs_dir.join("j1/tmp")).unwrap();
        let fake = FakeFno::new();
        let emit = EventEmitter::new(paths.journal.clone(), "agents");
        let mut runner = fake.runner();
        let lines = run_sweep(&paths, &opts(3), Some(&emit), &mut runner);
        assert_eq!(lines, vec!["insufficient: 0 candidates"], "got {lines:?}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn control_failure_blocks_everything() {
        let tmp = temp_root("nocontrol");
        for job in ["j1", "j2", "j3"] {
            write(
                &tmp.join("jobs"),
                &format!("{job}/tmp/mail_{job}.sh"),
                LONGTEXT,
            );
        }
        let mut paths = sweep_paths(&tmp);
        paths.fixtures_dir = Some(tmp.join("no-such-fixtures"));
        let fake = FakeFno::new();
        let emit = EventEmitter::new(paths.journal.clone(), "agents");
        let mut runner = fake.runner();
        let lines = run_sweep(&paths, &opts(3), Some(&emit), &mut runner);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].starts_with("insufficient: control failed"),
            "got {lines:?}"
        );
        assert!(
            fake.calls().is_empty(),
            "a failed control must emit and call nothing"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn hidden_verb_probe_names_the_verb_instead_of_filing_no_verb() {
        // The 2026-09-11 correction banked as an acceptance case: the remedy
        // leaf ships hidden, `fno help agents --all` lists it (the curated
        // `fno agents --help` does not), so the filed node names the verb at
        // low difficulty rather than a "no verb" at medium.
        let tmp = temp_root("hidden");
        for job in ["j1", "j2", "j3"] {
            write(
                &tmp.join("jobs"),
                &format!("{job}/tmp/wait_{job}.py"),
                GATE_WAIT,
            );
        }
        let paths = sweep_paths(&tmp);
        let fake = FakeFno::new();
        let emit = EventEmitter::new(paths.journal.clone(), "agents");
        let mut runner = fake.runner();
        let lines = run_sweep(&paths, &opts(3), Some(&emit), &mut runner);
        assert_eq!(lines, vec!["filed:x-filed9"], "got {lines:?}");
        let idea = fake
            .calls()
            .into_iter()
            .find(|c| c.get(1).map(String::as_str) == Some("idea"))
            .expect("idea call");
        let details = &idea[idea.iter().position(|a| a == "-d").unwrap() + 1];
        assert!(
            details.contains("gate-status"),
            "hint must name the hidden verb: {details}"
        );
        assert!(
            details.contains("existing (hidden)"),
            "hint must mark it existing: {details}"
        );
        assert_eq!(
            idea.iter()
                .position(|a| a == "--difficulty")
                .map(|i| idea[i + 1].as_str()),
            Some("low")
        );

        // The verb is gone (or the probe cannot confirm): the fallback remedy
        // files at medium, never claiming the hidden verb.
        let tmp2 = temp_root("hidden-gone");
        for job in ["j1", "j2", "j3"] {
            write(
                &tmp2.join("jobs"),
                &format!("{job}/tmp/wait_{job}.py"),
                GATE_WAIT,
            );
        }
        let paths2 = sweep_paths(&tmp2);
        let fake2 = FakeFno::new();
        fake2.help_has_gate.set(false);
        let emit2 = EventEmitter::new(paths2.journal.clone(), "agents");
        let mut runner2 = fake2.runner();
        let lines2 = run_sweep(&paths2, &opts(3), Some(&emit2), &mut runner2);
        assert_eq!(lines2, vec!["filed:x-filed9"], "got {lines2:?}");
        let idea2 = fake2
            .calls()
            .into_iter()
            .find(|c| c.get(1).map(String::as_str) == Some("idea"))
            .expect("idea call");
        let details2 = &idea2[idea2.iter().position(|a| a == "-d").unwrap() + 1];
        assert!(
            !details2.contains("gate-status"),
            "retired leaf must not be claimed: {details2}"
        );
        assert!(
            details2.contains("no verb today"),
            "fallback names the true state: {details2}"
        );
        assert_eq!(
            idea2
                .iter()
                .position(|a| a == "--difficulty")
                .map(|i| idea2[i + 1].as_str()),
            Some("medium")
        );
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&tmp2);
    }

    #[test]
    fn report_renders_the_ranked_table() {
        let tmp = temp_root("report");
        let journal = tmp.join("events.jsonl");
        seed_journal(
            &journal,
            &[
                observed_row("longtext_arg", "j1", now_ts()),
                observed_row("longtext_arg", "j2", now_ts()),
                observed_row("longtext_arg", "j3", now_ts()),
                filed_row("longtext_arg", "x-rep1", now_ts(), "filed"),
            ],
        );
        let table = run_report(&journal, 28, false);
        assert!(table.contains("longtext_arg"), "got {table}");
        assert!(table.contains("x-rep1"), "got {table}");
        let json = run_report(&journal, 28, true);
        assert!(json.contains("\"jobs\": 3"), "got {json}");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
