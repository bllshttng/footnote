//! `fno-agents verify-evidence` verb — Rust port of
//! `scripts/lib/verify-event-evidence.sh` (packaging EPIC ab-8bdb4642,
//! eliminate-don't-vendor leg).
//!
//! Three sub-functions are exposed via a leading sub-token:
//!   - `verify-evidence event SID NONCE EVENTS ARTIFACT`
//!   - `verify-evidence child-promise SID NONCE [EVENTS]`
//!   - `verify-evidence has-nonclaude ARTIFACT [SETTINGS]`
//!
//! Each reproduces the bash exit-code contract, stdout diagnostic `kind`
//! strings, and stderr soft-warnings byte-for-byte. The bash script stays
//! in-tree as the parity oracle (differential tests in
//! `tests/verify_evidence_parity.rs`).
//!
//! Diagnostic vocabulary (event sub-verb, rc=1, one stdout line):
//!   agent_mismatch:<agent>
//!   subagent_spawn_missing:<agent>
//!   subagent_complete_missing:<agent>
//!   subagent_pair_count_mismatch:<agent>:expected=N:got=M
//!   subagent_orchestrator_skipped:<agent>

use std::path::{Path, PathBuf};
use std::process::Command;

// ── shared helpers ────────────────────────────────────────────────────────────

/// Parse `agents_dispatched: [a, b, c]` from the artifact, preserving order and
/// duplicates. Mirrors the bash:
///   grep -E '^agents_dispatched:' | head -1
///     | sed -E 's/^agents_dispatched:[[:space:]]*\[//; s/\][[:space:]]*$//'
///   IFS=',' read -ra ...; for each: tr -d '"' / "'" / ' '
/// Returns the cleaned, non-empty agent names in order. An absent/empty list
/// yields an empty Vec (the bash returns 0 in that case, handled by the caller).
fn parse_agents_dispatched(artifact: &Path) -> Option<Vec<String>> {
    let content = std::fs::read_to_string(artifact).ok()?;
    // `grep -E '^agents_dispatched:' | head -1`: first line starting with the key.
    let line = content
        .lines()
        .find(|l| l.starts_with("agents_dispatched:"))?;

    // `sed s/^agents_dispatched:[[:space:]]*\[//` then `s/\][[:space:]]*$//`.
    let after_key = line
        .strip_prefix("agents_dispatched:")
        .unwrap_or(line)
        .trim_start_matches([' ', '\t']);
    // Strip a single leading `[`.
    let after_open = after_key.strip_prefix('[').unwrap_or(after_key);
    // Strip trailing whitespace then a single trailing `]` (the sed strips `]`
    // followed by trailing whitespace at EOL).
    let trimmed_tail = after_open.trim_end_matches([' ', '\t']);
    let inner = trimmed_tail.strip_suffix(']').unwrap_or(trimmed_tail);

    // `IFS=',' read -ra`: split on commas. Each token: remove all `"`, `'`, ` `.
    let names: Vec<String> = inner
        .split(',')
        .map(clean_name)
        .filter(|n| !n.is_empty())
        .collect();
    Some(names)
}

/// `tr -d '"' | tr -d "'" | tr -d ' '`: remove every double-quote, single-quote,
/// and space char. (Note: only ASCII space, not tabs — matches the bash `tr -d
/// ' '`.)
fn clean_name(raw: &str) -> String {
    raw.chars()
        .filter(|&c| c != '"' && c != '\'' && c != ' ')
        .collect()
}

// ── verify_event_evidence ─────────────────────────────────────────────────────

/// Outcome of one event verification, carrying the rc, optional stdout
/// diagnostic, and any soft stderr warnings (for outcome=error/timeout).
struct EventResult {
    code: i32,
    stdout: String,
    stderr: String,
}

/// `verify_event_evidence SESSION_ID NONCE EVENTS_FILE ARTIFACT_PATH`.
fn verify_event_evidence(
    session_id: &str,
    nonce: &str,
    events_file: &Path,
    artifact: &Path,
) -> EventResult {
    let mut res = EventResult {
        code: 0,
        stdout: String::new(),
        stderr: String::new(),
    };

    // rc=2: events.jsonl absent or unreadable.
    // The bash tests `[[ ! -f ]] || [[ ! -r ]]`. We approximate readability by
    // attempting to read; a missing/unreadable file -> rc=2.
    let events_content = match std::fs::read_to_string(events_file) {
        Ok(c) => c,
        Err(_) => {
            res.code = 2;
            return res;
        }
    };

    // Parse agents_dispatched; absent/empty -> rc=0 (gate passes vacuously).
    let declared = match parse_agents_dispatched(artifact) {
        Some(v) if !v.is_empty() => v,
        // raw_names empty -> `return 0`; OR zero declared after cleaning ->
        // `return 0`. A missing artifact also yields no names -> 0.
        _ => return res,
    };

    let lines: Vec<&str> = events_content.lines().collect();

    // ── Forgery check: any spawn event (this session+nonce) with an
    // agent_name NOT in declared -> agent_mismatch:<agent>, rc=1.
    for line in &lines {
        if !line.contains("\"type\":\"subagent_spawn\"") {
            continue;
        }
        if !line.contains(&format!("\"session_id\":\"{session_id}\"")) {
            continue;
        }
        if !line.contains(&format!("\"nonce\":\"{nonce}\"")) {
            continue;
        }
        let spawn_agent = extract_agent_name(line);
        if !declared.iter().any(|d| d == &spawn_agent) {
            res.stdout = format!("agent_mismatch:{spawn_agent}\n");
            res.code = 1;
            return res;
        }
    }

    // ── Build unique agents + expected counts (order-preserving).
    let mut unique: Vec<(String, usize)> = Vec::new();
    for agent in &declared {
        if let Some(entry) = unique.iter_mut().find(|(n, _)| n == agent) {
            entry.1 += 1;
        } else {
            unique.push((agent.clone(), 1));
        }
    }

    for (agent, expected) in &unique {
        // Count spawn events for this agent (session+nonce+agent_name).
        let spawn_count = lines
            .iter()
            .filter(|line| {
                line.contains("\"type\":\"subagent_spawn\"")
                    && line.contains(&format!("\"session_id\":\"{session_id}\""))
                    && line.contains(&format!("\"nonce\":\"{nonce}\""))
                    && line.contains(&format!("\"agent_name\":\"{agent}\""))
            })
            .count();

        if spawn_count == 0 {
            res.stdout = format!("subagent_spawn_missing:{agent}\n");
            res.code = 1;
            return res;
        }

        // Count complete events; orchestrator_skipped -> rc=1; soft-warn on
        // error/timeout.
        let mut complete_count = 0usize;
        for line in &lines {
            if !line.contains("\"type\":\"subagent_complete\"") {
                continue;
            }
            if !line.contains(&format!("\"session_id\":\"{session_id}\"")) {
                continue;
            }
            if !line.contains(&format!("\"nonce\":\"{nonce}\"")) {
                continue;
            }
            if !line.contains(&format!("\"agent_name\":\"{agent}\"")) {
                continue;
            }

            if line.contains("\"outcome\":\"orchestrator_skipped\"") {
                res.stdout = format!("subagent_orchestrator_skipped:{agent}\n");
                res.code = 1;
                return res;
            }

            // Soft-warn (NOT rc=1) on error/timeout. The bash uses an
            // if/elif, so error takes precedence over timeout on a line that
            // (pathologically) carries both.
            if line.contains("\"outcome\":\"error\"") {
                res.stderr.push_str(&format!(
                    "target: WARNING: subagent {agent} exited with outcome=error; downstream review may be incomplete\n"
                ));
            } else if line.contains("\"outcome\":\"timeout\"") {
                res.stderr.push_str(&format!(
                    "target: WARNING: subagent {agent} exited with outcome=timeout; downstream review may be incomplete\n"
                ));
            }

            complete_count += 1;
        }

        if complete_count == 0 {
            res.stdout = format!("subagent_complete_missing:{agent}\n");
            res.code = 1;
            return res;
        }

        if complete_count != *expected {
            res.stdout = format!(
                "subagent_pair_count_mismatch:{agent}:expected={expected}:got={complete_count}\n"
            );
            res.code = 1;
            return res;
        }
    }

    // All agents verified -> rc=0.
    res
}

/// Extract `agent_name` from an event line. Mirrors:
///   grep -oE '"agent_name":"[^"]+"' | head -1 | sed 's/^"agent_name":"//; s/"$//'
/// Returns "" when absent (the bash `_spawn_agent` would be empty, which then
/// fails the declared-set membership and triggers agent_mismatch:).
fn extract_agent_name(line: &str) -> String {
    let needle = "\"agent_name\":\"";
    let Some(start) = line.find(needle) else {
        return String::new();
    };
    let after = &line[start + needle.len()..];
    // `[^"]+` then the closing `"`.
    match after.find('"') {
        Some(end) if end > 0 => after[..end].to_string(),
        // `[^"]+` requires at least one char; an empty value doesn't match the
        // grep -oE pattern at all, so head -1 yields nothing -> "".
        _ => String::new(),
    }
}

// ── verify_child_promise ──────────────────────────────────────────────────────

/// Outcome of child-promise verification (rc + stderr diagnostic).
struct ChildPromiseResult {
    code: i32,
    stderr: String,
}

/// `verify_child_promise SESSION_ID NONCE [EVENTS_FILE]`.
/// rc=0 found+match; rc=1 missing/nonce-mismatch (stderr diag); rc=2 unreadable.
fn verify_child_promise(session_id: &str, nonce: &str, events_file: &Path) -> ChildPromiseResult {
    let mut res = ChildPromiseResult {
        code: 0,
        stderr: String::new(),
    };

    // `[[ ! -r "$events_file" ]]` -> rc=2.
    let content = match std::fs::read_to_string(events_file) {
        Ok(c) => c,
        Err(_) => {
            res.stderr = format!(
                "verify_child_promise: events file unreadable: {}\n",
                events_file.display()
            );
            res.code = 2;
            return res;
        }
    };

    // Pre-filter via grep -F '"type":"child_promise"', then select where
    // data.session_id == sid, take the first.
    //   grep -F '"type":"child_promise"' | jq -c 'select(.data.session_id==$sid)' | head -n1
    let matched_event: Option<serde_json::Value> = content
        .lines()
        .filter(|l| l.contains("\"type\":\"child_promise\""))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v.pointer("/data/session_id").and_then(|s| s.as_str()) == Some(session_id));

    let Some(event) = matched_event else {
        res.stderr = format!("child_promise missing for session {session_id}\n");
        res.code = 1;
        return res;
    };

    // `matched_nonce=$(jq -r '.data.nonce // ""')`.
    let matched_nonce = event
        .pointer("/data/nonce")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if matched_nonce != nonce {
        res.stderr = format!(
            "child_promise nonce mismatch for session {session_id} (got {matched_nonce}, expected {nonce})\n"
        );
        res.code = 1;
        return res;
    }

    res
}

// ── resolve_has_nonclaud_agent ────────────────────────────────────────────────

/// Outcome of the non-Claude resolution (rc + stderr warnings).
struct NonClaudeResult {
    code: i32,
    stderr: String,
}

/// `resolve_has_nonclaud_agent ARTIFACT_PATH [SETTINGS_FILE]`.
/// rc=0 at least one non-Claude agent; rc=1 all Claude (or unknown -> Claude);
/// rc=2 settings unavailable.
fn resolve_has_nonclaud_agent(
    artifact: &Path,
    settings_file: Option<&Path>,
    git_bin: &str,
) -> NonClaudeResult {
    let mut res = NonClaudeResult {
        code: 1,
        stderr: String::new(),
    };

    // Locate settings file if not provided: project `.fno/settings.yaml`, else
    // `$HOME/.fno/settings.yaml`.
    let settings: PathBuf = match settings_file {
        Some(p) => p.to_path_buf(),
        None => {
            let repo_root = git_show_toplevel(git_bin)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let project = repo_root.join(".fno/settings.yaml");
            if project.is_file() {
                project
            } else {
                let home = std::env::var("HOME").unwrap_or_default();
                PathBuf::from(home).join(".fno/settings.yaml")
            }
        }
    };

    // Settings absent -> rc=2.
    if !settings.is_file() {
        res.code = 2;
        return res;
    }

    let content = match std::fs::read_to_string(&settings) {
        Ok(c) => c,
        Err(_) => {
            res.code = 2;
            return res;
        }
    };

    // S3 fix: malformed YAML must NOT silently route to all-Claude. The bash
    // shells `python3 -c 'import yaml,sys; yaml.safe_load(sys.stdin)'`; on a
    // parse error it WARNs + returns rc=1. We reproduce this by invoking the
    // same python3 check, so the malformed-detection boundary is byte-identical
    // (a Rust-native YAML parser would diverge on edge cases). If python3 is
    // absent the bash skips the check entirely (`command -v python3`), so we do
    // too.
    if let Some(py) = which_python3() {
        let ok = Command::new(&py)
            .args(["-c", "import yaml,sys; yaml.safe_load(sys.stdin)"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(content.as_bytes());
                }
                child.wait()
            })
            .map(|status| status.success())
            .unwrap_or(false);
        if !ok {
            res.stderr.push_str(&format!(
                "target: WARNING: settings.yaml unparseable; verify-event-evidence falling through to existing transcript-parser path (file: {})\n",
                settings.display()
            ));
            res.code = 1;
            return res;
        }
    }

    // Parse agents_dispatched (same shape as event path, but `[[ -z ]] -> rc=1`).
    let names = match parse_agents_dispatched(artifact) {
        Some(v) if !v.is_empty() => v,
        _ => {
            res.code = 1;
            return res;
        }
    };

    // Global active provider (fallback when agent has no explicit provider).
    let global_active = parse_global_active(&content);

    let mut has_nonclaud = false;
    for name in &names {
        // name is already cleaned by parse_agents_dispatched, but the bash
        // re-cleans here too; the cleaned form is identical.
        if name.is_empty() {
            continue;
        }

        let agent_provider = parse_agent_provider(&content, name);
        let provider_id = agent_provider
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| global_active.clone().filter(|s| !s.is_empty()));
        let provider_id = match provider_id {
            Some(p) if !p.is_empty() => p,
            _ => continue, // `[[ -z "$_provider_id" ]] && continue`
        };

        let provider_cli = parse_provider_cli(&content, &provider_id);

        // Dangling reference: explicit agent override but the provider id isn't
        // in records -> WARN + skip this agent (don't infer claude).
        if provider_cli.as_deref().unwrap_or("").is_empty()
            && agent_provider
                .as_deref()
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        {
            res.stderr.push_str(&format!(
                "target: WARNING: config.agents.{}.provider='{}' references unknown provider id; ignoring this agent's pinning\n",
                name,
                agent_provider.as_deref().unwrap_or("")
            ));
            continue;
        }

        if let Some(cli) = &provider_cli {
            if !cli.is_empty() && cli != "claude" {
                has_nonclaud = true; // keep iterating to surface all warnings
            }
        }
    }

    if has_nonclaud {
        res.code = 0;
    } else {
        res.code = 1;
    }
    res
}

/// Locate python3 on PATH (mirrors `command -v python3`). None if absent.
fn which_python3() -> Option<PathBuf> {
    // `command -v python3` consults PATH. Use the same lookup the shell would.
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("python3");
        if candidate.is_file() {
            // Best-effort: assume it is executable (the bash `command -v` also
            // only checks existence-on-PATH, not the x-bit, for builtins-vs-files
            // in practice it returns the first PATH hit).
            return Some(candidate);
        }
    }
    None
}

/// Global active provider:
///   awk '/^[[:space:]]+active:/ { sub(...); print; exit }' | tr -d '"' / "'"
/// The first indented `active:` value anywhere in the file. (The bash anchors
/// on `^[[:space:]]+active:`, i.e. at least one leading space.)
fn parse_global_active(content: &str) -> Option<String> {
    for line in content.lines() {
        if !has_leading_ws(line) {
            continue;
        }
        let trimmed = line.trim_start_matches([' ', '\t']);
        if let Some(after) = trimmed.strip_prefix("active:") {
            let v = after
                .trim_start_matches([' ', '\t'])
                .trim_end_matches([' ', '\t'])
                .replace(['"', '\''], "");
            return Some(v);
        }
    }
    None
}

/// `config.agents.<name>.provider` via the bash awk:
///   /^[[:space:]]+agents:/ { in_agents=1; next }
///   in_agents && /^[[:space:]]{4}[a-zA-Z_-]/ { in_this = (key==agent); next }
///   in_this && /^[[:space:]]+provider:/ { print; exit }
///   in_agents && /^[a-zA-Z_]/ { exit }
fn parse_agent_provider(content: &str, agent: &str) -> Option<String> {
    let mut in_agents = false;
    let mut in_this = false;
    for line in content.lines() {
        // `^[[:space:]]+agents:` — indented `agents:` key.
        if has_leading_ws(line) && line.trim_start_matches([' ', '\t']).starts_with("agents:") {
            in_agents = true;
            continue;
        }
        if in_agents {
            // `^[[:space:]]{4}[a-zA-Z_-]`: exactly 4 leading spaces then a name
            // char -> an agent key line.
            if has_n_leading_spaces_then_namechar(line, 4) {
                let key = line
                    .trim_start_matches([' ', '\t'])
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .to_string();
                in_this = key == agent;
                continue;
            }
            // `in_this && /^[[:space:]]+provider:/`.
            if in_this && has_leading_ws(line) {
                let trimmed = line.trim_start_matches([' ', '\t']);
                if let Some(after) = trimmed.strip_prefix("provider:") {
                    let v = after
                        .trim_start_matches([' ', '\t'])
                        .trim_end_matches([' ', '\t'])
                        .replace(['"', '\''], "");
                    return Some(v);
                }
            }
            // `in_agents && /^[a-zA-Z_]/ { exit }`: a top-level key (col-0
            // letter/_) ends the agents block.
            if let Some(&first) = line.as_bytes().first() {
                if first.is_ascii_alphabetic() || first == b'_' {
                    return None;
                }
            }
        }
    }
    None
}

/// `providers.records.<pid>.cli` via the bash awk:
///   /^[[:space:]]+records:/ { in_records=1; next }
///   in_records && /^[[:space:]]{6}[a-zA-Z_-]/ { in_this = (key==pid); next }
///   in_this && /^[[:space:]]+cli:/ { print; exit }
///   in_records && /^[[:space:]]{0,4}[a-zA-Z_]/ { in_records=0 }
fn parse_provider_cli(content: &str, pid: &str) -> Option<String> {
    let mut in_records = false;
    let mut in_this = false;
    for line in content.lines() {
        if has_leading_ws(line) && line.trim_start_matches([' ', '\t']).starts_with("records:") {
            in_records = true;
            continue;
        }
        if in_records {
            // `^[[:space:]]{6}[a-zA-Z_-]`: exactly 6 leading spaces then a name
            // char -> a provider-id key line.
            if has_n_leading_spaces_then_namechar(line, 6) {
                let key = line
                    .trim_start_matches([' ', '\t'])
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .to_string();
                in_this = key == pid;
                continue;
            }
            // `in_this && /^[[:space:]]+cli:/`.
            if in_this && has_leading_ws(line) {
                let trimmed = line.trim_start_matches([' ', '\t']);
                if let Some(after) = trimmed.strip_prefix("cli:") {
                    let v = after
                        .trim_start_matches([' ', '\t'])
                        .trim_end_matches([' ', '\t'])
                        .replace(['"', '\''], "");
                    return Some(v);
                }
            }
            // `in_records && /^[[:space:]]{0,4}[a-zA-Z_]/ { in_records=0 }`:
            // 0..=4 leading spaces then a letter/_ ends the records block.
            // Note: this rule has NO `next`, so the bash continues to evaluate
            // the same line afterwards — but since the only later rules are
            // guarded by in_records (now false) or in_this, the practical effect
            // is to end the block. We replicate by clearing in_records and
            // continuing.
            if has_0_to_n_leading_spaces_then_namechar(line, 4) {
                in_records = false;
            }
        }
    }
    None
}

fn has_leading_ws(line: &str) -> bool {
    matches!(line.as_bytes().first(), Some(b' ') | Some(b'\t'))
}

/// `^[[:space:]]{N}[a-zA-Z_-]`: exactly N leading SPACES, then a name char.
/// The bash uses `{4}` / `{6}` with `[[:space:]]` but in practice indentation is
/// spaces; we match exactly N spaces then the name-char class `[a-zA-Z_-]`.
fn has_n_leading_spaces_then_namechar(line: &str, n: usize) -> bool {
    let bytes = line.as_bytes();
    if bytes.len() <= n {
        return false;
    }
    for &b in &bytes[..n] {
        if b != b' ' {
            return false;
        }
    }
    // The char at position n must NOT be a space (else it's deeper indent), and
    // must be in `[a-zA-Z_-]`.
    let c = bytes[n];
    is_name_char(c)
}

/// `^[[:space:]]{0,4}[a-zA-Z_]`: 0..=N leading spaces, then a letter/underscore.
fn has_0_to_n_leading_spaces_then_namechar(line: &str, n: usize) -> bool {
    let bytes = line.as_bytes();
    let mut spaces = 0;
    while spaces < bytes.len() && bytes[spaces] == b' ' {
        spaces += 1;
    }
    if spaces > n {
        return false;
    }
    match bytes.get(spaces) {
        Some(&c) => c.is_ascii_alphabetic() || c == b'_',
        None => false,
    }
}

fn is_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

fn git_show_toplevel(git_bin: &str) -> Option<PathBuf> {
    let out = Command::new(git_bin)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

// ── public dispatch entry ─────────────────────────────────────────────────────

/// Internal: run the requested sub-verb, returning (code, stdout, stderr).
fn run(args: &[String]) -> (i32, String, String) {
    let git_bin = std::env::var("FNO_VERIFY_GIT_BIN").unwrap_or_else(|_| "git".to_string());

    let Some(sub) = args.first().map(|s| s.as_str()) else {
        return (
            2,
            String::new(),
            "verify-evidence: missing subcommand (event|child-promise|has-nonclaude)\n".to_string(),
        );
    };
    let rest = &args[1..];

    match sub {
        "event" => {
            // event SID NONCE EVENTS ARTIFACT
            if rest.len() < 4 {
                return (
                    2,
                    String::new(),
                    "verify-evidence event: requires SESSION_ID NONCE EVENTS_FILE ARTIFACT_PATH\n"
                        .to_string(),
                );
            }
            let r =
                verify_event_evidence(&rest[0], &rest[1], Path::new(&rest[2]), Path::new(&rest[3]));
            (r.code, r.stdout, r.stderr)
        }
        "child-promise" => {
            // child-promise SID NONCE [EVENTS]
            if rest.len() < 2 {
                return (
                    2,
                    String::new(),
                    "verify-evidence child-promise: requires SESSION_ID NONCE [EVENTS_FILE]\n"
                        .to_string(),
                );
            }
            // Default events file mirrors bash:
            //   ${3:-${EVENTS_FILE:-.fno/events.jsonl}}
            let events = rest.get(2).cloned().unwrap_or_else(|| {
                std::env::var("EVENTS_FILE").unwrap_or_else(|_| ".fno/events.jsonl".to_string())
            });
            let r = verify_child_promise(&rest[0], &rest[1], Path::new(&events));
            (r.code, String::new(), r.stderr)
        }
        "has-nonclaude" => {
            // has-nonclaude ARTIFACT [SETTINGS]
            if rest.is_empty() {
                return (
                    2,
                    String::new(),
                    "verify-evidence has-nonclaude: requires ARTIFACT_PATH [SETTINGS_FILE]\n"
                        .to_string(),
                );
            }
            let settings = rest.get(1).map(PathBuf::from);
            let r = resolve_has_nonclaud_agent(Path::new(&rest[0]), settings.as_deref(), &git_bin);
            (r.code, String::new(), r.stderr)
        }
        other => (
            2,
            String::new(),
            format!("verify-evidence: unknown subcommand: {other}\n"),
        ),
    }
}

/// Print stdout/stderr and return the exit code. Used by `bin/client.rs`.
pub fn run_verify_evidence(args: &[String]) -> i32 {
    let (code, stdout, stderr) = run(args);
    if !stdout.is_empty() {
        print!("{stdout}");
    }
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }
    code
}

/// Test-friendly variant: returns (exit_code, stdout, stderr) without printing.
pub fn run_verify_evidence_capture(args: &[String]) -> (i32, String, String) {
    run(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_name_strips_quotes_and_spaces() {
        assert_eq!(clean_name(" \"foo\" "), "foo");
        assert_eq!(clean_name("'bar'"), "bar");
        assert_eq!(clean_name("baz"), "baz");
    }

    #[test]
    fn extract_agent_name_basic() {
        let line = r#"{"type":"subagent_spawn","agent_name":"code-reviewer","session_id":"s"}"#;
        assert_eq!(extract_agent_name(line), "code-reviewer");
        assert_eq!(extract_agent_name("{}"), "");
    }

    #[test]
    fn n_leading_spaces_namechar() {
        assert!(has_n_leading_spaces_then_namechar("    reviewer:", 4));
        assert!(!has_n_leading_spaces_then_namechar("      reviewer:", 4)); // 6 spaces
        assert!(!has_n_leading_spaces_then_namechar("  reviewer:", 4)); // 2 spaces
        assert!(has_n_leading_spaces_then_namechar("      codex-prov:", 6));
    }

    #[test]
    fn global_active_extraction() {
        let yaml = "config:\n  providers:\n    active: claude-main\n";
        assert_eq!(parse_global_active(yaml).as_deref(), Some("claude-main"));
    }

    #[test]
    fn agent_provider_and_cli_lookup() {
        let yaml = "config:\n  agents:\n    reviewer:\n      provider: codex-prov\n  providers:\n    records:\n      codex-prov:\n        cli: codex\n    active: claude-main\n";
        assert_eq!(
            parse_agent_provider(yaml, "reviewer").as_deref(),
            Some("codex-prov")
        );
        assert_eq!(parse_agent_provider(yaml, "other"), None);
        assert_eq!(
            parse_provider_cli(yaml, "codex-prov").as_deref(),
            Some("codex")
        );
        assert_eq!(parse_provider_cli(yaml, "nonexistent"), None);
    }
}
