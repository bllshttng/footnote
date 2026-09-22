//! `fno-agents intel [--days N] [--period 2w|1m|2m|3m|all] [--node <id>]
//! [--session <id>] [--json] [-H|--harness claude,codex,opencode|all]
//! [--project NAME]... [--all-projects]` - the provenance fold: which of
//! this machine's sessions did a person type into?
//!
//! Read-only fold over transcripts, like [`crate::bash_census`]: no daemon,
//! nothing written. Every user-shaped turn is classified by provenance
//! ([`crate::provenance`]), joined to its operator turns, tool calls,
//! commits, node, and PR; every bus row addressed to the session is judged
//! for delivery, reply, and the 80-word contract. No model, no writes: the
//! narrative judgment runs in the invoking session (`/fno:intel`), never
//! here. Default scope is this project including its worktrees; `--project`
//! names others, `--all-projects` reads the machine.

use crate::opencode_transcript::OpencodeSource;
use crate::paths::AgentsHome;
use crate::provenance::{BusIndex, ClaudeSource, CodexSource, Provenance, TranscriptSource};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

const DEFAULT_DAYS: u64 = 30;
const WORD_CONTRACT: usize = 80;

/// The `--period` words the skill presents, mapped to `--days` values.
/// `all` is `--days 0`, the no-window fold.
fn period_days(word: &str) -> Option<u64> {
    match word {
        "2w" => Some(14),
        "1m" => Some(30),
        "2m" => Some(60),
        "3m" => Some(90),
        "all" => Some(0),
        _ => None,
    }
}

/// Per-session report row.
#[derive(Debug, Serialize)]
struct SessionRow {
    harness: &'static str,
    session: String,
    path: String,
    started: Option<String>,
    duration_s: Option<i64>,
    /// `attended` (operator or relay turns present) or `unattended`
    /// (neither; excluded from totals.operator_sessions).
    kind: String,
    /// Every provenance counter, including zeros.
    counters: BTreeMap<&'static str, u64>,
    /// Raw timestamps of each witnessed operator turn: the skill quotes
    /// only these when it judges the session.
    operator_turns: Vec<String>,
    tool_use: usize,
    /// HEAD-sha transitions in the entry's loop_check fingerprints, the
    /// derivation digest.rs uses (events never carry a commit event).
    commits: usize,
    /// Transcript mtime + size: the facet-cache key the skill needs, so a
    /// resumed session re-judges instead of stranding on a stale cache.
    mtime: u64,
    size: u64,
    node: Option<String>,
    pr_number: Option<u64>,
    relay: Vec<RelayFacet>,
}

/// One bus row addressed to this session, judged.
#[derive(Debug, Serialize)]
struct RelayFacet {
    id: String,
    from_session: Option<String>,
    ts: String,
    words: usize,
    delivered: bool,
    answered: bool,
    within_contract: bool,
    control: bool,
    duplicate: bool,
}

/// The per-node mail graph: bus rows between the node's own sessions.
#[derive(Debug, Serialize)]
struct NodeRow {
    node: String,
    sessions: usize,
    /// from > to pair counts over 8-char session prefixes.
    pairs: BTreeMap<String, usize>,
    unanswered: usize,
    median_reply_s: Option<u64>,
    longest_silence_s: Option<u64>,
}

/// The receipt of what this fold read: the selected harnesses, whether every
/// project was in scope, the named projects, and the resolved roots.
#[derive(Debug, Serialize)]
struct Scope {
    harnesses: Vec<&'static str>,
    all_projects: bool,
    projects: Vec<String>,
    roots: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Report {
    days: u64,
    scope: Scope,
    sessions: Vec<SessionRow>,
    nodes: Vec<NodeRow>,
    /// Harnesses whose store could not be read, with the reason.
    skipped: BTreeMap<String, String>,
    totals: Totals,
    /// The operator_submit witness receipt: what the mux saw and bound.
    witness: crate::operator_witness::WitnessReceipt,
}

#[derive(Debug, Default, Serialize)]
struct Totals {
    sessions: usize,
    operator_sessions: usize,
    counters: BTreeMap<&'static str, u64>,
    relay_rows: usize,
    undelivered: usize,
    unanswered: usize,
}

/// Every input the fold reads, resolved once per invocation. Tests construct
/// it directly against tempdirs; `run_intel` resolves from the environment.
struct FoldCtx {
    bus: BusIndex,
    /// session id -> entry group (every id the ledger entry or manifest
    /// binds), node id, PR number.
    join: HashMap<String, (Vec<String>, Option<String>, Option<u64>)>,
    /// Raw events.jsonl rows, for the loop_check commit derivation.
    events: Vec<Value>,
    /// The operator_submit witness rows: what the fold binds turns against.
    witness: crate::operator_witness::SubmitIndex,
    days: u64,
    now: u64,
}

impl FoldCtx {
    /// Commit count for one session's ledger group: HEAD-sha transitions in
    /// the loop_check fingerprints, exactly how digest.rs derives it (events
    /// never carry a commit event). Unreadable or empty events read as zero.
    fn commits_for(&self, group: &[String]) -> usize {
        let group: HashSet<&str> = group.iter().map(String::as_str).collect();
        let mut shas: Vec<String> = Vec::new();
        for v in &self.events {
            let kind = v
                .get("type")
                .and_then(|t| t.as_str())
                .or_else(|| v.get("kind").and_then(|k| k.as_str()));
            if kind != Some("loop_check") {
                continue;
            }
            let field = |key: &str| {
                v.get("data")
                    .and_then(|d| d.get(key))
                    .or_else(|| v.get(key))
            };
            let in_group = field("session_id")
                .and_then(|s| s.as_str())
                .is_some_and(|s| group.contains(s));
            if !in_group {
                continue;
            }
            if let Some(fp) = field("fingerprint").and_then(|f| f.as_str()) {
                if let Some(head) = fp.split('|').next() {
                    if !head.is_empty() && shas.last().map(String::as_str) != Some(head) {
                        shas.push(head.to_string());
                    }
                }
            }
        }
        shas.len().saturating_sub(1)
    }
}

/// session id -> (group, node, pr), from ledger.json entries (their
/// `sessions[]` arrays, keyed per member id) and the worktree manifests
/// (`<fno>/spaces/*/worktrees/*/target-state.md`, whose `harness_session_id`
/// maps to the `input` node). The manifest fills only ids the ledger missed.
fn session_join(fno_dir: &Path) -> HashMap<String, (Vec<String>, Option<String>, Option<u64>)> {
    let mut join = HashMap::new();
    let ledger = fno_dir.join("ledger.json");
    if let Ok(raw) = std::fs::read_to_string(&ledger) {
        if let Ok(doc) = serde_json::from_str::<Value>(&raw) {
            if let Some(entries) = doc.get("entries").and_then(|v| v.as_array()) {
                for entry in entries {
                    let node = entry
                        .get("graph_node_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let pr = entry.get("pr_number").and_then(|v| v.as_u64());
                    let group: Vec<String> = entry
                        .get("sessions")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|s| s.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    for sid in &group {
                        join.entry(sid.clone())
                            .or_insert((group.clone(), node.clone(), pr));
                    }
                    if let Some(sid) = entry.get("session_id").and_then(|v| v.as_str()) {
                        join.entry(sid.to_string())
                            .or_insert((group.clone(), node.clone(), pr));
                    }
                }
            }
        }
    }
    let spaces = fno_dir.join("spaces");
    let Ok(spaces_read) = std::fs::read_dir(&spaces) else {
        return join;
    };
    for space in spaces_read.flatten() {
        let wt_root = space.path().join("worktrees");
        let Ok(wt_read) = std::fs::read_dir(&wt_root) else {
            continue;
        };
        for worktree in wt_read.flatten() {
            let manifest = worktree.path().join("target-state.md");
            let Ok(text) = std::fs::read_to_string(&manifest) else {
                continue;
            };
            let mut session: Option<String> = None;
            let mut input: Option<String> = None;
            for line in text.lines() {
                if let Some(v) = line.strip_prefix("harness_session_id:") {
                    let v = v.trim();
                    if !v.is_empty() && v != "null" {
                        session = Some(v.to_string());
                    }
                } else if let Some(v) = line.strip_prefix("input:") {
                    let v = v.trim().trim_matches('"');
                    if !v.is_empty() && v != "null" {
                        input = Some(v.to_string());
                    }
                }
            }
            if let (Some(sid), Some(node)) = (session, input) {
                let group = vec![sid.clone()];
                join.entry(sid).or_insert_with(|| (group, Some(node), None));
            }
        }
    }
    join
}

/// The bus log path, mirroring the env order every bus reader shares
/// (FNO_BUS_DIR, then FNO_INBOX_ROOT, else `<fno>/bus/messages.jsonl`). The
/// config.paths.bus_dir template override is the gap king_board.rs already
/// documents.
pub(crate) fn bus_log_path(fno_dir: &Path) -> PathBuf {
    if let Some(dir) = std::env::var("FNO_BUS_DIR").ok().filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("messages.jsonl");
    }
    if let Some(root) = std::env::var("FNO_INBOX_ROOT")
        .ok()
        .filter(|v| !v.is_empty())
    {
        return PathBuf::from(root).join(".bus").join("messages.jsonl");
    }
    fno_dir.join("bus").join("messages.jsonl")
}

fn ts_secs(ts: &str) -> Option<u64> {
    crate::state::rfc3339_like_to_secs(ts).or_else(|| {
        ts.get(..19)
            .and_then(|s| crate::state::rfc3339_like_to_secs(&format!("{s}Z")))
    })
}

fn rfc3339_str(ts_epoch: f64) -> Option<String> {
    chrono::DateTime::from_timestamp(ts_epoch as i64, 0)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// The per-session fold: provenance counters, tool calls, node/PR join, and
/// the relay facets of every bus row addressed to this session.
fn fold_session(
    harness: &'static str,
    file: &crate::provenance::SessionFile,
    source: &dyn TranscriptSource,
    ctx: &mut FoldCtx,
) -> SessionRow {
    // One read serves every parser: turns, tool calls, and the relay
    // delivery check all work from this text.
    let raw = source.read(file);
    let turns = source.turns(&raw);
    let mut counters: BTreeMap<&'static str, u64> = Provenance::all_labels()
        .into_iter()
        .map(|l| (l, 0u64))
        .collect();
    let mut first_ts: Option<f64> = None;
    let mut last_ts: Option<f64> = None;
    let mut operator_turns: Vec<String> = Vec::new();
    for turn in &turns {
        if turn.text.trim().is_empty() {
            continue;
        }
        if let Some(ts) = turn.ts_epoch {
            first_ts = Some(first_ts.map_or(ts, |f| f.min(ts)));
            last_ts = Some(last_ts.map_or(ts, |f| f.max(ts)));
        }
        let mut p = crate::provenance::classify_turn(&turn.obj, &ctx.bus, &file.session_id);
        // The witness join: an unshaped turn (or a command the operator ran
        // by hand) binds to the earliest unconsumed operator_submit inside
        // the window. A bound unshaped turn is witnessed; a bound command
        // stays a command but consumes the submit so it can never bind a
        // later machine turn. Relay and keepalive turns never bind; a turn
        // with no timestamp never binds.
        if p == crate::provenance::Provenance::Unknown
            || p == crate::provenance::Provenance::Harness(
                crate::provenance::HarnessKind::CommandInvocation,
            )
        {
            let bound = turn
                .ts_epoch
                .and_then(|ts| ctx.witness.bind(&file.session_id, (ts * 1000.0) as i64))
                .is_some();
            if bound && p == crate::provenance::Provenance::Unknown {
                p = crate::provenance::Provenance::Operator;
            }
        }
        if p == crate::provenance::Provenance::Operator {
            if let Some(ts) = turn.obj.get("timestamp").and_then(|t| t.as_str()) {
                operator_turns.push(ts.to_string());
            }
        }
        *counters.entry(p.label()).or_insert(0) += 1;
    }

    // Relay facets: bus rows addressed to this session, oldest first.
    let mut relay: Vec<RelayFacet> = Vec::new();
    let mut addressed: Vec<&crate::provenance::BusRow> = ctx
        .bus
        .rows()
        .iter()
        .filter(|r| r.to_session.as_deref() == Some(file.session_id.as_str()))
        .collect();
    if !addressed.is_empty() {
        addressed.sort_by(|a, b| a.ts.cmp(&b.ts));
        let mut seen: HashSet<(String, String)> = HashSet::new();
        for row in addressed {
            let delivered = (!row.id.is_empty() && raw.contains(row.id.as_str()))
                || raw.contains(row.body.trim());
            let row_ts = ts_secs(&row.ts);
            let mut answered = false;
            // The reply goes to the sender, so a row whose sender is
            // unknown can never be answered; and only a row addressed to a
            // session (to_session present) can be that reply.
            if row.from_session.is_some() {
                for other in ctx.bus.rows() {
                    let reply = other.from_session.as_deref() == row.to_session.as_deref()
                        && other.to_session.as_deref() == row.from_session.as_deref()
                        && ts_secs(&other.ts).is_some_and(|t2| row_ts.is_none_or(|t1| t2 > t1));
                    if reply {
                        answered = true;
                        break;
                    }
                }
            }
            let key = (
                row.to_session.clone().unwrap_or_default(),
                row.body.trim().to_string(),
            );
            let duplicate = seen.contains(&key);
            seen.insert(key);
            relay.push(RelayFacet {
                id: row.id.clone(),
                from_session: row.from_session.clone(),
                ts: row.ts.clone(),
                words: row.words,
                delivered,
                answered,
                within_contract: row.words <= WORD_CONTRACT,
                control: row
                    .body
                    .lines()
                    .any(|l| l.trim_start().starts_with("control:")),
                duplicate,
            });
        }
    }

    let (group, node, pr) = ctx
        .join
        .get(&file.session_id)
        .cloned()
        .unwrap_or_else(|| (Vec::new(), None, None));
    let operator_count = counters.get("operator").copied().unwrap_or(0);
    let unknown_count = counters.get("unknown").copied().unwrap_or(0);
    let relay_turns: u64 = counters
        .iter()
        .filter(|(k, _)| k.starts_with("relay_"))
        .map(|(_, v)| v)
        .sum();
    SessionRow {
        harness,
        session: file.session_id.clone(),
        path: file.path.display().to_string(),
        started: first_ts.and_then(rfc3339_str),
        duration_s: first_ts.zip(last_ts).map(|(f, l)| (l - f) as i64),
        // Attended now includes unwitnessed sessions: unknown turns mean the
        // session may hold operator speech the fold cannot witness yet, and
        // session totals must not shift.
        kind: if operator_count > 0 || unknown_count > 0 || relay_turns > 0 {
            "attended".to_string()
        } else {
            "unattended".to_string()
        },
        counters,
        operator_turns,
        tool_use: source.tool_uses(&raw),
        commits: ctx.commits_for(&group),
        mtime: file.mtime,
        size: file.size,
        node,
        pr_number: pr,
        relay,
    }
}

/// The pure fold over one source's sessions.
fn fold_source(source: &dyn TranscriptSource, ctx: &mut FoldCtx) -> Vec<SessionRow> {
    let rows: Vec<SessionRow> = source
        .sessions(ctx.days)
        .iter()
        .map(|file| fold_session(source.harness(), file, source, ctx))
        .collect();
    rows
}

/// The per-node mail graph over the rows a node's own sessions exchanged.
fn node_rows(rows: &[SessionRow], bus: &BusIndex, now: u64) -> Vec<NodeRow> {
    let mut by_node: BTreeMap<String, Vec<&SessionRow>> = BTreeMap::new();
    for row in rows {
        if row.kind == "attended" {
            if let Some(node) = &row.node {
                by_node.entry(node.clone()).or_default().push(row);
            }
        }
    }
    let mut out = Vec::new();
    for (node, members) in by_node {
        let ids: HashSet<&str> = members.iter().map(|m| m.session.as_str()).collect();
        let conv: Vec<&crate::provenance::BusRow> = bus
            .rows()
            .iter()
            .filter(|r| {
                r.from_session.as_deref().is_some_and(|f| ids.contains(f))
                    && r.to_session.as_deref().is_some_and(|t| ids.contains(t))
            })
            .collect();
        let mut pairs: BTreeMap<String, usize> = BTreeMap::new();
        let mut unanswered = 0usize;
        let mut latencies: Vec<u64> = Vec::new();
        let mut silences: Vec<u64> = Vec::new();
        for (i, row) in conv.iter().enumerate() {
            let pair = format!("{} > {}", short(&row.from_session), short(&row.to_session));
            *pairs.entry(pair).or_insert(0) += 1;
            let row_ts = ts_secs(&row.ts);
            let reply = conv.iter().skip(i + 1).find(|other| {
                other.from_session.as_deref() == row.to_session.as_deref()
                    && other.to_session.as_deref() == row.from_session.as_deref()
                    && ts_secs(&other.ts).is_some_and(|t2| row_ts.is_none_or(|t1| t2 > t1))
            });
            match reply {
                Some(other) => {
                    if let (Some(t1), Some(t2)) = (row_ts, ts_secs(&other.ts)) {
                        latencies.push(t2.saturating_sub(t1));
                    }
                }
                None => {
                    unanswered += 1;
                    if let Some(t1) = row_ts {
                        silences.push(now.saturating_sub(t1));
                    }
                }
            }
        }
        // Longest silence with no open row: the widest gap between
        // consecutive rows, so a fully answered graph still reports one.
        if silences.is_empty() {
            let mut stamps: Vec<u64> = conv.iter().filter_map(|r| ts_secs(&r.ts)).collect();
            stamps.sort();
            for pair in stamps.windows(2) {
                silences.push(pair[1] - pair[0]);
            }
        }
        latencies.sort();
        let median = match latencies.len() {
            0 => None,
            n => Some(latencies[n / 2]),
        };
        out.push(NodeRow {
            node,
            sessions: members.len(),
            pairs,
            unanswered,
            median_reply_s: median,
            longest_silence_s: silences.iter().max().copied(),
        });
    }
    out.sort_by(|a, b| {
        b.unanswered
            .cmp(&a.unanswered)
            .then_with(|| a.node.cmp(&b.node))
    });
    out
}

fn short(id: &Option<String>) -> &str {
    match id {
        Some(s) => s.get(..8).unwrap_or(s),
        None => "?",
    }
}

fn print_report(report: &Report) {
    println!(
        "intel: {} session(s) over {} day(s), {} attended",
        report.totals.sessions, report.days, report.totals.operator_sessions
    );
    println!(
        "  scope: harness={} project={}",
        report.scope.harnesses.join(","),
        if report.scope.all_projects {
            "all".to_string()
        } else {
            report.scope.projects.join(",")
        }
    );
    println!("  harness     session                              operator  relay  harness  keepalive  unknown  tool_use  commits  node");
    for s in &report.sessions {
        println!(
            "  {:<10}  {:<36}  {:>8}  {:>5}  {:>7}  {:>9}  {:>7}  {:>8}  {:>7}  {}",
            s.harness,
            short_id(&s.session),
            s.counters.get("operator").copied().unwrap_or(0),
            relay_total(s),
            harness_total(s),
            s.counters.get("keepalive").copied().unwrap_or(0),
            s.counters.get("unknown").copied().unwrap_or(0),
            s.tool_use,
            s.commits,
            s.node.as_deref().unwrap_or("-")
        );
    }
    if report.witness.submits == 0 {
        println!("  witness: no operator_submit rows in window; unshaped turns read unknown");
    }
    if !report.nodes.is_empty() {
        println!("  node mail graph:");
        for n in &report.nodes {
            println!(
                "    {}: {} unanswered, median reply {}s, longest silence {}s",
                n.node,
                n.unanswered,
                n.median_reply_s
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".into()),
                n.longest_silence_s
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".into())
            );
        }
    }
    if !report.skipped.is_empty() {
        for (harness, why) in &report.skipped {
            println!("  skipped {harness}: {why}");
        }
    }
}

fn short_id(id: &str) -> String {
    if id.len() <= 36 {
        id.to_string()
    } else {
        id[..12].to_string()
    }
}

fn relay_total(s: &SessionRow) -> u64 {
    s.counters
        .iter()
        .filter(|(k, _)| k.starts_with("relay_"))
        .map(|(_, v)| v)
        .sum()
}

fn harness_total(s: &SessionRow) -> u64 {
    s.counters
        .iter()
        .filter(|(k, _)| k.starts_with("harness_"))
        .map(|(_, v)| v)
        .sum()
}

/// CLI entry: the flag parse, the env-resolved inputs, the fold, one output.
pub fn run_intel(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--fleet") {
        return crate::fleet_load::run_fleet_cli(args);
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!(
            "fno-agents intel [--days N] [--period 2w|1m|2m|3m|all] [--node <id>]\n\
             [--session <id>] [--json] [-H|--harness claude,codex,opencode|all]\n\
             [--project NAME]... [--all-projects]\n\n\
             The provenance fold: per-session operator/relay/harness/keepalive counters,\n\
             tool_use, commits, the node and PR join, and the relay facets of every bus\n\
             row addressed to the session. Default window 30 days (--period 1m); the\n\
             period words map to --days 14, 30, 60, 90 and 0 (--days 0 means every\n\
             transcript, and --days beside --period is refused). Default scope is this\n\
             project including its worktrees; --project NAME (repeatable, comma-\n\
             separated) names other projects, --all-projects reads the machine,\n\
             -H/--harness narrows the sources. Exit 3 when the window holds no\n\
             sessions.\n"
        );
        return 0;
    }
    let mut days = DEFAULT_DAYS;
    let mut days_set = false;
    let mut period_set = false;
    let mut node: Option<String> = None;
    let mut session: Option<String> = None;
    let mut json = false;
    let mut all_projects = false;
    let mut harness_spec: Vec<String> = Vec::new();
    let mut projects: Vec<String> = Vec::new();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--days" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u64>().ok()) {
                    Some(v) => {
                        days = v;
                        days_set = true;
                    }
                    None => {
                        eprintln!("fno-agents intel: --days needs a non-negative integer");
                        return 2;
                    }
                }
            }
            "--period" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    eprintln!("fno-agents intel: --period needs a word (2w, 1m, 2m, 3m, all)");
                    return 2;
                };
                match period_days(v) {
                    Some(v) => {
                        days = v;
                        period_set = true;
                    }
                    None => {
                        eprintln!(
                            "fno-agents intel: unknown period {v} (known: 2w, 1m, 2m, 3m, all)"
                        );
                        return 2;
                    }
                }
            }
            "--node" => {
                i += 1;
                match args.get(i) {
                    Some(v) => node = Some(v.clone()),
                    None => {
                        eprintln!("fno-agents intel: --node needs an id");
                        return 2;
                    }
                }
            }
            "--session" => {
                i += 1;
                match args.get(i) {
                    Some(v) => session = Some(v.clone()),
                    None => {
                        eprintln!("fno-agents intel: --session needs an id");
                        return 2;
                    }
                }
            }
            "-H" | "--harness" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    eprintln!("fno-agents intel: --harness needs a comma-separated list");
                    return 2;
                };
                for part in v.split(',') {
                    match part.trim() {
                        "" => {}
                        "claude" | "codex" | "opencode" | "all" => {
                            let part = part.trim();
                            if !harness_spec.iter().any(|s| s == part) {
                                harness_spec.push(part.to_string());
                            }
                        }
                        other => {
                            eprintln!(
                                "fno-agents intel: unknown harness {other} (known: claude, codex, opencode, all)"
                            );
                            return 2;
                        }
                    }
                }
            }
            "--project" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    eprintln!("fno-agents intel: --project needs a name");
                    return 2;
                };
                for part in v.split(',') {
                    let part = part.trim();
                    if !part.is_empty() && !projects.iter().any(|s| s == part) {
                        projects.push(part.to_string());
                    }
                }
            }
            "--json" | "-J" => json = true,
            "--all-projects" => all_projects = true,
            other => {
                eprintln!("fno-agents intel: unknown flag {other}");
                return 2;
            }
        }
        i += 1;
    }
    if !projects.is_empty() && all_projects {
        eprintln!("fno-agents intel: --project and --all-projects are exclusive");
        return 2;
    }
    if days_set && period_set {
        eprintln!("fno-agents intel: --days and --period are exclusive");
        return 2;
    }

    let home = AgentsHome::from_env();
    let fno_dir = home
        .root()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".fno"));
    let selected = selected_harnesses(&harness_spec);
    let roots = if all_projects {
        None
    } else if !projects.is_empty() {
        let mut all_roots: Vec<PathBuf> = Vec::new();
        for name in &projects {
            match project_roots(name, &cwd, &fno_dir) {
                Ok(roots) => all_roots.extend(roots),
                Err(why) => {
                    eprintln!("{why}");
                    return 2;
                }
            }
        }
        let mut seen = HashSet::new();
        all_roots.retain(|r| seen.insert(r.clone()));
        Some(all_roots)
    } else {
        Some(default_roots(&cwd, &fno_dir))
    };
    let report = fold_all(
        days,
        node,
        session,
        selected,
        roots,
        projects,
        &fno_dir,
        &home.events_jsonl(),
    );

    if report.sessions.is_empty() {
        println!("no sessions in window");
        return 3;
    }
    if json {
        println!(
            "{}",
            serde_json::to_string(&report).unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        print_report(&report);
    }
    0
}

/// The selected harnesses in canonical order: `all` (or nothing named) expands to every harness, otherwise the named values intersect the canonical order.
fn selected_harnesses(spec: &[String]) -> Vec<&'static str> {
    const ALL: [&str; 3] = ["claude", "codex", "opencode"];
    if spec.iter().any(|s| s == "all") || spec.is_empty() {
        return ALL.to_vec();
    }
    ALL.into_iter()
        .filter(|h| spec.iter().any(|s| s == h))
        .collect()
}

/// The transcript roots one named project resolves to: the checkout, its fno
/// worktrees dir (`<fno>/worktrees/<basename>`), and `<base>/<basename>`
/// when the project config sets `paths.worktrees_base`. Deduplicated,
/// order-preserving.
fn project_roots(name: &str, cwd: &Path, fno_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let map = crate::king_board::scope::project_map(cwd).unwrap_or_default();
    let unknown = |map: &std::collections::HashMap<String, String>| {
        let known: std::collections::BTreeSet<&str> = map.values().map(String::as_str).collect();
        format!(
            "fno-agents intel: unknown project {name} (known: {})",
            known.into_iter().collect::<Vec<_>>().join(", ")
        )
    };
    let Some(canonical) = map.get(name) else {
        return Err(unknown(&map));
    };
    let paths = crate::territory::workspace_paths(cwd);
    let Some(path) = paths.get(canonical).map(PathBuf::from) else {
        return Err(unknown(&map));
    };
    let basename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    let mut roots: Vec<PathBuf> = vec![path.clone()];
    if !basename.is_empty() {
        roots.push(fno_dir.join("worktrees").join(&basename));
    }
    if let Some(base) = crate::agents_config::config_lookup(&path, &["paths", "worktrees_base"])
        .and_then(|v| v.as_str().map(str::to_string))
        .filter(|s| !s.is_empty())
    {
        roots.push(PathBuf::from(crate::territory::normalize_path(&base)).join(&basename));
    }
    let mut seen = HashSet::new();
    roots.retain(|r| seen.insert(r.clone()));
    Ok(roots)
}

/// The default scope: the first configured project whose roots contain the
/// cwd supplies its roots; otherwise the cwd itself is the only root.
fn default_roots(cwd: &Path, fno_dir: &Path) -> Vec<PathBuf> {
    let paths = crate::territory::workspace_paths(cwd);
    let mut names: Vec<&String> = paths.keys().collect();
    names.sort();
    for name in names {
        if let Ok(roots) = project_roots(name, cwd, fno_dir) {
            if roots.iter().any(|r| cwd.starts_with(r)) {
                return roots;
            }
        }
    }
    vec![cwd.to_path_buf()]
}

/// The env-resolved fold `run_intel` prints: the selected sources from the
/// harness list and project roots, bus/ledger/events/manifests from the fno
/// dir.
fn fold_all(
    days: u64,
    node: Option<String>,
    session: Option<String>,
    selected: Vec<&'static str>,
    roots: Option<Vec<PathBuf>>,
    projects: Vec<String>,
    fno_dir: &Path,
    events_journal: &Path,
) -> Report {
    let bus = BusIndex::load(&bus_log_path(fno_dir));
    let join = session_join(fno_dir);
    let events = read_events(&fno_dir.join("events.jsonl"));
    let witness = crate::operator_witness::SubmitIndex::load(events_journal);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut ctx = FoldCtx {
        bus,
        join,
        events,
        witness,
        days,
        now,
    };
    let mut sources: Vec<Box<dyn TranscriptSource>> = Vec::new();
    if selected.contains(&"claude") {
        sources.push(Box::new(ClaudeSource {
            projects_dir: crate::claude_drive::claude_projects_dir(),
            roots: roots.clone(),
        }));
    }
    if selected.contains(&"codex") {
        sources.push(Box::new(CodexSource {
            sessions_dir: None,
            roots: roots.clone(),
        }));
    }
    let mut skipped = BTreeMap::new();
    if selected.contains(&"opencode") {
        let source = OpencodeSource {
            dbs: crate::opencode_transcript::opencode_stores(),
            roots: roots.clone(),
        };
        if let Err(why) = source.probe() {
            skipped.insert("opencode".to_string(), why);
        } else {
            sources.push(Box::new(source));
        }
    }
    let mut rows: Vec<SessionRow> = Vec::new();
    for source in &sources {
        rows.extend(fold_source(source.as_ref(), &mut ctx));
    }
    if let Some(want) = &session {
        rows.retain(|r| &r.session == want);
    }
    if let Some(want) = &node {
        rows.retain(|r| r.node.as_deref() == Some(want.as_str()));
    }
    let nodes = node_rows(&rows, &ctx.bus, ctx.now);
    let totals = totals_of(&rows);
    // The witness receipt over the --days window; `unwitnessed_sessions`
    // counts sessions with unknown turns that no submit row names at all -
    // the from-the-fold measure of sessions typed outside the mux.
    let window_start_ms = if days == 0 {
        i64::MIN
    } else {
        (now.saturating_sub(days * 86_400)) as i64 * 1_000
    };
    let unwitnessed_sessions = rows
        .iter()
        .filter(|r| r.counters.get("unknown").copied().unwrap_or(0) > 0)
        .filter(|r| !ctx.witness.has_session(&r.session))
        .count();
    let witness = ctx.witness.receipt(window_start_ms, unwitnessed_sessions);
    Report {
        days,
        scope: Scope {
            harnesses: selected,
            all_projects: roots.is_none(),
            projects,
            roots: roots
                .as_ref()
                .map(|rs| rs.iter().map(|r| r.display().to_string()).collect())
                .unwrap_or_default(),
        },
        sessions: rows,
        nodes,
        skipped,
        totals,
        witness,
    }
}

fn read_events(path: &Path) -> Vec<Value> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

fn totals_of(rows: &[SessionRow]) -> Totals {
    let mut totals = Totals {
        sessions: rows.len(),
        ..Default::default()
    };
    for row in rows {
        for (label, count) in &row.counters {
            *totals.counters.entry(*label).or_insert(0) += count;
        }
        totals.relay_rows += row.relay.len();
        totals.undelivered += row.relay.iter().filter(|f| !f.delivered).count();
        totals.unanswered += row.relay.iter().filter(|f| !f.answered).count();
        if row.kind == "attended" {
            totals.operator_sessions += 1;
        }
    }
    totals
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CLAUDE_SID: &str = "ccccdddd-1111-2222-3333-444455556666";
    const CODEX_SID: &str = "0f0e1d2c-3b4a-4958-8675-3092f4c1b2a3";
    const QUIET_SID: &str = "eeee1111-2222-4333-8444-555566667777";

    fn write_lines(path: &Path, lines: &[Value]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let file = std::fs::File::create(path).unwrap();
        let mut writer = std::io::BufWriter::new(file);
        for line in lines {
            writeln!(writer, "{line}").unwrap();
        }
    }

    fn user_row(text: &str) -> Value {
        serde_json::json!({
            "type": "user", "uuid": "u1", "timestamp": "2026-09-16T12:00:00.000Z",
            "message": {"role": "user", "content": text}
        })
    }

    /// One env-pinned fixture: a claude transcript, a codex rollout, a bus
    /// log, a ledger, and an events file.
    struct Fixture {
        dir: PathBuf,
        cwd: PathBuf,
    }

    fn build_fixture(tag: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("fno-intel-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = PathBuf::from("/fixture/project");

        // Claude: 2 typed turns, 1 mail envelope, 1 keepalive, 1 tool call.
        let slug = crate::claude_ask::claude_cwd_slug(&cwd);
        let claude_rows = vec![
            user_row("please widen the review gate"),
            user_row("ship it when green"),
            user_row("<fno_mail from=\"peer\" to=\"me\" id=\"msg-9\">run the sweep</fno_mail>"),
            user_row("[cache-keepalive] Ping 1/4"),
            serde_json::json!({
                "type": "assistant", "timestamp": "2026-09-16T12:01:00.000Z",
                "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}}
                ]}
            }),
        ];
        write_lines(
            &dir.join("claude")
                .join(&slug)
                .join(format!("{CLAUDE_SID}.jsonl")),
            &claude_rows,
        );

        // Codex: 1 typed turn whose body equals the bus row (bus-row relay).
        let codex_rows = vec![serde_json::json!({
            "payload": {"type": "message", "role": "user",
                        "content": "drive the review to merge"},
            "timestamp": "2026-09-16T13:00:00.000Z"
        })];
        write_lines(
            &dir.join("codex")
                .join("sessions")
                .join("2026")
                .join("09")
                .join("16")
                .join(format!("rollout-2026-09-16T13-00-00-{CODEX_SID}.jsonl")),
            &codex_rows,
        );

        // Bus: msg-9 lands verbatim in the claude transcript; a raw body row
        // to the codex session; an undelivered row to the codex session.
        let bus_rows = vec![
            serde_json::json!({
                "v": 1, "id": "msg-9", "ts": "2026-09-16T11:00:00Z",
                "from_session": "dddd0000-0000-0000-0000-000000000000",
                "meta": {"to_session": CLAUDE_SID}, "word_count": 3,
                "body": "run the sweep"
            }),
            serde_json::json!({
                "v": 1, "id": "msg-10", "ts": "2026-09-16T12:30:00Z",
                "from_session": "dddd0000-0000-0000-0000-000000000000",
                "meta": {"to_session": CODEX_SID}, "word_count": 5,
                "body": "drive the review to merge"
            }),
            serde_json::json!({
                "v": 1, "id": "msg-11", "ts": "2026-09-16T12:40:00Z",
                "from_session": "dddd0000-0000-0000-0000-000000000000",
                "meta": {"to_session": CODEX_SID}, "word_count": 4,
                "body": "this body never reached the transcript anywhere"
            }),
        ];
        write_lines(&dir.join("bus").join("messages.jsonl"), &bus_rows);

        // Ledger: the claude session joins to test-node / PR 1234.
        let ledger = serde_json::json!({"entries": [{
            "session_id": "20260916T120000Z-fno-run",
            "graph_node_id": "test-node", "pr_number": 1234,
            "sessions": [CLAUDE_SID, "20260916T120000Z-fno-run"]
        }]});
        write_lines(&dir.join("ledger.json"), &[ledger]);

        // Events: two loop_check fingerprints for the run id -> one commit.
        let events = vec![
            serde_json::json!({
                "type": "loop_check", "ts": "2026-09-16T12:05:00Z",
                "data": {"session_id": "20260916T120000Z-fno-run",
                         "fingerprint": "aaa111|no_pr|pending|"}
            }),
            serde_json::json!({
                "type": "loop_check", "ts": "2026-09-16T12:10:00Z",
                "data": {"session_id": "20260916T120000Z-fno-run",
                         "fingerprint": "bbb222|open|pending|"}
            }),
        ];
        write_lines(&dir.join("events.jsonl"), &events);

        // The witness journal: two operator_submit rows 1.1s and 0.4s before
        // the fixture's typed turns, so both bind (AC3-HP).
        let base = ts_secs("2026-09-16T12:00:00.000Z").unwrap() as i64 * 1000;
        let witness = vec![
            serde_json::json!({
                "ts": "2026-09-16T11:59:58Z", "type": "operator_submit",
                "source": "daemon",
                "data": {"mux_session": "main", "pane": 7, "via": "pane",
                         "submit_ms": base - 1100, "resolution": "ok",
                         "harness_session": CLAUDE_SID}
            }),
            serde_json::json!({
                "ts": "2026-09-16T11:59:59Z", "type": "operator_submit",
                "source": "daemon",
                "data": {"mux_session": "main", "pane": 7, "via": "pane",
                         "submit_ms": base - 400, "resolution": "ok",
                         "harness_session": CLAUDE_SID}
            }),
        ];
        write_lines(&dir.join("witness").join("events.jsonl"), &witness);

        Fixture { dir, cwd }
    }

    /// The fold over the fixture, claude + codex sources. The roots are
    /// injected, never env: a full-suite run shares one process, and a
    /// set_var here would leak into unrelated tests (the heal suite's
    /// hermetic fence panics on a foreign CODEX_HOME).
    fn fold_fixture() -> (Fixture, Vec<SessionRow>) {
        // A unique tag per call: parallel tests share the process, and one
        // shared fixture dir would let a rebuild race another test's fold.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tag = format!(
            "folded-{}",
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let fx = build_fixture(&tag);
        let bus = BusIndex::load(&fx.dir.join("bus").join("messages.jsonl"));
        let mut ctx = FoldCtx {
            bus,
            join: session_join(&fx.dir),
            events: read_events(&fx.dir.join("events.jsonl")),
            witness: crate::operator_witness::SubmitIndex::load(
                &fx.dir.join("witness").join("events.jsonl"),
            ),
            days: 30,
            now: 1_800_000_000,
        };
        let claude = ClaudeSource {
            projects_dir: fx.dir.join("claude"),
            roots: Some(vec![fx.cwd.clone()]),
        };
        let codex = CodexSource {
            sessions_dir: Some(fx.dir.join("codex").join("sessions")),
            roots: None,
        };
        let sources: [&dyn TranscriptSource; 2] = [&claude, &codex];
        let mut rows = Vec::new();
        for source in sources {
            rows.extend(fold_source(source, &mut ctx));
        }
        (fx, rows)
    }

    #[test]
    fn the_fixture_folds_two_sessions_one_per_harness_with_every_counter() {
        let (_fx, rows) = fold_fixture();
        assert_eq!(rows.len(), 2, "one claude + one codex row");
        let claude = rows.iter().find(|r| r.harness == "claude").unwrap();
        let codex = rows.iter().find(|r| r.harness == "codex").unwrap();
        for row in [claude, codex] {
            for label in Provenance::all_labels() {
                assert!(
                    row.counters.contains_key(label),
                    "missing counter {label} on {}",
                    row.harness
                );
            }
        }
        assert_eq!(claude.counters.get("operator"), Some(&2));
        assert_eq!(
            claude.operator_turns.len(),
            2,
            "both witnessed turns carry their raw timestamps"
        );
        assert_eq!(claude.counters.get("relay_fno_mail"), Some(&1));
        assert_eq!(claude.counters.get("keepalive"), Some(&1));
        assert_eq!(claude.tool_use, 1);
        assert_eq!(claude.node.as_deref(), Some("test-node"));
        assert_eq!(claude.pr_number, Some(1234));
        assert_eq!(claude.commits, 1);
        assert_eq!(codex.counters.get("operator"), Some(&0));
        assert_eq!(codex.counters.get("relay_bus_row"), Some(&1));
    }

    #[test]
    fn a_bus_row_absent_from_the_transcript_reports_undelivered() {
        let (_fx, rows) = fold_fixture();
        let codex = rows.iter().find(|r| r.harness == "codex").unwrap();
        let undelivered = codex.relay.iter().find(|f| f.id == "msg-11").unwrap();
        assert!(!undelivered.delivered);
        let delivered = codex.relay.iter().find(|f| f.id == "msg-10").unwrap();
        assert!(delivered.delivered);
        assert_eq!(codex.relay.len(), 2);
    }

    #[test]
    fn a_session_with_zero_operator_and_relay_turns_is_unattended() {
        let fx = build_fixture("quiet");
        // A transcript with only a keepalive ping: no operator, no relay.
        let slug = crate::claude_ask::claude_cwd_slug(&fx.cwd);
        write_lines(
            &fx.dir
                .join("claude")
                .join(&slug)
                .join(format!("{QUIET_SID}.jsonl")),
            &[user_row("[cache-keepalive] Ping 2/4")],
        );
        let mut ctx = FoldCtx {
            bus: BusIndex::empty(),
            join: HashMap::new(),
            events: Vec::new(),
            witness: crate::operator_witness::SubmitIndex::empty(),
            days: 30,
            now: 1_800_000_000,
        };
        let claude = ClaudeSource {
            projects_dir: fx.dir.join("claude"),
            roots: Some(vec![fx.cwd.clone()]),
        };
        let rows = fold_source(&claude, &mut ctx);
        let quiet = rows.iter().find(|r| r.session == QUIET_SID).unwrap();
        assert_eq!(quiet.kind, "unattended");
        let totals = totals_of(&rows);
        assert_eq!(totals.operator_sessions, 1, "only the attended session");
        assert_eq!(totals.sessions, 2);
    }

    #[test]
    fn unknown_flag_exits_two_and_json_spelling_is_known() {
        // Pure flag-parse checks; no env, no fold.
        assert_eq!(run_intel(&["--nonsense".to_string()]), 2);
        assert_eq!(run_intel(&["--days".to_string()]), 2);
        assert_eq!(run_intel(&["--node".to_string()]), 2);
    }

    #[test]
    fn harness_and_project_refusals_exit_two() {
        assert_eq!(run_intel(&["-H".into(), "codex,bogus".into()]), 2);
        assert_eq!(run_intel(&["--harness".into(), "bogus".into()]), 2);
        assert_eq!(
            run_intel(&["--project".into(), "nope".into(), "--all-projects".into()]),
            2
        );
    }

    #[test]
    fn period_maps_to_days_and_the_default_is_one_month() {
        assert_eq!(period_days("2w"), Some(14));
        assert_eq!(period_days("1m"), Some(30));
        assert_eq!(period_days("2m"), Some(60));
        assert_eq!(period_days("3m"), Some(90));
        assert_eq!(period_days("all"), Some(0));
        assert_eq!(period_days("fortnight"), None);
        assert_eq!(DEFAULT_DAYS, 30, "the default period is 1m");
        assert_eq!(run_intel(&["--period".into(), "1x".into()]), 2);
        assert_eq!(
            run_intel(&["--days".into(), "7".into(), "--period".into(), "1m".into()]),
            2
        );
    }

    /// Overwrite the fixture's claude transcript with plain unshaped turns at
    /// base, base+1s, base+2s (UTC RFC3339), and return their raw stamps.
    fn write_unshaped_claude_turns(fx: &Fixture, sid: &str) -> Vec<String> {
        let base = ts_secs("2026-09-16T12:00:00Z").unwrap() as f64;
        let stamps: Vec<String> = (0..3)
            .map(|k| rfc3339_str(base + f64::from(k)).unwrap())
            .collect();
        let rows: Vec<Value> = stamps
            .iter()
            .map(|ts| {
                serde_json::json!({
                    "type": "user", "uuid": "u", "timestamp": ts,
                    "message": {"role": "user", "content": "a plain typed turn"}
                })
            })
            .collect();
        let slug = crate::claude_ask::claude_cwd_slug(&fx.cwd);
        write_lines(
            &fx.dir
                .join("claude")
                .join(&slug)
                .join(format!("{sid}.jsonl")),
            &rows,
        );
        stamps
    }

    fn fold_fixture_ctx(fx: &Fixture) -> FoldCtx {
        FoldCtx {
            bus: BusIndex::load(&fx.dir.join("bus").join("messages.jsonl")),
            join: session_join(&fx.dir),
            events: read_events(&fx.dir.join("events.jsonl")),
            witness: crate::operator_witness::SubmitIndex::load(
                &fx.dir.join("witness").join("events.jsonl"),
            ),
            days: 30,
            now: 1_800_000_000,
        }
    }

    #[test]
    fn three_unshaped_turns_two_submits_reads_operator_two_unknown_one() {
        let fx = build_fixture("ac3hp");
        let stamps = write_unshaped_claude_turns(&fx, CLAUDE_SID);
        let mut ctx = fold_fixture_ctx(&fx);
        let claude = ClaudeSource {
            projects_dir: fx.dir.join("claude"),
            roots: Some(vec![fx.cwd.clone()]),
        };
        let rows = fold_source(&claude, &mut ctx);
        let row = rows.iter().find(|r| r.session == CLAUDE_SID).unwrap();
        assert_eq!(row.counters.get("operator"), Some(&2));
        assert_eq!(row.counters.get("unknown"), Some(&1));
        assert_eq!(row.operator_turns, stamps[..2]);
        let receipt = ctx.witness.receipt(0, 0);
        assert_eq!(receipt.bound, 2);
    }

    #[test]
    fn no_witness_journal_reads_every_unshaped_turn_unknown() {
        let fx = build_fixture("ac3err");
        let _ = std::fs::remove_file(fx.dir.join("witness").join("events.jsonl"));
        let stamps = write_unshaped_claude_turns(&fx, CLAUDE_SID);
        let mut ctx = fold_fixture_ctx(&fx);
        let claude = ClaudeSource {
            projects_dir: fx.dir.join("claude"),
            roots: Some(vec![fx.cwd.clone()]),
        };
        let rows = fold_source(&claude, &mut ctx);
        let row = rows.iter().find(|r| r.session == CLAUDE_SID).unwrap();
        assert_eq!(row.counters.get("operator"), Some(&0));
        assert_eq!(row.counters.get("unknown"), Some(&3));
        assert!(row.operator_turns.is_empty());
        let receipt = ctx.witness.receipt(0, 0);
        assert_eq!(receipt.submits, 0);
        assert_eq!(stamps.len(), 3);
    }

    #[test]
    fn one_submit_binds_exactly_one_of_two_turns() {
        let fx = build_fixture("ac3edge");
        let _ = write_unshaped_claude_turns(&fx, CLAUDE_SID);
        // Keep only the -1100ms submit: one submit, three unshaped turns.
        let base = ts_secs("2026-09-16T12:00:00.000Z").unwrap() as i64 * 1000;
        let witness = vec![serde_json::json!({
            "ts": "2026-09-16T11:59:58Z", "type": "operator_submit",
            "source": "daemon",
            "data": {"mux_session": "main", "pane": 7, "via": "pane",
                     "submit_ms": base - 1100, "resolution": "ok",
                     "harness_session": CLAUDE_SID}
        })];
        write_lines(&fx.dir.join("witness").join("events.jsonl"), &witness);
        let mut ctx = fold_fixture_ctx(&fx);
        let claude = ClaudeSource {
            projects_dir: fx.dir.join("claude"),
            roots: Some(vec![fx.cwd.clone()]),
        };
        let rows = fold_source(&claude, &mut ctx);
        let row = rows.iter().find(|r| r.session == CLAUDE_SID).unwrap();
        assert_eq!(row.counters.get("operator"), Some(&1));
        assert_eq!(row.counters.get("unknown"), Some(&2));
    }

    #[test]
    fn a_relay_turn_never_binds_a_submit() {
        let fx = build_fixture("ac3relay");
        let base = ts_secs("2026-09-16T12:00:00.000Z").unwrap() as i64 * 1000;
        // One submit 1s before the transcript's ONLY turn: a mail envelope.
        let witness = vec![serde_json::json!({
            "ts": "2026-09-16T11:59:59Z", "type": "operator_submit",
            "source": "daemon",
            "data": {"mux_session": "main", "pane": 7, "via": "pane",
                     "submit_ms": base - 1000, "resolution": "ok",
                     "harness_session": CLAUDE_SID}
        })];
        write_lines(&fx.dir.join("witness").join("events.jsonl"), &witness);
        let slug = crate::claude_ask::claude_cwd_slug(&fx.cwd);
        write_lines(
            &fx.dir
                .join("claude")
                .join(slug)
                .join(format!("{CLAUDE_SID}.jsonl")),
            &[user_row(
                "<fno_mail from=\"peer\" to=\"me\" id=\"msg-9\">run the sweep</fno_mail>",
            )],
        );
        let mut ctx = fold_fixture_ctx(&fx);
        let claude = ClaudeSource {
            projects_dir: fx.dir.join("claude"),
            roots: Some(vec![fx.cwd.clone()]),
        };
        let rows = fold_source(&claude, &mut ctx);
        let row = rows.iter().find(|r| r.session == CLAUDE_SID).unwrap();
        // The mail turn stays a relay; the submit stays unbound for a typed
        // turn to claim.
        assert_eq!(row.counters.get("relay_fno_mail"), Some(&1));
        let receipt = ctx.witness.receipt(0, 0);
        assert_eq!(receipt.bound, 0);
    }
}
