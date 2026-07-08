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
            let project = repo_root.join(".fno/config.toml");
            if project.is_file() {
                project
            } else {
                let home = std::env::var("HOME").unwrap_or_default();
                PathBuf::from(home).join(".fno/config.toml")
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

    // S3 fix: a malformed config must NOT silently route to all-Claude. Parse
    // the flat config.toml; on a parse error, WARN + rc=1 so the caller falls
    // through to the transcript-parser path rather than inferring all-Claude.
    if cfg_table(&content).is_none() {
        res.stderr.push_str(&format!(
            "target: WARNING: config.toml unparseable; verify-event-evidence falling through to existing transcript-parser path (file: {})\n",
            settings.display()
        ));
        res.code = 1;
        return res;
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

/// Parse a flat config.toml body into a table; None on parse error.
fn cfg_table(content: &str) -> Option<toml::Table> {
    content.parse::<toml::Table>().ok()
}

/// Global active provider: flat `providers.active`.
fn parse_global_active(content: &str) -> Option<String> {
    cfg_table(content)?
        .get("providers")?
        .as_table()?
        .get("active")?
        .as_str()
        .map(str::to_string)
}

/// `agents.<name>.provider` from a flat config.toml.
fn parse_agent_provider(content: &str, agent: &str) -> Option<String> {
    cfg_table(content)?
        .get("agents")?
        .as_table()?
        .get(agent)?
        .as_table()?
        .get("provider")?
        .as_str()
        .map(str::to_string)
}

/// `cli` for the record whose `id == pid`, from a flat config.toml.
///
/// save_providers() serializes `providers.records` from a Python list, so it is
/// a TOML array-of-tables (`[[providers.records]]` with an `id` field), NOT a
/// table keyed by provider id. Reading it as a keyed table made `as_table()`
/// return None for every real config, so codex/gemini providers went undetected
/// and the non-Claude evidence path was skipped (codex P2).
fn parse_provider_cli(content: &str, pid: &str) -> Option<String> {
    let table = cfg_table(content)?;
    let records = table
        .get("providers")?
        .as_table()?
        .get("records")?
        .as_array()?;
    records.iter().find_map(|rec| {
        let t = rec.as_table()?;
        if t.get("id").and_then(|v| v.as_str()) == Some(pid) {
            t.get("cli").and_then(|v| v.as_str()).map(str::to_string)
        } else {
            None
        }
    })
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
    fn global_active_extraction() {
        let cfg = "[providers]\nactive = \"claude-main\"\n";
        assert_eq!(parse_global_active(cfg).as_deref(), Some("claude-main"));
    }

    #[test]
    fn agent_provider_and_cli_lookup() {
        let cfg = "[agents.reviewer]\nprovider = \"codex-prov\"\n\n[providers]\nactive = \"claude-main\"\n\n[[providers.records]]\nid = \"codex-prov\"\ncli = \"codex\"\n";
        assert_eq!(
            parse_agent_provider(cfg, "reviewer").as_deref(),
            Some("codex-prov")
        );
        assert_eq!(parse_agent_provider(cfg, "other"), None);
        assert_eq!(
            parse_provider_cli(cfg, "codex-prov").as_deref(),
            Some("codex")
        );
        assert_eq!(parse_provider_cli(cfg, "nonexistent"), None);
    }
}
