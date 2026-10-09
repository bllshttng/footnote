//! The launch record: who started a session, written by the launcher and
//! claimed by the child.
//!
//! A `claude --bg` child cannot see its launcher. Every bg worker is a child
//! of one claude daemon pid and its env carries no launcher session id, so
//! env intent and the ppid chain both lose the lead. The launcher's own
//! PreToolUse Bash hook does see the launch command, so it writes a record
//! under `<agents home>/launches/pending/`. The child's SessionStart report,
//! which all six harnesses send, claims the record by harness, cwd, name and
//! a short window, and the claim stamps the node's first-launch edge.
//!
//! A record never names a parent it did not observe: no launcher session id,
//! no record. A claim never guesses: two equal candidates claim nothing.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// How long a pending record stays claimable, and the prune age.
pub const WINDOW_MS: i64 = 10 * 60 * 1000;

/// One observed launch. `cwd` is where the child starts; the `launcher_*`
/// triple is the parent edge the claim writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launch {
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    #[serde(flatten)]
    pub launch: Launch,
    pub launcher_session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launcher_harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launcher_cwd: Option<String>,
    pub launched_at_ms: i64,
    #[serde(default)]
    pub command: String,
    /// Set by the claim: the child that took this record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at_ms: Option<i64>,
}

/// The session that starts and wants its launcher.
#[derive(Debug, Clone)]
pub struct Child {
    pub harness: String,
    pub session_id: String,
    pub cwd: String,
    pub name: Option<String>,
    pub started_at_ms: i64,
}

fn pending_dir(root: &Path) -> PathBuf {
    root.join("launches").join("pending")
}

fn claimed_dir(root: &Path) -> PathBuf {
    root.join("launches").join("claimed")
}

// ---------------------------------------------------------------------------
// parse

/// Split a shell command into simple-command segments of unquoted words.
/// Quotes and backslashes are honored; `&&`, `||`, `;`, `|`, `&` and
/// newlines end a segment. Redirections and substitutions are not expanded:
/// a launch hidden behind them is not seen, which is the documented gap.
fn segments(cmd: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = cmd.chars().peekable();
    let end_word = |words: &mut Vec<String>, word: &mut String, in_word: &mut bool| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                word.push(n);
                            }
                        }
                        _ => word.push(q),
                    }
                }
            }
            '\\' => {
                if let Some(n) = chars.next() {
                    if n != '\n' {
                        in_word = true;
                        word.push(n);
                    }
                }
            }
            // `2>&1` and `&>` are redirections, not a background separator.
            '&' if (in_word && word.ends_with(['>', '<'])) || chars.peek() == Some(&'>') => {
                in_word = true;
                word.push(c);
            }
            ';' | '&' | '|' | '\n' => {
                end_word(&mut words, &mut word, &mut in_word);
                if !words.is_empty() {
                    out.push(std::mem::take(&mut words));
                }
            }
            c if c.is_whitespace() => end_word(&mut words, &mut word, &mut in_word),
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    end_word(&mut words, &mut word, &mut in_word);
    if !words.is_empty() {
        out.push(words);
    }
    out
}

/// Harnesses whose binary name equals their id, past claude and codex.
const PROMPT_HARNESSES: &[&str] = &[
    "agy",
    "opencode",
    "pi",
    "gemini",
    "cursor-agent",
    "grok",
    "zcode",
];

/// Flags that take a value, so the value is not read as the prompt.
const VALUE_FLAGS: &[&str] = &[
    "-n",
    "--name",
    "-m",
    "--model",
    "--settings",
    "--permission-mode",
    "--add-dir",
    "--effort",
    "-c",
    "--config",
    "-C",
    "--cd",
    "--cwd",
    "-H",
    "--harness",
    "-P",
    "--provider",
    "--node",
    "--agent",
    "--session-id",
    "--resume",
    "-r",
    "--substrate",
    "--account",
    "--route",
    "--mcp-config",
    "--append-system-prompt",
    "--system-prompt",
    "-s",
    "--sandbox",
    "-a",
    "--ask-for-approval",
    "--output-format",
    "--input-format",
    "--max-turns",
    "--allowedTools",
    "--disallowedTools",
    "--profile",
    "--dir",
];

fn flag_value(args: &[String], names: &[&str]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if names.contains(&a.as_str()) {
            return it.next().cloned();
        }
        for n in names.iter().filter(|n| n.starts_with("--")) {
            if let Some(v) = a.strip_prefix(&format!("{n}=")) {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// The non-flag words, skipping each value-taking flag's value.
fn positionals(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            out.extend(it.by_ref().cloned());
            break;
        }
        if a.starts_with('-') {
            if VALUE_FLAGS.contains(&a.as_str()) {
                it.next();
            }
            continue;
        }
        if let Some(op_len) = redirection_op_len(a) {
            // A bare operator (`>`, `2>`) takes the next word as its target.
            if op_len == a.len() {
                it.next();
            }
            continue;
        }
        out.push(a.clone());
    }
    out
}

/// The length of a leading redirection operator (`>`, `>>`, `2>`, `&>`,
/// `<`, `2>&1`), or `None` when the word is not a redirection.
fn redirection_op_len(word: &str) -> Option<usize> {
    let digits = word.chars().take_while(char::is_ascii_digit).count();
    let rest = &word[digits..];
    let rest_amp = rest.strip_prefix('&').unwrap_or(rest);
    if !rest_amp.starts_with(['>', '<']) {
        return None;
    }
    let ops = word
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '>' | '<' | '&'))
        .count();
    Some(ops)
}

fn has_any(args: &[String], flags: &[&str]) -> bool {
    args.iter().any(|a| flags.contains(&a.as_str()))
}

fn seed_node(seed: Option<&String>) -> Option<String> {
    seed.and_then(|s| crate::node_seed::scan_seed_node(s))
}

fn join_cwd(base: &str, dir: &str) -> String {
    let p = Path::new(dir);
    if p.is_absolute() {
        dir.to_string()
    } else if let Some(rest) = dir.strip_prefix("~/") {
        std::env::var("HOME")
            .map(|h| format!("{h}/{rest}"))
            .unwrap_or_else(|_| dir.to_string())
    } else {
        Path::new(base).join(p).to_string_lossy().into_owned()
    }
}

/// Every launch a shell command starts, with the cwd each child starts in.
pub fn parse_command(cmd: &str, cwd: &str) -> Vec<Launch> {
    let mut out = Vec::new();
    let mut here = cwd.to_string();
    for seg in segments(cmd) {
        let mut i = 0;
        // Wrapper heads that run the next word as the command.
        while i < seg.len() {
            let w = seg[i].as_str();
            if w.contains('=') && !w.starts_with('-') && !w.starts_with('=') {
                i += 1;
            } else if matches!(w, "env" | "nohup" | "exec" | "command" | "caffeinate") {
                i += 1;
            } else if matches!(w, "timeout" | "gtimeout") {
                i += 2;
            } else {
                break;
            }
        }
        let Some(head) = seg.get(i) else { continue };
        let args = &seg[i + 1..];
        if head == "cd" {
            if let Some(dir) = args.first() {
                here = join_cwd(&here, dir);
            }
            continue;
        }
        if has_any(args, &["--help", "-h", "--version", "-v", "-V"]) {
            continue;
        }
        let bin = Path::new(head)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let launch = match bin.as_str() {
            "claude" => {
                if !has_any(args, &["--bg", "--background"]) {
                    continue;
                }
                let pos = positionals(args);
                Some(Launch {
                    harness: "claude".into(),
                    name: flag_value(args, &["-n", "--name"]),
                    node: seed_node(pos.last()),
                    cwd: here.clone(),
                })
            }
            "codex" => {
                if args.first().map(String::as_str) != Some("exec")
                    || args.get(1).map(String::as_str) == Some("resume")
                {
                    continue;
                }
                let pos = positionals(&args[1..]);
                let cwd = flag_value(args, &["-C", "--cd"])
                    .map(|d| join_cwd(&here, &d))
                    .unwrap_or_else(|| here.clone());
                Some(Launch {
                    harness: "codex".into(),
                    name: None,
                    node: seed_node(pos.last()),
                    cwd,
                })
            }
            "fno" | "fno-agents" => {
                let rest: &[String] = if bin == "fno" {
                    if args.first().map(String::as_str) != Some("agents") {
                        continue;
                    }
                    &args[1..]
                } else {
                    args
                };
                if rest.first().map(String::as_str) != Some("spawn") {
                    continue;
                }
                let rest = &rest[1..];
                let pos = positionals(rest);
                let cwd = flag_value(rest, &["--cwd"])
                    .map(|d| join_cwd(&here, &d))
                    .unwrap_or_else(|| here.clone());
                Some(Launch {
                    harness: flag_value(rest, &["-H", "--harness"])
                        .unwrap_or_else(|| "claude".into()),
                    name: flag_value(rest, &["--name"]),
                    node: flag_value(rest, &["--node"]).or_else(|| seed_node(pos.first())),
                    cwd,
                })
            }
            other if PROMPT_HARNESSES.contains(&other) => {
                let (args, sub) = match args.first().map(String::as_str) {
                    Some("run") | Some("exec") => (&args[1..], true),
                    _ => (args, false),
                };
                let print = has_any(args, &["-p", "--print", "--prompt"]);
                let pos = positionals(args);
                if !sub && !print && pos.is_empty() {
                    continue;
                }
                let seed = flag_value(args, &["-p", "--print", "--prompt"])
                    .filter(|v| !v.starts_with('-'))
                    .or_else(|| pos.last().cloned());
                Some(Launch {
                    harness: other.to_string(),
                    name: flag_value(args, &["-n", "--name"]),
                    node: seed_node(seed.as_ref()),
                    cwd: here.clone(),
                })
            }
            _ => None,
        };
        out.extend(launch);
    }
    out
}

/// The cheap first test the hook runs on every Bash call: no harness word,
/// no parse.
pub fn may_launch(cmd: &str) -> bool {
    [
        "claude",
        "codex",
        "agy",
        "opencode",
        "gemini",
        "cursor-agent",
        "grok",
        "zcode",
        "spawn",
    ]
    .iter()
    .any(|w| cmd.contains(w))
        || cmd.split_whitespace().any(|w| w == "pi")
}

// ---------------------------------------------------------------------------
// write

fn canonical(cwd: &str) -> String {
    std::fs::canonicalize(cwd)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| cwd.trim_end_matches('/').to_string())
}

fn file_key(r: &Record) -> String {
    let mut h = Sha256::new();
    h.update(r.launcher_session.as_bytes());
    h.update(b"\0");
    h.update(r.launch.harness.as_bytes());
    h.update(b"\0");
    h.update(r.launch.cwd.as_bytes());
    h.update(b"\0");
    match &r.launch.name {
        // One named launch is one file, whichever writer saw it first.
        Some(n) => h.update(n.as_bytes()),
        None => h.update(r.launched_at_ms.to_string().as_bytes()),
    }
    let digest = h.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("{}-{hex}.json", r.launched_at_ms)
}

fn write_atomic(path: &Path, value: &Record) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut tmp, &serde_json::to_vec(value)?)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Delete pending records older than the window. Best effort.
fn prune(root: &Path, now_ms: i64) {
    let Ok(entries) = std::fs::read_dir(pending_dir(root)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let born = name.split('-').next().and_then(|t| t.parse::<i64>().ok());
        if born.is_some_and(|b| now_ms - b > WINDOW_MS) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Write one pending record per launch the command starts. Returns how many
/// were written. A missing launcher session writes nothing: a record that
/// names no parent proves nothing.
pub fn record_command(
    root: &Path,
    cmd: &str,
    cwd: &str,
    launcher_session: Option<&str>,
    launcher_harness: Option<&str>,
    now_ms: i64,
) -> std::io::Result<usize> {
    let Some(session) = launcher_session.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(0);
    };
    let launches = parse_command(cmd, cwd);
    if launches.is_empty() {
        return Ok(0);
    }
    prune(root, now_ms);
    let command: String = cmd.chars().take(300).collect();
    for mut launch in launches.iter().cloned() {
        launch.cwd = canonical(&launch.cwd);
        let record = Record {
            launch,
            launcher_session: session.to_string(),
            launcher_harness: launcher_harness.map(str::to_string),
            launcher_cwd: Some(cwd.to_string()),
            launched_at_ms: now_ms,
            command: command.clone(),
            child_session: None,
            claimed_at_ms: None,
        };
        write_atomic(&pending_dir(root).join(file_key(&record)), &record)?;
    }
    Ok(launches.len())
}

// ---------------------------------------------------------------------------
// claim

fn read_record(path: &Path) -> Option<Record> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The record a child already claimed, if any.
pub fn claimed_edge(root: &Path, session_id: &str) -> Option<Record> {
    if session_id.is_empty() || session_id.contains('/') {
        return None;
    }
    read_record(&claimed_dir(root).join(format!("{session_id}.json")))
}

/// Claim the one pending record this child is the launch of. A record whose
/// name equals the child's name wins outright; otherwise exactly one
/// candidate must remain. The claim is a rename, so a lost race moves on.
pub fn claim(root: &Path, child: &Child) -> Option<Record> {
    if let Some(done) = claimed_edge(root, &child.session_id) {
        return Some(done);
    }
    if child.session_id.is_empty() || child.session_id.contains('/') {
        return None;
    }
    let cwd = canonical(&child.cwd);
    let mut candidates: Vec<(PathBuf, Record)> = std::fs::read_dir(pending_dir(root))
        .ok()?
        .flatten()
        .filter_map(|e| read_record(&e.path()).map(|r| (e.path(), r)))
        .filter(|(_, r)| {
            r.launch.harness == child.harness
                && r.launch.cwd == cwd
                && r.launched_at_ms <= child.started_at_ms + 5_000
                && child.started_at_ms - r.launched_at_ms <= WINDOW_MS
                && match (&r.launch.name, &child.name) {
                    (Some(a), Some(b)) => a == b,
                    _ => true,
                }
        })
        .collect();
    candidates.sort_by_key(|(_, r)| r.launched_at_ms);
    let named: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, (_, r))| r.launch.name.is_some() && r.launch.name == child.name)
        .map(|(i, _)| i)
        .collect();
    let pick: Vec<usize> = if !named.is_empty() {
        named
    } else if candidates.len() == 1 {
        vec![0]
    } else {
        if candidates.len() > 1 {
            eprintln!(
                "launch record: {} candidates match {} session {} in {cwd}; none claimed",
                candidates.len(),
                child.harness,
                child.session_id
            );
        }
        return None;
    };
    let dest = claimed_dir(root).join(format!("{}.json", child.session_id));
    std::fs::create_dir_all(claimed_dir(root)).ok()?;
    for i in pick {
        let (path, record) = &candidates[i];
        // The rename is the claim: a racing child that renamed first leaves
        // this path missing, and we try the next one.
        if std::fs::rename(path, &dest).is_ok() {
            let mut record = record.clone();
            record.child_session = Some(child.session_id.clone());
            record.claimed_at_ms = Some(child.started_at_ms);
            let _ = write_atomic(&dest, &record);
            return Some(record);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// the node's first-launch edge

/// What one stamp did, for receipts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stamp {
    Wrote,
    Kept(String),
    NoNode,
}

/// Write `spawned_by_*` on `node` when its `spawned_by_session` is empty.
/// First launch wins: an existing edge is never overwritten. The check runs
/// inside the locked single-row transaction, so a racing stamp cannot both
/// write.
pub fn stamp_node(
    graph: &Path,
    node: &str,
    session: &str,
    harness: Option<&str>,
    cwd: Option<&str>,
) -> Result<Stamp, String> {
    let outcome = std::cell::RefCell::new(Stamp::NoNode);
    crate::backlog::mutate_single_row(graph, "launch_edge_stamp", |rows| {
        let Some(row) = rows
            .iter_mut()
            .find(|r| r.get("id").and_then(Value::as_str) == Some(node))
        else {
            outcome.replace(Stamp::NoNode);
            return Ok(false);
        };
        if let Some(existing) = row
            .get("spawned_by_session")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            outcome.replace(Stamp::Kept(existing.to_string()));
            return Ok(false);
        }
        let obj = row.as_object_mut().ok_or("node row is not an object")?;
        let opt = |v: Option<&str>| v.map_or(Value::Null, |s| Value::String(s.to_string()));
        obj.insert(
            "spawned_by_session".into(),
            Value::String(session.to_string()),
        );
        obj.insert("spawned_by_harness".into(), opt(harness));
        obj.insert("spawned_by_cwd".into(), opt(cwd));
        outcome.replace(Stamp::Wrote);
        Ok(true)
    })?;
    Ok(outcome.into_inner())
}

/// The child's own name: a claude --bg job carries it in its job state, a
/// spawned worker in `FNO_AGENT_SELF`.
pub fn child_name_from_env() -> Option<String> {
    if let Some(dir) = std::env::var_os("CLAUDE_JOB_DIR").filter(|d| !d.is_empty()) {
        let state = PathBuf::from(dir).join("state.json");
        if let Some(name) = std::fs::read(&state)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v.get("name").and_then(Value::as_str).map(str::to_string))
            .filter(|n| !n.is_empty())
        {
            return Some(name);
        }
    }
    std::env::var("FNO_AGENT_SELF")
        .ok()
        .filter(|n| !n.is_empty())
}

/// The SessionStart leg: claim this child's record and stamp its node.
/// Never fails the report; every miss is silent or one stderr line.
pub fn claim_and_stamp(root: &Path, graph: &Path, child: &Child) -> Option<Record> {
    // A worker's SessionStart reports twice; only the first claim stamps.
    if let Some(done) = claimed_edge(root, &child.session_id) {
        return Some(done);
    }
    let record = claim(root, child)?;
    if let Some(node) = record.launch.node.as_deref() {
        if crate::backlog::workflows::active_backend_name() == "graph" {
            match stamp_node(
                graph,
                node,
                &record.launcher_session,
                record.launcher_harness.as_deref(),
                record.launcher_cwd.as_deref(),
            ) {
                Ok(Stamp::NoNode) => {
                    eprintln!("launch record: node {node} is not in the graph; no edge written")
                }
                Ok(_) => {}
                Err(e) => eprintln!("launch record: edge on {node} not written: {e}"),
            }
        }
    }
    Some(record)
}

#[cfg(test)]
#[path = "launch_record_tests.rs"]
mod tests;
