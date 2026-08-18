//! `fno-agents verify-evidence` verb — Rust port of
//! `scripts/lib/verify-event-evidence.sh` (packaging EPIC ab-8bdb4642,
//! eliminate-don't-vendor leg).
//!
//! Three sub-functions are exposed via a leading sub-token:
//!   - `verify-evidence child-promise SID NONCE [EVENTS]`
//!   - `verify-evidence has-nonclaude ARTIFACT [SETTINGS]`
//!   - `verify-evidence receipt CANDIDATE_SHA CANONICAL_EVENTS [MIRROR_EVENTS...]`
//!
//! Each reproduces the bash exit-code contract, stdout diagnostic `kind`
//! strings, and stderr soft-warnings byte-for-byte. The bash script stays
//! in-tree as the parity oracle (differential tests in
//! `tests/verify_evidence_parity.rs`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{json, Value};

// Mirrors cli/src/fno/events/schema.yaml limits.
const MAX_RECEIPT_DATA_BYTES: usize = 65_536;
const DATA_SIZE_ENCODING: &str = "compact-json-ascii-v1";

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

/// The accounts block: canonical `accounts`, else the pre-rename `providers`.
///
/// The Rust side has its own config readers that never pass through the Python
/// loader's `_extract_accounts_block`, so the rename needs the fallback here
/// too. It matters more here than it looks: every accessor below is an Option
/// chain, so reading only one spelling degrades to `None` SILENTLY - a config
/// in the other spelling would read as "no accounts configured" rather than
/// as an error.
fn accounts_block(table: &toml::Table) -> Option<&toml::Table> {
    table
        .get("accounts")
        .or_else(|| table.get("providers"))?
        .as_table()
}

/// Global active account: flat `accounts.active` (pre-rename `providers.active`).
fn parse_global_active(content: &str) -> Option<String> {
    accounts_block(&cfg_table(content)?)?
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

/// `harness` for the record whose `id == pid`, from a flat config.toml.
///
/// save_providers() serializes `accounts.records` from a Python list, so it is
/// a TOML array-of-tables (`[[accounts.records]]` with an `id` field), NOT a
/// table keyed by account id. Reading it as a keyed table made `as_table()`
/// return None for every real config, so codex/gemini accounts went undetected
/// and the non-Claude evidence path was skipped (codex P2).
///
/// Both the block and the field were renamed, so both need the fallback: a
/// config can carry `[[providers.records]]` with `cli` until its first write.
fn parse_provider_cli(content: &str, pid: &str) -> Option<String> {
    let table = cfg_table(content)?;
    let records = accounts_block(&table)?.get("records")?.as_array()?;
    records.iter().find_map(|rec| {
        let t = rec.as_table()?;
        if t.get("id").and_then(|v| v.as_str()) == Some(pid) {
            t.get("harness")
                .or_else(|| t.get("cli"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
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

fn full_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn bounded_strings(value: &Value, max_items: usize, max_len: usize) -> bool {
    let Some(items) = value.as_array() else {
        return false;
    };
    !items.is_empty()
        && items.len() <= max_items
        && items.iter().all(|item| {
            item.as_str()
                .is_some_and(|text| !text.is_empty() && text.len() <= max_len)
        })
}

fn required_strings(value: &Value, fields: &[&str]) -> bool {
    value.as_object().is_some()
        && fields.iter().all(|field| {
            value
                .get(field)
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
        })
}

fn nonnegative_integer(value: &Value) -> Option<f64> {
    let number = value.as_f64()?;
    (number.is_finite() && number >= 0.0 && number.fract() == 0.0).then_some(number)
}

fn receipt_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    static UTC_TIMESTAMP: OnceLock<Regex> = OnceLock::new();
    let pattern = UTC_TIMESTAMP.get_or_init(|| {
        Regex::new(
            r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-5][0-9](?:\.[0-9]{1,6})?(?:Z|\+00:00)$",
        )
        .expect("receipt timestamp regex is valid")
    });
    let raw = value.as_str()?;
    if !pattern.is_match(raw) || raw.starts_with("0000-") {
        return None;
    }
    let without_zone = raw
        .strip_suffix('Z')
        .or_else(|| raw.strip_suffix("+00:00"))?;
    if let Some((_, fraction)) = without_zone.rsplit_once('.') {
        if fraction.is_empty()
            || fraction.len() > 6
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
    }
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .filter(|parsed| parsed.offset().local_minus_utc() == 0)
        .map(|parsed| parsed.with_timezone(&Utc))
}

fn compact_ascii_json_len(value: &Value) -> Option<usize> {
    if DATA_SIZE_ENCODING != "compact-json-ascii-v1" {
        return None;
    }
    serde_json::to_string(value).ok().map(|encoded| {
        encoded
            .chars()
            .map(|ch| {
                if ch == '\u{7f}' {
                    6
                } else if !ch.is_ascii() {
                    if u32::from(ch) <= 0xffff {
                        6
                    } else {
                        12
                    }
                } else {
                    1
                }
            })
            .sum()
    })
}

fn valid_receipt(event: &Value) -> bool {
    let Some(root) = event.as_object() else {
        return false;
    };
    if root.get("type").and_then(Value::as_str) != Some("verification_receipt")
        || !matches!(
            root.get("source").and_then(Value::as_str),
            Some("target" | "hook" | "test")
        )
        || receipt_timestamp(root.get("ts").unwrap_or(&Value::Null)).is_none()
    {
        return false;
    }
    let Some(data) = root.get("data") else {
        return false;
    };
    if compact_ascii_json_len(data)
        .map(|encoded_len| encoded_len > MAX_RECEIPT_DATA_BYTES)
        .unwrap_or(true)
    {
        return false;
    }
    let Some(candidate) = data.get("candidate_sha").and_then(Value::as_str) else {
        return false;
    };
    let mode = data.get("mode").and_then(Value::as_str).unwrap_or("");
    let result = data.get("result").and_then(Value::as_str).unwrap_or("");
    let started = data.get("started_at").and_then(receipt_timestamp);
    let finished = data.get("finished_at").and_then(receipt_timestamp);
    let Some(expected) = data.get("steps_expected").and_then(nonnegative_integer) else {
        return false;
    };
    let Some(executed) = data.get("steps_executed").and_then(nonnegative_integer) else {
        return false;
    };
    let Some(generation) = data.get("generation").and_then(nonnegative_integer) else {
        return false;
    };
    let scope_len = data
        .get("scope")
        .and_then(Value::as_array)
        .map(|scope| scope.len() as f64);
    full_sha(candidate)
        && bounded_strings(data.get("command").unwrap_or(&Value::Null), 4096, 4096)
        && bounded_strings(data.get("scope").unwrap_or(&Value::Null), 128, 512)
        && required_strings(
            data.get("environment").unwrap_or(&Value::Null),
            &["host", "platform", "runner"],
        )
        && required_strings(
            data.get("producer").unwrap_or(&Value::Null),
            &["kind", "id"],
        )
        && started.is_some()
        && finished.is_some()
        && finished >= started
        && matches!(mode, "full" | "subset" | "void" | "advisory")
        && matches!(
            result,
            "not_configured" | "unavailable" | "pending" | "failed" | "passed" | "stale"
        )
        && executed <= expected
        && (1.0..=9_007_199_254_740_991.0).contains(&generation)
        && scope_len == Some(expected)
        && (mode != "full" || result != "passed" || (expected > 0.0 && executed == expected))
        && (mode != "void" || result != "passed")
}

fn gate_eligible_receipt(event: &Value) -> bool {
    const BASE_SCOPE: [&str; 5] = [
        "smoke",
        "rustfmt:fno-agents",
        "rustfmt:fno",
        "cargo-test:fno-agents",
        "cargo-test:fno",
    ];
    let Some(data) = event.get("data") else {
        return false;
    };
    let command_path = data
        .get("command")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(Value::as_str)
        .unwrap_or("");
    let environment = data.get("environment").unwrap_or(&Value::Null);
    let producer = data.get("producer").unwrap_or(&Value::Null);
    let scope = data.get("scope").and_then(Value::as_array);
    event.get("source").and_then(Value::as_str) == Some("target")
        && producer.get("kind").and_then(Value::as_str) == Some("preflight")
        && producer
            .get("id")
            .and_then(Value::as_str)
            .zip(environment.get("host").and_then(Value::as_str))
            .is_some_and(|(id, host)| id.starts_with(&format!("{host}:")))
        && environment.get("runner").and_then(Value::as_str) == Some("scripts/ci/preflight.sh")
        && (command_path == "scripts/ci/preflight.sh"
            || command_path.ends_with("/scripts/ci/preflight.sh"))
        && scope.is_some_and(|items| {
            matches!(items.len(), 5 | 6)
                && BASE_SCOPE
                    .iter()
                    .all(|expected| items.iter().any(|item| item.as_str() == Some(expected)))
                && (items.len() == 5
                    || items
                        .iter()
                        .any(|item| item.as_str() == Some("squads-leak-guard:fno")))
        })
}

fn receipt_decision_all(candidate_sha: &str, paths: &[String]) -> Value {
    let mut seen = std::collections::HashSet::new();
    let mut receipts: Vec<(DateTime<Utc>, String, Value)> = Vec::new();
    let mut malformed = 0u64;
    let mut unreadable = 0u64;
    for path in paths {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        for line in content
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            let event: Value = match serde_json::from_str(line) {
                Ok(event) => event,
                Err(_) => {
                    malformed += 1;
                    continue;
                }
            };
            if event.get("type").and_then(Value::as_str) != Some("verification_receipt") {
                continue;
            }
            if !valid_receipt(&event) {
                malformed += 1;
                continue;
            }
            let ts = receipt_timestamp(event.get("ts").unwrap()).unwrap();
            if ts > Utc::now() {
                malformed += 1;
                continue;
            }
            let signature = serde_json::to_string(&event).unwrap_or_default();
            if seen.insert(signature.clone()) {
                receipts.push((ts, signature, event));
            }
        }
    }
    receipts.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let mut coverage = json!({
        "complete": malformed == 0 && unreadable == 0,
        "malformed_lines": malformed,
        "unreadable_paths": unreadable,
        "deduped_events": receipts.len(),
    });
    let exact: Vec<&(DateTime<Utc>, String, Value)> = receipts
        .iter()
        .filter(|(_, _, event)| {
            event
                .pointer("/data/candidate_sha")
                .and_then(Value::as_str)
                .is_some_and(|receipt_sha| receipt_sha.eq_ignore_ascii_case(candidate_sha))
        })
        .collect();
    if !exact.is_empty() {
        let newest_generation = exact
            .iter()
            .filter_map(|(_, _, event)| event.pointer("/data/generation").and_then(Value::as_f64))
            .fold(0.0_f64, f64::max);
        let newest: Vec<&&(DateTime<Utc>, String, Value)> = exact
            .iter()
            .filter(|(_, _, event)| {
                event.pointer("/data/generation").and_then(Value::as_f64) == Some(newest_generation)
            })
            .collect();
        if newest.len() != 1 {
            coverage["complete"] = Value::Bool(false);
            coverage["conflicting_latest"] = json!(newest.len());
            return json!({
                "satisfied": false,
                "mode": Value::Null,
                "result": "unavailable",
                "receipt": Value::Null,
                "coverage": coverage,
            });
        }
        let event = &newest[0].2;
        let mode = event
            .pointer("/data/mode")
            .and_then(Value::as_str)
            .unwrap_or("");
        let result = event
            .pointer("/data/result")
            .and_then(Value::as_str)
            .unwrap_or("");
        return json!({
            "satisfied": malformed == 0
                && unreadable == 0
                && mode == "full"
                && result == "passed"
                && gate_eligible_receipt(event),
            "mode": mode,
            "result": result,
            "receipt": event,
            "coverage": coverage,
        });
    }
    if let Some((_, _, event)) = receipts.last() {
        return json!({
            "satisfied": false,
            "mode": event.pointer("/data/mode").and_then(Value::as_str),
            "result": "stale",
            "receipt": event,
            "coverage": coverage,
        });
    }
    json!({
        "satisfied": false,
        "mode": Value::Null,
        "result": "unavailable",
        "receipt": Value::Null,
        "coverage": coverage,
    })
}

fn receipt_decision(candidate_sha: &str, paths: &[String]) -> Value {
    let Some(canonical_path) = paths.first() else {
        return receipt_decision_all(candidate_sha, paths);
    };
    let mut canonical = receipt_decision_all(candidate_sha, std::slice::from_ref(canonical_path));
    if canonical.pointer("/coverage/conflicting_latest").is_some() {
        return canonical;
    }
    let exact = canonical
        .pointer("/receipt/data/candidate_sha")
        .and_then(Value::as_str)
        .is_some_and(|receipt_sha| receipt_sha.eq_ignore_ascii_case(candidate_sha));
    if !exact {
        canonical["coverage"]["canonical_required"] = Value::Bool(true);
        canonical["satisfied"] = Value::Bool(false);
        return canonical;
    }

    let mut readable = vec![canonical_path.clone()];
    let mut unavailable_mirrors = 0u64;
    for path in &paths[1..] {
        match std::fs::read_to_string(path) {
            Ok(_) => readable.push(path.clone()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => unavailable_mirrors += 1,
        }
    }
    let mut combined = receipt_decision_all(candidate_sha, &readable);
    let canonical_generation = canonical
        .pointer("/receipt/data/generation")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let combined_generation = combined
        .pointer("/receipt/data/generation")
        .and_then(Value::as_f64);
    if combined.pointer("/coverage/conflicting_latest").is_some()
        || combined_generation.is_some_and(|generation| generation > canonical_generation)
    {
        combined["satisfied"] = Value::Bool(false);
        combined["mode"] = Value::Null;
        combined["result"] = Value::String("unavailable".to_string());
        combined["receipt"] = Value::Null;
        combined["coverage"]["mirror_ahead"] = Value::Bool(true);
        if unavailable_mirrors > 0 {
            combined["coverage"]["unavailable_mirrors"] = json!(unavailable_mirrors);
        }
        return combined;
    }
    canonical["coverage"] = combined["coverage"].clone();
    if combined["coverage"]["complete"] != Value::Bool(true) {
        canonical["satisfied"] = Value::Bool(false);
    }
    if unavailable_mirrors > 0 {
        canonical["coverage"]["unavailable_mirrors"] = json!(unavailable_mirrors);
    }
    canonical
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkflowState {
    Present,
    Absent,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostedCiResult {
    NotConfigured,
    Unavailable,
    Pending,
    Failed,
    Passed,
    Stale,
}

#[cfg(test)]
impl HostedCiResult {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Unavailable => "unavailable",
            Self::Pending => "pending",
            Self::Failed => "failed",
            Self::Passed => "passed",
            Self::Stale => "stale",
        }
    }
}

pub(crate) fn hosted_workflow_state(cwd: &Path) -> WorkflowState {
    let workflows = cwd.join(".github/workflows");
    let metadata = match std::fs::metadata(&workflows) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return WorkflowState::Absent;
        }
        Err(_) => return WorkflowState::Unavailable,
    };
    if !metadata.is_dir() {
        return WorkflowState::Unavailable;
    }
    let entries = match std::fs::read_dir(workflows) {
        Ok(entries) => entries,
        Err(_) => return WorkflowState::Unavailable,
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return WorkflowState::Unavailable;
        };
        let path = entry.path();
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("yml") || extension.eq_ignore_ascii_case("yaml")
            })
        {
            match std::fs::metadata(path) {
                Ok(metadata) if metadata.is_file() => return WorkflowState::Present,
                Ok(_) => continue,
                Err(_) => return WorkflowState::Unavailable,
            }
        }
    }
    WorkflowState::Absent
}

fn hosted_ci_result(
    declared_none: bool,
    workflow_state: WorkflowState,
    candidate_sha: &str,
    observed_sha: Option<&str>,
    checks: Option<&Value>,
) -> HostedCiResult {
    if !full_sha(candidate_sha) || workflow_state == WorkflowState::Unavailable {
        return HostedCiResult::Unavailable;
    }
    let Some(checks) = checks.and_then(Value::as_array) else {
        return HostedCiResult::Unavailable;
    };
    if !checks.is_empty() && observed_sha.is_none() {
        return HostedCiResult::Unavailable;
    }
    if let Some(observed) = observed_sha {
        if !full_sha(observed) {
            return HostedCiResult::Unavailable;
        }
        if !observed.eq_ignore_ascii_case(candidate_sha) {
            return HostedCiResult::Stale;
        }
    }
    let mut failed = false;
    let mut pending = false;
    for check in checks {
        if !check.is_object() {
            return HostedCiResult::Unavailable;
        }
        let bucket = check
            .get("bucket")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let status = check
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_uppercase();
        let conclusion = check
            .get("conclusion")
            .or_else(|| check.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_uppercase();
        if matches!(bucket.as_str(), "fail" | "cancel")
            || matches!(
                conclusion.as_str(),
                "FAILURE"
                    | "TIMED_OUT"
                    | "CANCELLED"
                    | "ACTION_REQUIRED"
                    | "STARTUP_FAILURE"
                    | "STALE"
                    | "ERROR"
            )
        {
            failed = true;
        } else if !matches!(bucket.as_str(), "pass" | "skipping")
            && !((status.is_empty() || status == "COMPLETED")
                && matches!(conclusion.as_str(), "SUCCESS" | "NEUTRAL" | "SKIPPED"))
        {
            pending = true;
        }
    }
    if failed {
        HostedCiResult::Failed
    } else if pending {
        HostedCiResult::Pending
    } else if !checks.is_empty() {
        HostedCiResult::Passed
    } else if declared_none && workflow_state == WorkflowState::Absent {
        HostedCiResult::NotConfigured
    } else {
        HostedCiResult::Pending
    }
}

pub(crate) fn hosted_ci_not_configured(
    declared_none: bool,
    cwd: &Path,
    candidate_sha: &str,
) -> bool {
    hosted_ci_result(
        declared_none,
        hosted_workflow_state(cwd),
        candidate_sha,
        None,
        Some(&json!([])),
    ) == HostedCiResult::NotConfigured
}

// ── public dispatch entry ─────────────────────────────────────────────────────

/// Internal: run the requested sub-verb, returning (code, stdout, stderr).
fn run(args: &[String]) -> (i32, String, String) {
    let git_bin = std::env::var("FNO_VERIFY_GIT_BIN").unwrap_or_else(|_| "git".to_string());

    let Some(sub) = args.first().map(|s| s.as_str()) else {
        return (
            2,
            String::new(),
            "verify-evidence: missing subcommand (child-promise|has-nonclaude|receipt)\n"
                .to_string(),
        );
    };
    let rest = &args[1..];

    match sub {
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
        "receipt" => {
            if rest.len() < 2 || !full_sha(&rest[0]) {
                return (
                    2,
                    String::new(),
                    "verify-evidence receipt: requires full CANDIDATE_SHA CANONICAL_EVENTS [MIRROR_EVENTS...]\n"
                        .to_string(),
                );
            }
            // The journal is append-only, so a read excludes nobody: writers
            // append single lines without holding any lock. Taking the
            // preflight writer lock here made every fleet verification
            // unavailable for the duration of any preflight run.
            let decision = receipt_decision(&rest[0], &rest[1..]);
            let code = if decision["satisfied"] == Value::Bool(true) {
                0
            } else {
                1
            };
            (code, format!("{decision}\n"), String::new())
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
    use std::io::Write;

    #[test]
    fn clean_name_strips_quotes_and_spaces() {
        assert_eq!(clean_name(" \"foo\" "), "foo");
        assert_eq!(clean_name("'bar'"), "bar");
        assert_eq!(clean_name("baz"), "baz");
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

    #[test]
    fn canonical_accounts_block_and_harness_field_resolve_identically() {
        // Same fixture as the two tests above in the post-rename spelling. Both
        // the block (`providers` -> `accounts`) and the record field (`cli` ->
        // `harness`) moved, and this reader parses the config.toml directly
        // rather than through the Python loader, so it needs its own fallback.
        // Every accessor here is an Option chain: an unknown spelling reads as
        // None, which is indistinguishable from "not configured".
        let cfg = "[agents.reviewer]\nprovider = \"codex-prov\"\n\n[accounts]\nactive = \"claude-main\"\n\n[[accounts.records]]\nid = \"codex-prov\"\nharness = \"codex\"\n";
        assert_eq!(parse_global_active(cfg).as_deref(), Some("claude-main"));
        assert_eq!(
            parse_provider_cli(cfg, "codex-prov").as_deref(),
            Some("codex")
        );
        assert_eq!(parse_provider_cli(cfg, "nonexistent"), None);
    }

    #[test]
    fn receipt_validation_rejects_zero_step_full_pass() {
        let event = json!({
            "ts": "2026-07-26T01:00:00Z",
            "type": "verification_receipt",
            "source": "target",
            "data": {
                "candidate_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "command": ["preflight"],
                "environment": {"host": "h", "platform": "p", "runner": "r"},
                "scope": ["smoke"],
                "started_at": "2026-07-26T01:00:00Z",
                "finished_at": "2026-07-26T01:00:01Z",
                "mode": "full",
                "result": "passed",
                "producer": {"kind": "preflight", "id": "h:1"},
                "generation": 1,
                "steps_expected": 0,
                "steps_executed": 0
            }
        });
        assert!(!valid_receipt(&event));
    }

    #[test]
    fn receipt_validation_rejects_excessive_command_arguments() {
        let mut event = receipt_event(
            "2026-07-26T01:00:00Z",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "full",
            "passed",
        );
        event["data"]["command"] =
            Value::Array((0..4097).map(|_| Value::String("x".to_string())).collect());
        assert!(!valid_receipt(&event));
    }

    #[test]
    fn receipt_validation_requires_positive_generation() {
        let mut event = receipt_event(
            "2026-07-26T01:00:00Z",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "full",
            "passed",
        );
        event["data"]["generation"] = json!(0);
        assert!(!valid_receipt(&event));
        event["data"]["generation"] = json!(9_007_199_254_740_991_u64);
        assert!(valid_receipt(&event));
        event["data"]["generation"] = json!(9_007_199_254_740_992_u64);
        assert!(!valid_receipt(&event));
    }

    #[test]
    fn receipt_validation_requires_canonical_calendar_timestamps() {
        for invalid in [
            "2026-07-26 01:00:00+00:00",
            "2023-02-29T00:00:00Z",
            "2016-12-31T23:59:60Z",
            "0000-01-01T00:00:00Z",
        ] {
            for pointer in ["/ts", "/data/started_at", "/data/finished_at"] {
                let mut event = receipt_event(
                    "2026-07-26T01:00:00Z",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "full",
                    "passed",
                );
                *event.pointer_mut(pointer).unwrap() = json!(invalid);
                assert!(
                    !valid_receipt(&event),
                    "{pointer} accepted invalid timestamp {invalid}"
                );
            }
        }
    }

    #[test]
    fn receipt_validation_rejects_oversized_data() {
        let mut event = receipt_event(
            "2026-07-26T01:00:00Z",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "full",
            "passed",
        );
        event["data"]["detail"] = Value::String("x".repeat(70_000));

        assert!(!valid_receipt(&event));
    }

    #[test]
    fn receipt_validation_counts_compact_ascii_json_bytes() {
        for (detail, expected) in [
            ("é".repeat(10_000), true),
            ("é".repeat(11_000), false),
            ("\u{7f}".repeat(10_000), true),
            ("\u{7f}".repeat(11_000), false),
        ] {
            let mut event = receipt_event(
                "2026-07-26T01:00:00Z",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "full",
                "passed",
            );
            event["data"]["detail"] = Value::String(detail);

            assert_eq!(valid_receipt(&event), expected);
        }
    }

    #[test]
    fn receipt_validation_accepts_pre_epoch_utc_timestamps() {
        for timestamp in ["0001-01-01T00:00:00Z", "1969-12-31T23:59:59.123456Z"] {
            let mut event = receipt_event(
                timestamp,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "full",
                "passed",
            );
            event["data"]["started_at"] = json!(timestamp);
            event["data"]["finished_at"] = json!(timestamp);

            assert!(
                valid_receipt(&event),
                "rejected valid pre-epoch timestamp {timestamp}"
            );
        }
    }

    fn receipt_event(ts: &str, candidate_sha: &str, mode: &str, result: &str) -> Value {
        json!({
            "ts": ts,
            "type": "verification_receipt",
            "source": "target",
            "data": {
                "candidate_sha": candidate_sha,
                "command": ["scripts/ci/preflight.sh", "--force"],
                "environment": {"host": "h", "platform": "p", "runner": "scripts/ci/preflight.sh"},
                "scope": [
                    "smoke",
                    "rustfmt:fno-agents",
                    "rustfmt:fno",
                    "cargo-test:fno-agents",
                    "cargo-test:fno",
                    "squads-leak-guard:fno"
                ],
                "started_at": "2026-07-26T01:00:00Z",
                "finished_at": "2026-07-26T01:00:01Z",
                "mode": mode,
                "result": result,
                "producer": {"kind": "preflight", "id": "h:1"},
                "generation": 1,
                "steps_expected": 6,
                "steps_executed": 6
            }
        })
    }

    #[test]
    fn receipt_decision_dedupes_unordered_journals_and_selects_parsed_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("global.jsonl");
        let second = dir.path().join("delivery.jsonl");
        let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let failed = receipt_event("2026-07-26T01:00:00Z", sha, "full", "failed");
        let mut passed = receipt_event("2026-07-26T03:00:00+00:00", sha, "full", "passed");
        passed["data"]["generation"] = json!(2);
        std::fs::write(&first, format!("{passed}\n{failed}\n")).unwrap();
        std::fs::write(&second, format!("{failed}\n{passed}\n")).unwrap();

        let decision = receipt_decision(
            &sha.to_ascii_uppercase(),
            &[first.display().to_string(), second.display().to_string()],
        );

        assert_eq!(decision["satisfied"], true);
        assert_eq!(decision["coverage"]["deduped_events"], 2);
        assert_eq!(decision["receipt"]["ts"], "2026-07-26T03:00:00+00:00");
    }

    #[test]
    fn receipt_generation_supersedes_timestamp_after_clock_rollback() {
        let mut journal = tempfile::NamedTempFile::new().unwrap();
        let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let passed = receipt_event("2026-07-26T03:00:00Z", sha, "full", "passed");
        let mut failed = receipt_event("2026-07-26T02:00:00Z", sha, "full", "failed");
        failed["data"]["generation"] = json!(2);
        writeln!(journal, "{passed}").unwrap();
        writeln!(journal, "{failed}").unwrap();

        let decision = receipt_decision(sha, &[journal.path().display().to_string()]);

        assert_eq!(decision["satisfied"], false);
        assert_eq!(decision["result"], "failed");
        assert_eq!(decision["receipt"]["data"]["generation"], 2);
    }

    #[test]
    fn pending_generation_supersedes_older_pass() {
        let mut journal = tempfile::NamedTempFile::new().unwrap();
        let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let passed = receipt_event("2026-07-26T01:00:00Z", sha, "full", "passed");
        let mut pending = receipt_event("2026-07-26T02:00:00Z", sha, "void", "pending");
        pending["data"]["generation"] = json!(2);
        pending["data"]["scope"] = json!(["preflight-execution"]);
        pending["data"]["steps_expected"] = json!(1);
        pending["data"]["steps_executed"] = json!(0);
        writeln!(journal, "{passed}").unwrap();
        writeln!(journal, "{pending}").unwrap();

        let decision = receipt_decision(sha, &[journal.path().display().to_string()]);

        assert_eq!(decision["satisfied"], false);
        assert_eq!(decision["mode"], "void");
        assert_eq!(decision["result"], "pending");
    }

    #[test]
    fn canonical_receipt_ignores_unreadable_optional_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().join("global.jsonl");
        let mirror = dir.path().join("delivery.jsonl");
        let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        std::fs::write(
            &canonical,
            format!(
                "{}\n",
                receipt_event("2026-07-26T03:00:00Z", sha, "full", "passed")
            ),
        )
        .unwrap();
        std::fs::create_dir(&mirror).unwrap();

        let decision = receipt_decision(
            sha,
            &[
                canonical.display().to_string(),
                mirror.display().to_string(),
            ],
        );

        assert_eq!(decision["satisfied"], true);
        assert_eq!(decision["coverage"]["unreadable_paths"], 0);
        assert_eq!(decision["coverage"]["unavailable_mirrors"], 1);
    }

    #[test]
    fn mirror_cannot_originate_satisfaction_without_canonical_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().join("global.jsonl");
        let mirror_path = dir.path().join("delivery.jsonl");
        let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        std::fs::write(&canonical, "").unwrap();
        std::fs::write(
            &mirror_path,
            format!(
                "{}\n",
                receipt_event("2026-07-26T03:00:00Z", sha, "full", "passed")
            ),
        )
        .unwrap();

        let decision = receipt_decision(
            sha,
            &[
                canonical.display().to_string(),
                mirror_path.display().to_string(),
            ],
        );

        assert_eq!(decision["satisfied"], false);
        assert_eq!(decision["result"], "unavailable");
        assert_eq!(decision["coverage"]["canonical_required"], true);
    }

    #[test]
    fn mirror_ahead_cannot_supersede_canonical_pending() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().join("global.jsonl");
        let mirror_path = dir.path().join("delivery.jsonl");
        let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut pending = receipt_event("2026-07-26T02:00:00Z", sha, "void", "pending");
        pending["data"]["generation"] = json!(5);
        pending["data"]["scope"] = json!(["preflight-execution"]);
        pending["data"]["steps_expected"] = json!(1);
        pending["data"]["steps_executed"] = json!(0);
        std::fs::write(
            &canonical,
            format!(
                "{}\n{pending}\n",
                receipt_event("2026-07-26T01:00:00Z", sha, "full", "passed")
            ),
        )
        .unwrap();
        let mut mirror = receipt_event("2026-07-26T03:00:00Z", sha, "full", "passed");
        mirror["data"]["generation"] = json!(100.0);
        std::fs::write(&mirror_path, format!("{mirror}\n")).unwrap();

        let decision = receipt_decision(
            sha,
            &[
                canonical.display().to_string(),
                mirror_path.display().to_string(),
            ],
        );

        assert_eq!(decision["satisfied"], false);
        assert_eq!(decision["result"], "unavailable");
        assert_eq!(decision["coverage"]["mirror_ahead"], true);
    }

    #[test]
    fn receipt_decision_fails_closed_when_journal_coverage_is_corrupt() {
        let mut journal = tempfile::NamedTempFile::new().unwrap();
        let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        writeln!(journal, "not json").unwrap();
        writeln!(
            journal,
            "{}",
            receipt_event("2026-07-26T03:00:00Z", sha, "full", "passed")
        )
        .unwrap();

        let decision = receipt_decision(sha, &[journal.path().display().to_string()]);

        assert_eq!(decision["mode"], "full");
        assert_eq!(decision["result"], "passed");
        assert_eq!(decision["coverage"]["complete"], false);
        assert_eq!(decision["satisfied"], false);
    }

    #[test]
    fn receipt_decision_rejects_equal_timestamp_conflicts() {
        let mut journal = tempfile::NamedTempFile::new().unwrap();
        let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        writeln!(
            journal,
            "{}",
            receipt_event("2026-07-26T03:00:00Z", sha, "full", "failed")
        )
        .unwrap();
        writeln!(
            journal,
            "{}",
            receipt_event("2026-07-26T03:00:00Z", sha, "full", "passed")
        )
        .unwrap();

        let decision = receipt_decision(sha, &[journal.path().display().to_string()]);

        assert_eq!(decision["satisfied"], false);
        assert_eq!(decision["result"], "unavailable");
        assert_eq!(decision["coverage"]["conflicting_latest"], 2);
    }

    #[test]
    fn receipt_decision_rejects_future_dated_evidence() {
        let mut journal = tempfile::NamedTempFile::new().unwrap();
        let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        writeln!(
            journal,
            "{}",
            receipt_event("2099-01-01T00:00:00Z", sha, "full", "passed")
        )
        .unwrap();

        let decision = receipt_decision(sha, &[journal.path().display().to_string()]);

        assert_eq!(decision["satisfied"], false);
        assert_eq!(decision["coverage"]["malformed_lines"], 1);
    }

    #[test]
    #[cfg(unix)]
    fn receipt_read_succeeds_while_preflight_write_lock_is_held() {
        let common = tempfile::tempdir().unwrap();
        // A preflight run holds the writer lock on the shared common dir.
        std::fs::create_dir(common.path().join(".preflight.lock.d")).unwrap();

        // Hermetic git_common_dir: the FNO_VERIFY_GIT_BIN seam answers the
        // one rev-parse the receipt verb makes, pointing at the temp dir.
        let bin = tempfile::tempdir().unwrap();
        let script = bin.path().join("git");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho {}\n", common.path().display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut journal = tempfile::NamedTempFile::new().unwrap();
        let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        writeln!(
            journal,
            "{}",
            receipt_event("2026-07-26T03:00:00Z", sha, "full", "passed")
        )
        .unwrap();

        std::env::set_var("FNO_VERIFY_GIT_BIN", &script);
        let (code, stdout, stderr) = run(&[
            "receipt".to_string(),
            sha.to_string(),
            journal.path().display().to_string(),
        ]);
        std::env::remove_var("FNO_VERIFY_GIT_BIN");

        assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
        let decision: Value = serde_json::from_str(stdout.trim()).unwrap();
        assert_eq!(decision["satisfied"], true);
        assert!(
            decision.pointer("/coverage/lock_error").is_none(),
            "coverage carried a lock error: {decision}"
        );
    }

    #[test]
    fn gate_accepts_actual_five_step_scope_without_optional_squads_guard() {
        let mut event = receipt_event(
            "2026-07-26T03:00:00Z",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "full",
            "passed",
        );
        event["data"]["scope"].as_array_mut().unwrap().pop();
        event["data"]["steps_expected"] = json!(5);
        event["data"]["steps_executed"] = json!(5);

        assert!(valid_receipt(&event));
        assert!(gate_eligible_receipt(&event));
    }

    #[test]
    fn hosted_ci_states_preserve_policy_observation_and_sha_distinctions() {
        let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let other = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let empty = json!([]);
        let pending = json!([{"status": "IN_PROGRESS", "conclusion": ""}]);
        let failed = json!([{"status": "COMPLETED", "conclusion": "FAILURE"}]);
        let passed = json!([{"status": "COMPLETED", "conclusion": "SUCCESS"}]);
        let malformed = json!(["not-an-object"]);
        let cases = [
            (true, WorkflowState::Absent, None, &empty, "not_configured"),
            (false, WorkflowState::Absent, None, &empty, "pending"),
            (true, WorkflowState::Present, None, &empty, "pending"),
            (
                false,
                WorkflowState::Unavailable,
                None,
                &empty,
                "unavailable",
            ),
            (false, WorkflowState::Present, Some(other), &empty, "stale"),
            (false, WorkflowState::Present, None, &passed, "unavailable"),
            (
                false,
                WorkflowState::Present,
                Some(sha),
                &pending,
                "pending",
            ),
            (false, WorkflowState::Present, Some(sha), &failed, "failed"),
            (false, WorkflowState::Present, Some(sha), &passed, "passed"),
            (
                false,
                WorkflowState::Present,
                Some(sha),
                &malformed,
                "unavailable",
            ),
        ];
        for (declared, workflow, observed, checks, expected) in cases {
            assert_eq!(
                hosted_ci_result(declared, workflow, sha, observed, Some(checks)).as_str(),
                expected
            );
        }
    }

    #[test]
    fn hosted_workflow_detection_revokes_declared_none() {
        let dir = tempfile::tempdir().unwrap();
        let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(hosted_ci_not_configured(true, dir.path(), sha));
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join("ci.yml"), "name: ci\n").unwrap();
        assert!(!hosted_ci_not_configured(true, dir.path(), sha));
        std::fs::remove_dir_all(dir.path().join(".github")).unwrap();
        std::fs::create_dir(dir.path().join(".github")).unwrap();
        std::fs::write(&workflows, "not a directory\n").unwrap();
        assert_eq!(
            hosted_workflow_state(dir.path()),
            WorkflowState::Unavailable
        );
    }
}
