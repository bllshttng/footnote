//! `fno-agents loop-check` verb (Task 1.1, ab-d0337fbc).
//!
//! Single entry-point decision-maker for the target stop hook. Reads external
//! state (manifest, transcript, git, gh, events, ledger) and returns a JSON
//! decision object. The manifest is NEVER mutated; the only write surface is
//! append-only event logs.
//!
//! Module name starts with "loop" to match the LOC-ratchet glob
//! `crates/fno-agents/src/loop*`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

// ── public types ──────────────────────────────────────────────────────────────

/// Why the loop terminated. Serialized as the exact string enum the spec names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminationReason {
    DonePRGreen,
    DoneAdvisory,
    /// A batch-lane member (batch-lane Wave 2/3): its commits live on a shared
    /// batch branch and ship via the batch PR, not its own, so there is no
    /// per-node PR to go green. Terminal, but NOT a ship reason - the batch's
    /// own `/pr create` graduates the plan; a member must not.
    DoneBatched,
    NoWork,
    Budget,
    NoProgress,
    Interrupted,
    Aborted,
}

/// The JSON object written to stdout on every fire.
#[derive(Debug, Serialize)]
pub struct LoopCheckOutput {
    pub decision: String, // "allow" | "block"
    pub termination_reason: Option<TerminationReason>,
    pub message: String,
    pub fires: u64,
    pub fingerprint: Option<String>,
}

// ── manifest parsing ──────────────────────────────────────────────────────────

/// Fields parsed from target-state.md YAML frontmatter.
#[derive(Debug)]
struct Manifest {
    session_id: Option<String>,
    created_at: Option<String>,
    attended: bool, // default true when absent
    advisory: bool,
    no_ship: bool,
    no_external: bool,
    /// batch-lane member: commits ship via the batch PR, not a per-node PR.
    batched: bool,
    legacy_status: Option<String>, // COMPLETE | BLOCKED | ABORTED
    /// None = absent (unlimited). Some(Ok(v)) = valid cap. Some(Err(s)) = malformed raw value.
    budget_wall_clock_cap_minutes: Option<Result<u64, String>>,
    /// None = absent (unlimited). Some(Ok(v)) = valid cap. Some(Err(s)) = malformed raw value.
    budget_cost_cap_usd: Option<Result<f64, String>>,
    /// Node-claim key + holder written by `fno target init` (x-ba4b). Used to
    /// renew the lease on every loop-check so a respawned worker keeps its claim.
    target_claim_key: Option<String>,
    target_claim_holder: Option<String>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            session_id: None,
            created_at: None,
            attended: true, // spec: attended defaults to true
            advisory: false,
            no_ship: false,
            no_external: false,
            batched: false,
            legacy_status: None,
            budget_wall_clock_cap_minutes: None, // None = absent = unlimited
            budget_cost_cap_usd: None,           // None = absent = unlimited
            target_claim_key: None,
            target_claim_holder: None,
        }
    }
}

/// Parse frontmatter from a `---\n...\n---\n` block at the top of a file.
/// Returns None if the file does not start with `---`.
/// Unknown fields are silently ignored.
fn parse_manifest(content: &str) -> Option<Manifest> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return None;
    }
    let after_first = &content[3..];
    // Find closing ---
    let end = after_first.find("\n---")?;
    let body = &after_first[..end];

    let mut m = Manifest {
        attended: true, // default
        ..Default::default()
    };

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            // YAML string values may be quoted; strip surrounding quotes so a
            // quoted session_id/created_at parses identically (gemini MEDIUM).
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
            match k {
                "session_id" => m.session_id = Some(v.to_string()),
                "created_at" => m.created_at = Some(v.to_string()),
                "target_claim_key" => m.target_claim_key = Some(v.to_string()),
                "target_claim_holder" => m.target_claim_holder = Some(v.to_string()),
                "attended" => m.attended = v == "true",
                "advisory" => m.advisory = v == "true",
                "no_ship" => m.no_ship = v == "true",
                "no_external" => m.no_external = v == "true",
                "batched" => m.batched = v == "true",
                "status" => {
                    let upper = v.to_uppercase();
                    if matches!(upper.as_str(), "COMPLETE" | "BLOCKED" | "ABORTED") {
                        m.legacy_status = Some(upper);
                    }
                }
                "budget_wall_clock_cap_minutes" => {
                    // Manifests are machine-written numeric fields; tolerate a '#'-tail
                    // (e.g. `90# Auto-merge inputs`) by truncating at the first '#'.
                    let stripped = v
                        .split_once('#')
                        .map(|(before, _)| before.trim())
                        .unwrap_or(v);
                    m.budget_wall_clock_cap_minutes = Some(stripped.parse::<u64>().map_err(|_| {
                        eprintln!(
                            "loop-check: malformed budget cap 'budget_wall_clock_cap_minutes: {v}' - failing closed; fix the config"
                        );
                        v.to_string()
                    }));
                }
                "budget_cost_cap_usd" => {
                    let stripped = v
                        .split_once('#')
                        .map(|(before, _)| before.trim())
                        .unwrap_or(v);
                    m.budget_cost_cap_usd = Some(stripped.parse::<f64>().map_err(|_| {
                        eprintln!(
                            "loop-check: malformed budget cap 'budget_cost_cap_usd: {v}' - failing closed; fix the config"
                        );
                        v.to_string()
                    }));
                }
                _ => {}
            }
        }
    }
    Some(m)
}

// ── settings parsing ──────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Settings {
    /// config.budget.attended.wall_clock_cap_minutes
    /// None = absent. Some(Ok(v)) = valid. Some(Err(s)) = malformed raw value.
    attended_wall_cap_minutes: Option<Result<u64, String>>,
    /// config.budget.attended.cost_cap_usd
    attended_cost_cap_usd: Option<Result<f64, String>>,
    /// config.budget.unattended.wall_clock_cap_minutes
    unattended_wall_cap_minutes: Option<Result<u64, String>>,
    /// config.budget.unattended.cost_cap_usd
    unattended_cost_cap_usd: Option<Result<f64, String>>,
    /// flat budget_cap: (folds in ab-41b13d9d) - applies as cost cap for both modes
    flat_budget_cap: Option<Result<f64, String>>,
    /// config.ci.declared_none: true
    ci_declared_none: bool,
    /// config.external_reviewers list
    external_reviewers: Vec<String>,
    /// config.review.required_bots (grilled decision 5 / step 2).
    /// None = key absent -> code default applies.
    /// Some([]) = explicitly `[]` -> declared no-review-gate path.
    /// Some(list) = every listed bot must have a completed review pass.
    /// A malformed value (scalar, bare key with no items) stays None so the
    /// gate fails closed to the code default (AC3-ERR).
    required_bots: Option<Vec<String>>,
}

/// Strip a trailing YAML inline comment (` # ...`) from a raw scalar value
/// (codex P2 on #448). YAML requires whitespace before the `#`; a value that
/// IS a comment strips to empty. Quoted values containing '#' are out of
/// scope for this minimal parser (no known bot login contains '#').
fn strip_inline_comment(raw: &str) -> &str {
    if raw.starts_with('#') {
        return "";
    }
    match raw.find(" #").or_else(|| raw.find("\t#")) {
        Some(i) => raw[..i].trim_end(),
        None => raw,
    }
}

/// Minimal indentation-aware settings.yaml parser.
/// Handles nested `config.budget.attended/unattended` blocks plus flat keys.
fn parse_settings(content: &str) -> Settings {
    let mut s = Settings::default();
    let mut in_config = false;
    let mut in_budget = false;
    let mut in_attended = false;
    let mut in_unattended = false;
    let mut in_ci = false;
    let mut in_review = false;
    let mut collecting_reviewers = false;
    let mut collecting_required_bots = false;

    // Derive the file's indent unit from the first indented line instead of
    // assuming 2 spaces, so a 4-space-indented settings.yaml parses
    // identically instead of being silently skipped (gemini HIGH on #447).
    let unit = content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|l| l.len() - l.trim_start().len())
        .find(|&i| i > 0)
        .unwrap_or(2);

    for line in content.lines() {
        if line.trim_start().starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let raw_indent = line.len() - line.trim_start().len();
        // Normalize to the canonical 2-space levels the state machine below
        // matches on (0 / 2 / 4 / 6).
        let indent = (raw_indent / unit) * 2;
        let trimmed = line.trim();

        // Top-level: indent == 0
        if indent == 0 {
            in_config = trimmed.starts_with("config:");
            collecting_reviewers = false;
            collecting_required_bots = false;
            if !in_config {
                in_budget = false;
                in_attended = false;
                in_unattended = false;
                in_ci = false;
                in_review = false;
            }
            // Flat key: budget_cap: N
            if let Some(rest) = trimmed.strip_prefix("budget_cap:") {
                let raw = rest.trim();
                s.flat_budget_cap = Some(raw.parse::<f64>().map_err(|_| {
                    eprintln!(
                        "loop-check: malformed budget cap 'budget_cap: {raw}' - failing closed; fix the config"
                    );
                    raw.to_string()
                }));
            }
            continue;
        }

        // indent == 2: inside config
        if in_config && indent == 2 {
            in_budget = trimmed.starts_with("budget:");
            in_ci = trimmed.starts_with("ci:");
            in_review = trimmed.starts_with("review:");
            collecting_reviewers = trimmed.starts_with("external_reviewers:");
            collecting_required_bots = false;
            if !in_budget {
                in_attended = false;
                in_unattended = false;
            }
            continue;
        }

        // indent == 4: inside config.budget or config.ci
        if in_config && in_budget && indent == 4 {
            in_attended = trimmed.starts_with("attended:");
            in_unattended = trimmed.starts_with("unattended:");
            continue;
        }

        if in_config && in_ci && indent == 4 {
            if let Some(rest) = trimmed.strip_prefix("declared_none:") {
                s.ci_declared_none = rest.trim() == "true";
            }
            continue;
        }

        // indent == 4: inside config.review
        if in_config && in_review && indent == 4 {
            if let Some(rest) = trimmed.strip_prefix("required_bots:") {
                // Strip a trailing YAML inline comment first (codex P2 on
                // #448): `required_bots: []  # no review gate` must parse as
                // the declared-empty form, not fall through to malformed.
                let raw = strip_inline_comment(rest.trim());
                if raw == "[]" {
                    // Explicit empty list: the ONLY way to declare the
                    // no-review-gate path (US3). Never inferred.
                    s.required_bots = Some(Vec::new());
                    collecting_required_bots = false;
                } else if raw.is_empty() {
                    // Block-list form: items follow at deeper indent. Until an
                    // item arrives this stays None - a bare key with nothing
                    // under it is malformed and fails closed to the code
                    // default rather than accidentally disabling the gate.
                    collecting_required_bots = true;
                } else if raw.starts_with('[') && raw.ends_with(']') {
                    // Inline list form: required_bots: ["a", "b"]
                    let inner = &raw[1..raw.len() - 1];
                    let items: Vec<String> = inner
                        .split(',')
                        .map(|p| p.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
                        .filter(|p| !p.is_empty())
                        .collect();
                    s.required_bots = Some(items);
                    collecting_required_bots = false;
                } else {
                    // Scalar / malformed -> fail closed to the code default
                    eprintln!(
                        "loop-check: malformed config.review.required_bots '{raw}' (not a list) - using code default"
                    );
                    s.required_bots = None;
                    collecting_required_bots = false;
                }
            } else {
                collecting_required_bots = false;
            }
            continue;
        }

        // Required-bots list items: "- login" under config.review.required_bots
        if in_config && collecting_required_bots && trimmed.starts_with('-') {
            let bot = strip_inline_comment(trimmed.trim_start_matches('-').trim())
                .trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .to_string();
            if !bot.is_empty() {
                s.required_bots.get_or_insert_with(Vec::new).push(bot);
            }
            continue;
        }

        // indent == 6: inside attended/unattended blocks
        if in_config && in_budget && (in_attended || in_unattended) && indent == 6 {
            if let Some(rest) = trimmed.strip_prefix("wall_clock_cap_minutes:") {
                let raw = rest.trim();
                let parsed = raw.parse::<u64>().map_err(|_| {
                    let which = if in_attended { "attended" } else { "unattended" };
                    eprintln!(
                        "loop-check: malformed budget cap '{which}.wall_clock_cap_minutes: {raw}' - failing closed; fix the config"
                    );
                    raw.to_string()
                });
                if in_attended {
                    s.attended_wall_cap_minutes = Some(parsed);
                } else {
                    s.unattended_wall_cap_minutes = Some(parsed);
                }
            }
            if let Some(rest) = trimmed.strip_prefix("cost_cap_usd:") {
                let raw = rest.trim();
                let parsed = raw.parse::<f64>().map_err(|_| {
                    let which = if in_attended { "attended" } else { "unattended" };
                    eprintln!(
                        "loop-check: malformed budget cap '{which}.cost_cap_usd: {raw}' - failing closed; fix the config"
                    );
                    raw.to_string()
                });
                if in_attended {
                    s.attended_cost_cap_usd = Some(parsed);
                } else {
                    s.unattended_cost_cap_usd = Some(parsed);
                }
            }
            continue;
        }

        // External reviewers list items: "  - login" under config.external_reviewers
        if in_config && collecting_reviewers && trimmed.starts_with('-') {
            let reviewer = trimmed.trim_start_matches('-').trim().to_string();
            if !reviewer.is_empty() {
                s.external_reviewers.push(reviewer);
            }
        }
    }
    s
}

// ── ledger parsing ────────────────────────────────────────────────────────────

/// Sum cost_usd for entries matching session_id. Tolerate missing/malformed as 0.
fn session_cost_from_ledger(ledger_path: &Path, session_id: &str) -> f64 {
    let Ok(content) = std::fs::read_to_string(ledger_path) else {
        return 0.0;
    };
    let Ok(arr) = serde_json::from_str::<Value>(&content) else {
        return 0.0;
    };
    let Some(entries) = arr.as_array() else {
        return 0.0;
    };
    let mut total = 0.0_f64;
    for entry in entries {
        if entry.get("session_id").and_then(|v| v.as_str()) == Some(session_id) {
            if let Some(c) = entry.get("cost_usd").and_then(|v| v.as_f64()) {
                total += c;
            }
        }
    }
    total
}

// ── transcript parsing ────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum Intent {
    Promise,
    Aborted { reason: String },
    None,
}

fn extract_assistant_text(val: &Value) -> String {
    // Try /message/content as string
    if let Some(s) = val.pointer("/message/content").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    // Try /message/content as array of blocks
    if let Some(arr) = val.pointer("/message/content").and_then(|v| v.as_array()) {
        let mut parts = Vec::new();
        for block in arr {
            // Only include text blocks (not tool_use, tool_result)
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    parts.push(t.to_string());
                }
            }
        }
        return parts.join(" ");
    }
    // Fallback: top-level content
    if let Some(s) = val.get("content").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    String::new()
}

/// Detect intent with proper attribute extraction for aborted reason.
fn detect_intent_from_text(text: &str) -> Intent {
    // Look for <aborted ...> tag
    if let Some(aborted_start) = text.find("<aborted") {
        // Find the closing >
        if let Some(gt) = text[aborted_start..].find('>') {
            let tag_text = &text[aborted_start..aborted_start + gt + 1];
            let reason = parse_xml_attr(tag_text, "reason").unwrap_or_default();
            return Intent::Aborted { reason };
        }
    }
    if text.contains("<promise>") {
        return Intent::Promise;
    }
    Intent::None
}

fn parse_xml_attr(tag_text: &str, attr: &str) -> Option<String> {
    let pattern = format!(r#"{attr}=""#);
    let start = tag_text.find(&pattern)? + pattern.len();
    let end = tag_text[start..].find('"')?;
    Some(tag_text[start..start + end].to_string())
}

/// Extract `last_assistant_message` from the Stop-hook stdin JSON
/// (ab-223d2dae). The harness emits it as a plain string (the stopping
/// turn's final assistant text, blocks joined by newline and trimmed),
/// omitted when empty. Any parse failure -> None so the caller falls back
/// to the transcript scan.
fn extract_last_assistant_message(hook_input: &str) -> Option<String> {
    let val: Value = serde_json::from_str(hook_input).ok()?;
    let s = val.get("last_assistant_message")?.as_str()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// A-primary, B-fallback intent read (ab-223d2dae). A present payload is the
/// stopping turn's final text - recomputed per fire, race-free, overwrite-
/// proof - and is authoritative, INCLUDING its "no tag" answer. Falling
/// through to the transcript behind a tag-less payload would resurrect the
/// stale-promise edge the bounded scan exists to contain. Returns the intent
/// plus its source for the loop_check event (`payload` | `transcript`).
fn detect_intent(
    last_assistant_message: Option<&str>,
    transcript_path: &Path,
) -> (Intent, &'static str) {
    match last_assistant_message {
        Some(text) => (detect_intent_from_text(text), "payload"),
        None => (detect_intent_full(transcript_path), "transcript"),
    }
}

/// Fallback transcript scan (ab-223d2dae, B): bounded lookback over the
/// newest INTENT_LOOKBACK_ENTRIES assistant text entries instead of
/// last-line-only. Newest tag wins; a tag-less entry no longer ends the
/// scan, which covers the promise-overwritten-by-block-feedback shape when
/// no payload exists. The bound is load-bearing: a stale promise from
/// pivoted work must fall out of the window (done()'s head_shipped read is
/// the real gate against the remainder).
const INTENT_LOOKBACK_ENTRIES: usize = 5;

fn detect_intent_full(transcript_path: &Path) -> Intent {
    let Ok(content) = std::fs::read_to_string(transcript_path) else {
        return Intent::None;
    };

    let lines: Vec<&str> = content.lines().collect();
    let mut scanned: usize = 0;
    for line in lines.iter().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let role = val
            .pointer("/message/role")
            .or_else(|| val.get("role"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if role != "assistant" {
            continue;
        }
        let text = extract_assistant_text(&val);
        if text.is_empty() {
            continue;
        }
        match detect_intent_from_text(&text) {
            Intent::None => {
                scanned += 1;
                if scanned >= INTENT_LOOKBACK_ENTRIES {
                    return Intent::None;
                }
            }
            tagged => return tagged,
        }
    }
    Intent::None
}

// ── git / gh helpers ──────────────────────────────────────────────────────────

/// PR state vocabulary (fu-4faa3d). Parsed once at the read_pr_info boundary.
/// `as_str()` reproduces the exact legacy strings so the fingerprint (which
/// persists across fires in events.jsonl) stays byte-identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrState {
    Open,
    Merged,
    Closed,
    /// No PR, or an unrecognized gh state string (fail-closed, AC5-EDGE).
    None,
}

impl PrState {
    fn from_gh_str(s: &str) -> Self {
        match s {
            "OPEN" => PrState::Open,
            "MERGED" => PrState::Merged,
            "CLOSED" => PrState::Closed,
            _ => PrState::None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            PrState::Open => "OPEN",
            PrState::Merged => "MERGED",
            PrState::Closed => "CLOSED",
            PrState::None => "none",
        }
    }

    fn is_open_or_merged(&self) -> bool {
        matches!(self, PrState::Open | PrState::Merged)
    }
}

/// CI conclusion vocabulary (fu-4faa3d). `render()` reproduces the exact
/// legacy strings ("FAILURE:{name}" carries the failing check name).
#[derive(Debug, Clone, PartialEq, Eq)]
enum CiConclusion {
    Success,
    /// Failing check name when one was identified.
    Failure(Option<String>),
    Pending,
    /// CI read skipped via ci.declared_none.
    Skipped,
    /// No checks found (fail-closed unless declared_none).
    None,
}

impl CiConclusion {
    fn render(&self) -> String {
        match self {
            CiConclusion::Success => "SUCCESS".to_string(),
            CiConclusion::Failure(Some(name)) => format!("FAILURE:{name}"),
            CiConclusion::Failure(None) => "FAILURE".to_string(),
            CiConclusion::Pending => "PENDING".to_string(),
            CiConclusion::Skipped => "skipped".to_string(),
            CiConclusion::None => "none".to_string(),
        }
    }

    fn is_ok(&self) -> bool {
        matches!(self, CiConclusion::Success | CiConclusion::Skipped)
    }
}

#[derive(Debug)]
struct PrInfo {
    state: PrState,
    number: i64,
    /// PR head commit OID; must match local HEAD for DonePRGreen (codex P1
    /// on #447: a green PR must not complete a session with unpushed work).
    head_oid: String,
    ci_conclusion: CiConclusion,
    /// Newest review/comment/inline-comment activity (ISO8601 or "none");
    /// folded into the fingerprint's 4th component on done() fires.
    latest_review_ts: String,
    reviewed: bool, // every required bot passed AND no unaddressed blocking finding
    /// Required bots with no completed review pass (names the gap in the
    /// block message, AC1-UI).
    missing_bots: Vec<String>,
    /// Blocking inline findings (codex P1 / gemini critical|high) whose
    /// thread has no qualifying ack (AC2).
    unaddressed_findings: Vec<Finding>,
    /// Reads 3+4 were skipped (per-session no_external OR the repo declared
    /// `required_bots: []`). Recorded in loop_check events so the skip is
    /// observable, not silently absent (AC3-UI).
    review_skipped: bool,
}

fn git_head_sha(git_bin: &str, cwd: &Path) -> String {
    let out = Command::new(git_bin)
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

/// `gh pr view` exits 1 both when no PR exists and when gh itself fails.
/// "No PR" is real world-state - the fingerprint should record it and the
/// NoProgress backstop should keep ticking - while an outage must freeze the
/// streak (US4). Distinguish via gh's deterministic no-PR stderr message. If
/// gh ever changes the message, no-PR fires degrade to outage semantics
/// (freeze -> budget ceiling): safe, never a premature termination.
fn is_no_pr_stderr(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr)
        .to_lowercase()
        .contains("no pull requests found")
}

/// Capture the last ~200 bytes of stderr as a lossy UTF-8 string.
fn stderr_tail(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim();
    if s.len() <= 200 {
        s.to_string()
    } else {
        // Byte index must land on a char boundary or the slice panics
        // (gemini HIGH on PR #447): walk forward to the next boundary.
        let mut start = s.len() - 200;
        while start < s.len() && !s.is_char_boundary(start) {
            start += 1;
        }
        s[start..].to_string()
    }
}

/// Run done() reads. Returns Ok(PrInfo) or Err((read_name, stderr_tail)) on gh failure.
fn read_pr_info(
    gh_bin: &str,
    cwd: &Path,
    ci_declared_none: bool,
    no_external: bool,
    required_bots: &[String],
    external_reviewers: &[String],
) -> Result<PrInfo, (String, String)> {
    // Read 1: PR state + number + head OID
    let pr_view_out = Command::new(gh_bin)
        .args([
            "pr",
            "view",
            "--json",
            "state,number,headRefName,headRefOid",
        ])
        .current_dir(cwd)
        .output()
        .map_err(|e| ("pr_view".to_string(), e.to_string()))?;

    if !pr_view_out.status.success() {
        if is_no_pr_stderr(&pr_view_out.stderr) {
            // No PR yet: world-state, not an error. done() is simply false
            // ("no PR for HEAD"), and the backstop can resolve a stuck
            // no-PR session as NoProgress rather than freezing forever.
            return Ok(PrInfo {
                state: PrState::None,
                number: 0,
                head_oid: String::new(),
                ci_conclusion: CiConclusion::None,
                latest_review_ts: "none".to_string(),
                reviewed: false,
                missing_bots: Vec::new(),
                unaddressed_findings: Vec::new(),
                review_skipped: false,
            });
        }
        return Err(("pr_view".to_string(), stderr_tail(&pr_view_out.stderr)));
    }

    let pr_json: Value = serde_json::from_slice(&pr_view_out.stdout)
        .map_err(|_| ("pr_view_parse".to_string(), String::new()))?;

    let state = PrState::from_gh_str(
        pr_json
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("none"),
    );
    let number = pr_json.get("number").and_then(|v| v.as_i64()).unwrap_or(0);
    let head_oid = pr_json
        .get("headRefOid")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // x-8b64 (E): a MERGED PR is terminal. A PR merged out-of-band (GitHub
    // web/mobile, or `gh pr merge`) is done regardless of whether the required
    // bot ever reviewed it or whether CI is still green post-merge - the merge
    // IS the authority. Short-circuit the now-irrelevant CI + review polls
    // (which also avoids a transient gh blip on those reads re-blocking a
    // finished session). The single merge signal is `state` from the same
    // `gh pr view` call that `reconcile`/`fno pr verify` read - one signal, not
    // two independently-polled sources. done()'s `head_shipped` guard still
    // applies downstream: an unpushed commit on top of a merged PR stays
    // unshipped work.
    if state == PrState::Merged {
        return Ok(PrInfo {
            state,
            number,
            head_oid,
            ci_conclusion: CiConclusion::Skipped,
            latest_review_ts: "none".to_string(),
            reviewed: true,
            missing_bots: Vec::new(),
            unaddressed_findings: Vec::new(),
            review_skipped: true,
        });
    }

    // Read 2: CI checks
    let ci_conclusion = if ci_declared_none {
        CiConclusion::Skipped
    } else {
        let checks_out = Command::new(gh_bin)
            .args(["pr", "checks", "--json", "name,state,bucket"])
            .current_dir(cwd)
            .output()
            .map_err(|e| ("pr_checks".to_string(), e.to_string()))?;

        if !checks_out.status.success() {
            return Err(("pr_checks".to_string(), stderr_tail(&checks_out.stderr)));
        }

        let checks: Value = serde_json::from_slice(&checks_out.stdout)
            .map_err(|_| ("pr_checks_parse".to_string(), String::new()))?;

        compute_ci_conclusion(&checks).map_err(|e| (e, String::new()))?
    };

    // Reads 3+4: reviews + inline findings. Skipped when the session declares
    // no_external OR the repo declares `required_bots: []` (the no-review-gate
    // path, US3 - mirrors ci.declared_none; PR + CI carry the gate). The two
    // skips are orthogonal: one is per-session, the other repo config.
    let review_skipped = no_external || required_bots.is_empty();
    let (latest_review_ts, reviewed, missing_bots, unaddressed_findings) = if review_skipped {
        ("none".to_string(), true, Vec::new(), Vec::new()) // skip reads, treat as reviewed
    } else {
        // Read 3: top-level reviews + issue comments
        let reviews_out = Command::new(gh_bin)
            .args(["pr", "view", "--json", "reviews,comments"])
            .current_dir(cwd)
            .output()
            .map_err(|e| ("pr_reviews".to_string(), e.to_string()))?;

        if !reviews_out.status.success() {
            return Err(("pr_reviews".to_string(), stderr_tail(&reviews_out.stderr)));
        }

        let reviews_json: Value = serde_json::from_slice(&reviews_out.stdout)
            .map_err(|_| ("pr_reviews_parse".to_string(), String::new()))?;

        let info = compute_review_info(&reviews_json, required_bots);

        // Read 4: inline review comments (NEW in step 2). Codex's P1s land on
        // the /pulls/N/comments REST endpoint, which `gh pr view --json
        // comments` does NOT return (verified on PR #447). --paginate may
        // emit CONCATENATED JSON arrays (one per page), so parse as a stream.
        let comments_out = Command::new(gh_bin)
            .args([
                "api",
                &format!("repos/{{owner}}/{{repo}}/pulls/{number}/comments"),
                "--paginate",
            ])
            .current_dir(cwd)
            .output()
            .map_err(|e| ("pulls_comments".to_string(), e.to_string()))?;

        if !comments_out.status.success() {
            return Err((
                "pulls_comments".to_string(),
                stderr_tail(&comments_out.stderr),
            ));
        }

        let mut inline_comments: Vec<Value> = Vec::new();
        for page in serde_json::Deserializer::from_slice(&comments_out.stdout).into_iter::<Value>()
        {
            let page = page.map_err(|_| ("pulls_comments_parse".to_string(), String::new()))?;
            match page.as_array() {
                Some(arr) => inline_comments.extend(arr.iter().cloned()),
                None => return Err(("pulls_comments_parse".to_string(), String::new())),
            }
        }

        // Commit timestamps feed the commit-after arm of "addressed". Only
        // fetched when a blocking candidate could exist (cheap pre-scan).
        let has_blocking_candidate = inline_comments.iter().any(|c| {
            c.get("in_reply_to_id").and_then(|v| v.as_i64()).is_none()
                && blocking_severity(c.get("body").and_then(|v| v.as_str()).unwrap_or("")).is_some()
        });
        let commit_dates: Vec<String> = if has_blocking_candidate {
            let commits_out = Command::new(gh_bin)
                .args(["pr", "view", "--json", "commits"])
                .current_dir(cwd)
                .output()
                .map_err(|e| ("pr_commits".to_string(), e.to_string()))?;
            if !commits_out.status.success() {
                return Err(("pr_commits".to_string(), stderr_tail(&commits_out.stderr)));
            }
            let commits_json: Value = serde_json::from_slice(&commits_out.stdout)
                .map_err(|_| ("pr_commits_parse".to_string(), String::new()))?;
            commits_json
                .get("commits")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| {
                            c.get("committedDate")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let (inline_ts, unaddressed) = compute_unaddressed_findings(
            &inline_comments,
            &commit_dates,
            required_bots,
            external_reviewers,
        );

        // Read 4's newest comment timestamp joins the activity timestamp so
        // inline-only review traffic advances the fingerprint (closes the
        // false-NoProgress hole).
        let activity_ts = max_ts(&info.latest_ts, &inline_ts);
        let reviewed = info.all_required_passed() && unaddressed.is_empty();
        (activity_ts, reviewed, info.missing_bots, unaddressed)
    };

    Ok(PrInfo {
        state,
        number,
        head_oid,
        ci_conclusion,
        latest_review_ts,
        reviewed,
        missing_bots,
        unaddressed_findings,
        review_skipped,
    })
}

fn compute_ci_conclusion(checks: &Value) -> Result<CiConclusion, String> {
    let arr = match checks.as_array() {
        Some(a) => a,
        None => return Err("pr_checks_parse".to_string()),
    };

    if arr.is_empty() {
        // No checks configured and no declared_none -> fail closed
        return Ok(CiConclusion::None);
    }

    // `gh pr checks --json` classifies each check into a rollup `bucket`:
    // pass | fail | pending | skipping | cancel. (`conclusion` is NOT an
    // available field on this subcommand; requesting it errored the read on
    // every fire - ab-610d2ee3 follow-on, previously masked by the budget
    // bug terminating sessions before this read ran.) Unknown or missing
    // buckets fail closed as Pending - never green.
    let bucket_of = |check: &Value| -> String {
        check
            .get("bucket")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase()
    };

    if let Some(failing) = arr
        .iter()
        .find(|c| matches!(bucket_of(c).as_str(), "fail" | "cancel"))
    {
        let name = failing
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return Ok(CiConclusion::Failure(Some(name.to_string())));
    }
    if arr
        .iter()
        .any(|c| !matches!(bucket_of(c).as_str(), "pass" | "skipping"))
    {
        return Ok(CiConclusion::Pending);
    }
    Ok(CiConclusion::Success)
}

/// Known bot logins that count as reviewers when external_reviewers is not configured.
const KNOWN_BOTS: &[&str] = &["chatgpt-codex-connector", "gemini-code-assist"];

/// Default must-have-reviewed list when config.review.required_bots is absent.
/// EMPTY for fresh installs: a clone with no review configuration completes on
/// PR + CI green without hanging on a review bot it has never set up (a fresh
/// `/target` otherwise runs to the budget cap waiting for a codex review that
/// never arrives). Maintainers who want an external-review gate pin it
/// explicitly via config.review.required_bots (e.g. ["chatgpt-codex-connector"]).
const DEFAULT_REQUIRED_BOTS: &[&str] = &[];

fn resolved_required_bots(settings: &Settings) -> Vec<String> {
    match &settings.required_bots {
        Some(list) => list.clone(),
        None => DEFAULT_REQUIRED_BOTS
            .iter()
            .map(|s| s.to_string())
            .collect(),
    }
}

/// Case-insensitive substring match so a configured short name ("codex") or a
/// full login both match the review author, including gh's `[bot]`-suffixed
/// form (reference_gh_bot_login_suffix_polling_trap).
fn login_matches_bot(login: &str, bot: &str) -> bool {
    !bot.is_empty() && login.to_lowercase().contains(&bot.to_lowercase())
}

fn is_bot_reviewer(login: &str, external_reviewers: &[String]) -> bool {
    if !external_reviewers.is_empty() {
        let login_lower = login.to_lowercase();
        // Case-insensitive substring match: "gemini" matches "gemini-code-assist[bot]"
        if external_reviewers
            .iter()
            .any(|r| login_lower.contains(&r.to_lowercase()))
        {
            return true;
        }
        // Configured list present but no entry matched: fall back to bot heuristic
        // so a configured-but-partial list doesn't make reviewed unreachable.
    }
    // Default: endswith [bot] or known list
    login.ends_with("[bot]") || KNOWN_BOTS.iter().any(|&b| login.contains(b))
}

/// Per-required-bot review verdict (grilled decision 5 / step 2).
#[derive(Debug)]
struct ReviewInfo {
    /// Latest review/comment activity timestamp, or "none".
    latest_ts: String,
    /// Required bots with no completed review pass. A pass is a top-level
    /// review with any non-empty state on ANY commit - in practice COMMENTED
    /// (verified on PR #447; codex reviews once per PR and never re-reviews,
    /// so requiring a pass on HEAD would make the gate unsatisfiable).
    missing_bots: Vec<String>,
}

impl ReviewInfo {
    /// Every required bot has at least one completed pass.
    fn all_required_passed(&self) -> bool {
        self.missing_bots.is_empty()
    }
}

fn compute_review_info(reviews_json: &Value, required_bots: &[String]) -> ReviewInfo {
    let reviews = reviews_json
        .get("reviews")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let comments = reviews_json
        .get("comments")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);

    let mut latest_ts = String::new(); // empty; "none" returned if no activity found
    let mut passed: Vec<bool> = vec![false; required_bots.len()];

    for r in reviews {
        let login = r
            .pointer("/author/login")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let submitted_at = r.get("submittedAt").and_then(|v| v.as_str()).unwrap_or("");
        let state = r.get("state").and_then(|v| v.as_str()).unwrap_or("");

        if !submitted_at.is_empty() && submitted_at > latest_ts.as_str() {
            latest_ts = submitted_at.to_string();
        }

        if !state.is_empty() {
            for (i, bot) in required_bots.iter().enumerate() {
                if login_matches_bot(login, bot) {
                    passed[i] = true;
                }
            }
        }
    }

    for c in comments {
        let created_at = c.get("createdAt").and_then(|v| v.as_str()).unwrap_or("");
        if !created_at.is_empty() && created_at > latest_ts.as_str() {
            latest_ts = created_at.to_string();
        }
    }

    let final_ts = if latest_ts.is_empty() {
        "none".to_string()
    } else {
        latest_ts
    };

    let missing_bots: Vec<String> = required_bots
        .iter()
        .zip(passed.iter())
        .filter(|(_, ok)| !**ok)
        .map(|(bot, _)| bot.clone())
        .collect();

    ReviewInfo {
        latest_ts: final_ts,
        missing_bots,
    }
}

// ── inline findings (Read 4, step 2 / US2) ────────────────────────────────────

/// A blocking inline finding: a root review comment (in_reply_to_id == null)
/// authored by a required bot whose body carries a blocking severity badge.
#[derive(Debug, Clone)]
struct Finding {
    id: i64,
    /// Bot login that posted the finding (REST `user.login`).
    author: String,
    path: String,
    line: i64,
    created_at: String,
    /// Parsed severity label (P1 / critical / high).
    severity: &'static str,
}

/// Parse a blocking severity from the bot's own badge markup. The exact
/// strings are pinned from PR #447 ground truth; both the alt-text and the
/// badge-URL forms are matched so a partial render still classifies:
///   codex:  `![P1 Badge](https://img.shields.io/badge/P1-orange?style=flat)`
///   gemini: `![high](https://www.gstatic.com/codereviewagent/high-priority.svg)`
/// Anything unparseable is advisory, never blocking (locked decision 4:
/// under-blocking is the only safe failure - the agent cannot edit a bot's
/// comment, and PR history is the post-hoc backstop).
fn blocking_severity(body: &str) -> Option<&'static str> {
    if body.contains("![P1 Badge]") || body.contains("badge/P1-") {
        return Some("P1");
    }
    if body.contains("![critical]") || body.contains("critical-priority.svg") {
        return Some("critical");
    }
    if body.contains("![high]") || body.contains("high-priority.svg") {
        return Some("high");
    }
    None
}

/// Max of two timestamp strings, treating "none"/"" as the lowest value.
/// Both sides are compared chronologically when they parse (gemini HIGH on
/// #448: an offset-suffixed timestamp can sort above a Zulu one
/// lexicographically while being earlier in UTC); the returned value is
/// always one of the ORIGINAL strings so the fingerprint stays byte-stable.
/// Unparseable-but-real strings fall back to lexicographic comparison.
fn max_ts(a: &str, b: &str) -> String {
    if let (Ok(da), Ok(db)) = (a.parse::<DateTime<Utc>>(), b.parse::<DateTime<Utc>>()) {
        return if da >= db {
            a.to_string()
        } else {
            b.to_string()
        };
    }
    let a_real = !a.is_empty() && a != "none";
    let b_real = !b.is_empty() && b != "none";
    match (a_real, b_real) {
        (true, true) => {
            if a >= b {
                a.to_string()
            } else {
                b.to_string()
            }
        }
        (true, false) => a.to_string(),
        (false, true) => b.to_string(),
        (false, false) => "none".to_string(),
    }
}

/// The `wontfix:` decline marker (documented in skills/check-pr). Matched
/// case-insensitively in a non-bot reply body.
const WONTFIX_MARKER: &str = "wontfix:";

/// True iff `a` is strictly after `b`. Both sides parse as RFC3339; an
/// unparseable timestamp returns false, so a blocking finding is never
/// cleared on garbage data. Raw string comparison is NOT used here because
/// offset-suffixed and Z-suffixed forms mis-order lexicographically
/// (e.g. "...T23:30:00+13:00" sorts above "...T11:00:00Z" as a string but
/// is 30 minutes EARLIER in UTC).
fn ts_after(a: &str, b: &str) -> bool {
    match (a.parse::<DateTime<Utc>>(), b.parse::<DateTime<Utc>>()) {
        (Ok(da), Ok(db)) => da > db,
        _ => false,
    }
}

/// Walk the `/pulls/N/comments` array (REST shape: `user.login`,
/// `in_reply_to_id`, `created_at`). Returns the newest comment timestamp
/// (fingerprint contribution) and the UNADDRESSED blocking findings.
///
/// A blocking finding is addressed iff its thread has a non-bot reply AND
/// (a commit landed after the finding's created_at OR a non-bot reply body
/// carries `wontfix:`). The reply is mandatory: a commit alone must not
/// silently clear a P1 (anti-gaming, locked decision 3).
fn compute_unaddressed_findings(
    comments: &[Value],
    commit_dates: &[String],
    required_bots: &[String],
    external_reviewers: &[String],
) -> (String, Vec<Finding>) {
    let mut latest_ts = String::new();
    let mut candidates: Vec<Finding> = Vec::new();
    // finding id -> non-bot replies' bodies
    let mut replies: std::collections::HashMap<i64, Vec<String>> = std::collections::HashMap::new();

    for c in comments {
        let created_at = c.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        if !created_at.is_empty() && created_at > latest_ts.as_str() {
            latest_ts = created_at.to_string();
        }

        let login = c
            .pointer("/user/login")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let body = c.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let in_reply_to = c.get("in_reply_to_id").and_then(|v| v.as_i64());

        match in_reply_to {
            Some(parent_id) => {
                // A reply. Only non-bot replies count as the agent's ack.
                if !is_bot_reviewer(login, external_reviewers) {
                    replies.entry(parent_id).or_default().push(body.to_string());
                }
            }
            None => {
                // A root comment: a finding when a required bot posted it
                // with a blocking badge.
                let by_required_bot = required_bots
                    .iter()
                    .any(|bot| login_matches_bot(login, bot));
                if by_required_bot {
                    if let Some(severity) = blocking_severity(body) {
                        // A REST comment always carries an integer id; a row
                        // without one is schema drift. Skip it rather than
                        // pooling id-less findings on a shared default bucket
                        // where a single stray reply could mark them all
                        // addressed (under-blocking is the safe direction per
                        // locked decision 4; PR history is the backstop).
                        let Some(id) = c.get("id").and_then(|v| v.as_i64()) else {
                            eprintln!(
                                "loop-check: skipping blocking finding with missing id (author={login})"
                            );
                            continue;
                        };
                        candidates.push(Finding {
                            id,
                            author: login.to_string(),
                            path: c
                                .get("path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string(),
                            line: c
                                .get("line")
                                .and_then(|v| v.as_i64())
                                .or_else(|| c.get("original_line").and_then(|v| v.as_i64()))
                                .unwrap_or(0),
                            created_at: created_at.to_string(),
                            severity,
                        });
                    }
                }
            }
        }
    }

    let unaddressed: Vec<Finding> = candidates
        .into_iter()
        .filter(|f| {
            let non_bot_replies = replies.get(&f.id);
            let has_reply = non_bot_replies.map(|r| !r.is_empty()).unwrap_or(false);
            if !has_reply {
                return true; // no ack -> unaddressed
            }
            let commit_after = commit_dates.iter().any(|d| ts_after(d, &f.created_at));
            let wontfix = non_bot_replies
                .map(|rs| rs.iter().any(|b| b.to_lowercase().contains(WONTFIX_MARKER)))
                .unwrap_or(false);
            !(commit_after || wontfix)
        })
        .collect();

    let final_ts = if latest_ts.is_empty() {
        "none".to_string()
    } else {
        latest_ts
    };
    (final_ts, unaddressed)
}

// ── fingerprint + fire history ────────────────────────────────────────────────

fn make_fingerprint(
    head_sha: &str,
    pr_state: &str,
    ci_conclusion: &str,
    latest_ts: &str,
) -> String {
    format!("{head_sha}|{pr_state}|{ci_conclusion}|{latest_ts}")
}

/// Count prior loop_check events for this session_id in the project events file.
/// Returns (total_fires, consecutive_unchanged_count, last_fingerprint_in_log).
///
/// `current_fp` is the fingerprint computed this fire (used for streak matching).
/// `last_fp` is the most recent fingerprint recorded in the events log for this
/// session -- used for carry-forward when the gh pre-read fails this fire.
fn read_prior_fires(
    events_path: &Path,
    session_id: &str,
    current_fp: &str,
) -> (u64, u64, Option<String>) {
    let Ok(content) = std::fs::read_to_string(events_path) else {
        return (0, 0, None);
    };

    let mut total: u64 = 0;

    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("loop_check") {
            continue;
        }
        if val.pointer("/data/session_id").and_then(|v| v.as_str()) != Some(session_id) {
            continue;
        }
        total += 1;
    }

    // Calculate consecutive streak from the end (how many recent fires share current_fp)
    // and capture the most recent fp recorded.
    let mut consecutive: u64 = 0;
    let mut last_fp: Option<String> = None;
    for line in content.lines().rev() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("loop_check") {
            continue;
        }
        if val.pointer("/data/session_id").and_then(|v| v.as_str()) != Some(session_id) {
            continue;
        }
        // US4: gh-errored fires are TRANSPARENT to the streak - they neither
        // advance nor reset the consecutive count (their recorded fp is just
        // a carry-forward, not an observation). After an outage clears, the
        // streak resumes from its pre-outage value (AC4-FR).
        if val
            .pointer("/data/fp_read_failed")
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            continue;
        }
        let fp = val
            .pointer("/data/fingerprint")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // Capture the most recent fp (first match in reverse order)
        if last_fp.is_none() && !fp.is_empty() {
            last_fp = Some(fp.to_string());
        }
        if fp == current_fp {
            consecutive += 1;
        } else {
            break;
        }
    }

    (total, consecutive, last_fp)
}

// ── event emission ────────────────────────────────────────────────────────────

/// Envelope struct for target-stream events. Field order ts,type,source,data is
/// preserved because serde_json serializes struct fields in declaration order.
/// Method is named `append_loop_event` (NOT .emit / .emit_fields) so the
/// production-emit scanner test in lib.rs does not capture it and force
/// registration in KNOWN_EVENT_KINDS (which is the Branch B / fno-agents
/// daemon stream, not the target stream that these events belong to).
#[derive(Debug, Serialize)]
struct LoopEventEnvelope<'a> {
    ts: String,
    #[serde(rename = "type")]
    event_type: &'a str,
    source: &'static str,
    data: serde_json::Value,
}

// pub(crate): the `finalize` verb (step 6, ab-f8e5f214) reuses this so its
// `session_finalized` events carry the identical RFC3339 timestamp shape.
pub(crate) fn now_rfc3339_utc() -> String {
    // Seconds precision, Z suffix, as required by the envelope spec.
    let now = chrono::Utc::now();
    now.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Append a target-stream event to a file (O_APPEND, create if missing).
/// Failure is loud on stderr but never fatal to the decision.
fn append_loop_event(path: &Path, event_type: &str, data: serde_json::Value) {
    let env = LoopEventEnvelope {
        ts: now_rfc3339_utc(),
        event_type,
        source: "hook",
        data,
    };
    let Ok(mut line) = serde_json::to_string(&env) else {
        eprintln!("loop-check: failed to serialize event {event_type}");
        return;
    };
    line.push('\n');

    // Create parent dirs
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                eprintln!(
                    "loop-check: failed to write event {event_type} to {}: {e}",
                    path.display()
                );
            }
        }
        Err(e) => {
            eprintln!(
                "loop-check: failed to open events file {}: {e}",
                path.display()
            );
        }
    }
}

/// Append to both project and global event logs.
///
/// pub(crate): the `finalize` verb (step 6, ab-f8e5f214) emits its
/// `session_finalized` / `session_finalize_failed` events through the same
/// writer so they land in both logs with the identical `{ts,type,source,data}`
/// envelope loop-check uses.
pub(crate) fn emit_to_both(
    project_events: &Path,
    global_events: &Path,
    event_type: &str,
    data: serde_json::Value,
) {
    append_loop_event(project_events, event_type, data.clone());
    if project_events != global_events {
        append_loop_event(global_events, event_type, data);
    }
}

// ── cancel sentinel ───────────────────────────────────────────────────────────

fn check_cancel_sentinel(cwd: &Path, created_at: &Option<String>) -> bool {
    let sentinel = cwd.join(".fno/.target-cancelled");
    let tombstone = cwd.join(".fno/.target-cancelled-final");

    for path in &[&tombstone, &sentinel] {
        if !path.exists() {
            continue;
        }
        // Check mtime >= created_at
        if let Some(ca) = created_at {
            if let Ok(parsed_ca) = ca.parse::<DateTime<Utc>>() {
                if let Ok(meta) = std::fs::metadata(path) {
                    if let Ok(modified) = meta.modified() {
                        let sentinel_time: DateTime<Utc> = modified.into();
                        if sentinel_time >= parsed_ca {
                            return true;
                        }
                        // Stale sentinel (older than created_at) -> ignore
                        continue;
                    }
                }
            }
            // Can't read mtime -> treat as present (fail-closed)
            return true;
        }
        return true;
    }
    false
}

// ── budget check ──────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum BudgetTrip {
    WallClock,
    Cost,
}

/// Resolve an `Option<Result<T, String>>` budget cap for use in check_budget.
/// - None => absent (no cap)
/// - Some(Ok(v)) => valid cap value
/// - Some(Err(raw)) => malformed: fail-closed, treat as cap exceeded immediately
enum ResolvedCap<T> {
    Absent,
    Valid(T),
    Malformed(String),
}

fn resolve_cap<T: Copy>(cap: &Option<Result<T, String>>) -> ResolvedCap<T> {
    match cap {
        None => ResolvedCap::Absent,
        Some(Ok(v)) => ResolvedCap::Valid(*v),
        Some(Err(raw)) => ResolvedCap::Malformed(raw.clone()),
    }
}

fn check_budget(
    manifest: &Manifest,
    settings: &Settings,
    now: &DateTime<Utc>,
    ledger_path: &Path,
) -> Option<BudgetTrip> {
    let attended = manifest.attended;

    // Wall-clock cap: prefer manifest value, then settings
    let wall_cap = match resolve_cap(&manifest.budget_wall_clock_cap_minutes) {
        ResolvedCap::Absent => {
            if attended {
                resolve_cap(&settings.attended_wall_cap_minutes)
            } else {
                resolve_cap(&settings.unattended_wall_cap_minutes)
            }
        }
        other => other,
    };

    match wall_cap {
        ResolvedCap::Malformed(raw) => {
            eprintln!("loop-check: malformed budget cap '{raw}' - failing closed; fix the config");
            return Some(BudgetTrip::WallClock);
        }
        ResolvedCap::Valid(cap) => {
            if let Some(ca_str) = &manifest.created_at {
                if let Ok(created) = ca_str.parse::<DateTime<Utc>>() {
                    // Guard against negative elapsed (clock skew / future created_at)
                    let duration = now.signed_duration_since(created);
                    let elapsed_min = if duration.num_minutes() < 0 {
                        0u64
                    } else {
                        duration.num_minutes() as u64
                    };
                    if elapsed_min >= cap {
                        return Some(BudgetTrip::WallClock);
                    }
                }
            }
        }
        ResolvedCap::Absent => {}
    }

    // Cost cap: prefer manifest value, then nested settings, then flat budget_cap
    let cost_cap = match resolve_cap(&manifest.budget_cost_cap_usd) {
        ResolvedCap::Absent => {
            let nested = if attended {
                resolve_cap(&settings.attended_cost_cap_usd)
            } else {
                resolve_cap(&settings.unattended_cost_cap_usd)
            };
            match nested {
                ResolvedCap::Absent => resolve_cap(&settings.flat_budget_cap),
                other => other,
            }
        }
        other => other,
    };

    match cost_cap {
        ResolvedCap::Malformed(raw) => {
            eprintln!("loop-check: malformed budget cap '{raw}' - failing closed; fix the config");
            Some(BudgetTrip::Cost)
        }
        ResolvedCap::Valid(cap) => {
            if let Some(session_id) = &manifest.session_id {
                let cost = session_cost_from_ledger(ledger_path, session_id);
                if cost >= cap {
                    return Some(BudgetTrip::Cost);
                }
            }
            None
        }
        ResolvedCap::Absent => None,
    }
}

// ── main decision function ────────────────────────────────────────────────────

/// CLI flags parsed for `loop-check`. The three required paths are
/// non-optional by construction (fu-4faa3d): `parse_args` validates them and
/// returns `Err` on absence, so downstream code cannot forget to check.
#[derive(Debug)]
struct LoopCheckArgs {
    state_path: PathBuf,
    transcript_path: PathBuf,
    cwd: PathBuf,
    /// Override for the GLOBAL settings file (default $HOME/.fno/
    /// settings.yaml). Tests point it at a nonexistent path for hermeticity.
    global_settings_path: Option<PathBuf>,
    events_path: Option<PathBuf>,
    global_events_path: Option<PathBuf>,
    settings_path: Option<PathBuf>,
    ledger_path: Option<PathBuf>,
    now_override: Option<String>,
    gh_bin: String,
    git_bin: String,
    /// When set, the full Stop-hook JSON payload is read from stdin so
    /// `last_assistant_message` becomes the primary intent channel
    /// (ab-223d2dae). Flag-gated so manual terminal invocations never hang
    /// on a stdin read.
    hook_input_stdin: bool,
}

fn parse_args(args: &[String]) -> Result<LoopCheckArgs, String> {
    let mut state_path: Option<PathBuf> = None;
    let mut transcript_path: Option<PathBuf> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut global_settings_path: Option<PathBuf> = None;
    let mut events_path: Option<PathBuf> = None;
    let mut global_events_path: Option<PathBuf> = None;
    let mut settings_path: Option<PathBuf> = None;
    let mut ledger_path: Option<PathBuf> = None;
    let mut now_override: Option<String> = None;
    let mut gh_bin = std::env::var("FNO_LOOPCHECK_GH_BIN").unwrap_or_else(|_| "gh".to_string());
    let mut git_bin = std::env::var("FNO_LOOPCHECK_GIT_BIN").unwrap_or_else(|_| "git".to_string());
    let mut hook_input_stdin = false;

    // Skip the "loop-check" verb itself if present
    let args = if args.first().map(|s| s.as_str()) == Some("loop-check") {
        &args[1..]
    } else {
        args
    };

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        // Support both --flag value and --flag=value forms. Unknown flags are
        // tolerated (AC5-FR: forward-compat for the shim).
        if let Some(val) = try_flag_value(arg, "--state", args, &mut i) {
            state_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--transcript", args, &mut i) {
            transcript_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--cwd", args, &mut i) {
            cwd = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--events", args, &mut i) {
            events_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--global-events", args, &mut i) {
            global_events_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--settings", args, &mut i) {
            settings_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--global-settings", args, &mut i) {
            global_settings_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--ledger", args, &mut i) {
            ledger_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--now", args, &mut i) {
            now_override = Some(val);
        } else if let Some(val) = try_flag_value(arg, "--gh-bin", args, &mut i) {
            gh_bin = val;
        } else if let Some(val) = try_flag_value(arg, "--git-bin", args, &mut i) {
            git_bin = val;
        } else if arg == "--hook-input-stdin" {
            // Bare boolean flag (no value): try_flag_value would consume the
            // next token as a value, so it is matched directly (ab-223d2dae).
            hook_input_stdin = true;
        }
        i += 1;
    }

    // Required-flag validation lives here (AC5-ERR), not downstream in decide().
    let state_path = state_path.ok_or_else(|| "--state is required".to_string())?;
    let transcript_path = transcript_path.ok_or_else(|| "--transcript is required".to_string())?;
    let cwd = cwd.ok_or_else(|| "--cwd is required".to_string())?;

    Ok(LoopCheckArgs {
        state_path,
        transcript_path,
        cwd,
        global_settings_path,
        events_path,
        global_events_path,
        settings_path,
        ledger_path,
        now_override,
        gh_bin,
        git_bin,
        hook_input_stdin,
    })
}

fn try_flag_value(arg: &str, flag: &str, args: &[String], i: &mut usize) -> Option<String> {
    if arg == flag {
        *i += 1;
        args.get(*i).cloned()
    } else if let Some(val) = arg.strip_prefix(&format!("{flag}=")) {
        Some(val.to_string())
    } else {
        None
    }
}

/// Core decision logic. Returns (exit_code, json_output).
/// Exit 0 always for allow/block; non-zero only for internal/CLI errors.
pub fn decide(args: &[String]) -> (i32, String) {
    // Missing required flags are CLI misuse: exit 2 with the same JSON error
    // shape the pre-refactor inline checks emitted (AC5-ERR).
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(e) => {
            let out = serde_json::json!({ "error": e });
            return (2, out.to_string());
        }
    };

    let state_path = parsed.state_path.clone();
    let transcript_path = parsed.transcript_path.clone();
    let cwd = parsed.cwd.clone();

    // ab-223d2dae (A): the shim feeds the full Stop-hook JSON via stdin so
    // the stopping turn's final text (`last_assistant_message`, recomputed
    // per fire) is readable without racing the transcript flush. Read or
    // parse failures degrade to None (transcript fallback), never an error -
    // but a genuine I/O error is named on stderr (-> the shim's
    // loop-check.stderr.log) so a sustained stdin failure is separable from
    // an ordinary transcript-channel fire in the forensic trail.
    let last_assistant_message: Option<String> = if parsed.hook_input_stdin {
        match std::io::read_to_string(std::io::stdin()) {
            Ok(s) => extract_last_assistant_message(&s),
            Err(e) => {
                eprintln!(
                    "loop-check: failed to read hook input from stdin: {e}; falling back to transcript scan"
                );
                None
            }
        }
    } else {
        None
    };

    // Parse manifest
    let manifest_content = match std::fs::read_to_string(&state_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "loop-check: cannot read state file {}: {e}",
                state_path.display()
            );
            let out = allow_output(
                "allow",
                None,
                "corrupt/missing manifest; allowing exit",
                0,
                None,
            );
            return (0, out);
        }
    };

    let manifest = match parse_manifest(&manifest_content) {
        Some(m) => m,
        None => {
            eprintln!("loop-check: corrupt manifest (no frontmatter)");
            let out = allow_output(
                "allow",
                None,
                "corrupt manifest (no frontmatter); allowing exit",
                0,
                None,
            );
            return (0, out);
        }
    };

    // Lease renewal (x-ba4b): keep this session's node claim fresh on every
    // stop, so a worker whose supervisor pid died mid-run (and now runs under a
    // new pid) never loses its claim to TTL expiry. Best-effort and non-fatal:
    // renew only bumps expires_at when the on-disk holder still matches, so it
    // can never steal, and any failure is a warning that just shortens the lease
    // (the loop never blocks on it). Root=None routes node:<id> to the global
    // claims root inside renew.
    if let (Some(key), Some(holder)) =
        (&manifest.target_claim_key, &manifest.target_claim_holder)
    {
        match crate::claims::renew(key, holder, None) {
            Ok(_) => {}
            Err(e) => eprintln!("loop-check: lease renewal for {key} failed (non-fatal): {e}"),
        }
    }

    // Resolve paths
    let project_events = parsed
        .events_path
        .clone()
        .unwrap_or_else(|| cwd.join(".fno/events.jsonl"));

    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let global_events = parsed
        .global_events_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(&home).join(".fno/events.jsonl"));

    let ledger_path = parsed
        .ledger_path
        .clone()
        .unwrap_or_else(|| cwd.join(".fno/ledger.json"));

    // Parse settings: GLOBAL first, then overlay the project-local file's
    // populated fields (codex P1 on #447: budgets normally live in the
    // global file; a project-local settings.yaml with unrelated content
    // must not silently uncap the session). An explicit --settings path
    // replaces the merge entirely (tests rely on full isolation).
    let settings = if let Some(ref explicit) = parsed.settings_path {
        if let Ok(sc) = std::fs::read_to_string(explicit) {
            parse_settings(&sc)
        } else {
            Settings::default()
        }
    } else {
        let global_path = parsed
            .global_settings_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(&home).join(".fno/settings.yaml"));
        let mut merged = std::fs::read_to_string(&global_path)
            .map(|sc| parse_settings(&sc))
            .unwrap_or_default();
        if let Ok(sc) = std::fs::read_to_string(cwd.join(".fno/settings.yaml")) {
            let local = parse_settings(&sc);
            if local.attended_wall_cap_minutes.is_some() {
                merged.attended_wall_cap_minutes = local.attended_wall_cap_minutes;
            }
            if local.attended_cost_cap_usd.is_some() {
                merged.attended_cost_cap_usd = local.attended_cost_cap_usd;
            }
            if local.unattended_wall_cap_minutes.is_some() {
                merged.unattended_wall_cap_minutes = local.unattended_wall_cap_minutes;
            }
            if local.unattended_cost_cap_usd.is_some() {
                merged.unattended_cost_cap_usd = local.unattended_cost_cap_usd;
            }
            if local.flat_budget_cap.is_some() {
                merged.flat_budget_cap = local.flat_budget_cap;
            }
            if local.ci_declared_none {
                merged.ci_declared_none = true;
            }
            if !local.external_reviewers.is_empty() {
                merged.external_reviewers = local.external_reviewers;
            }
            if local.required_bots.is_some() {
                // Some([]) is a meaningful project-local override (declared
                // no-review-gate), so presence - not non-emptiness - wins.
                merged.required_bots = local.required_bots;
            }
        }
        merged
    };

    // Resolve the must-have-reviewed list once (code default when unset).
    let required_bots = resolved_required_bots(&settings);

    // Now timestamp
    let now: DateTime<Utc> = if let Some(ref s) = parsed.now_override {
        s.parse().unwrap_or_else(|_| Utc::now())
    } else {
        Utc::now()
    };

    let session_id = manifest
        .session_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let emit = |event_type: &str, data: serde_json::Value| {
        emit_to_both(&project_events, &global_events, event_type, data);
    };

    // ── Step 1: cancel sentinel ───────────────────────────────────────────────
    if check_cancel_sentinel(&cwd, &manifest.created_at) {
        emit(
            "termination",
            serde_json::json!({
                "session_id": session_id,
                "reason": "Interrupted",
                "message": "cancel sentinel present"
            }),
        );
        return (
            0,
            allow_output(
                "allow",
                Some(TerminationReason::Interrupted),
                "cancel sentinel present; exiting",
                0,
                None,
            ),
        );
    }

    // ── Step 2: legacy terminal status ───────────────────────────────────────
    if let Some(ref status) = manifest.legacy_status {
        emit(
            "loop_check_legacy_manifest",
            serde_json::json!({
                "session_id": session_id,
                "status": status
            }),
        );
        return (
            0,
            allow_output(
                "allow",
                None,
                &format!("legacy manifest status={status}; allowing exit"),
                0,
                None,
            ),
        );
    }

    // ── Step 3: budget check ──────────────────────────────────────────────────
    if let Some(trip) = check_budget(&manifest, &settings, &now, &ledger_path) {
        let axis = match &trip {
            BudgetTrip::WallClock => "wall_clock",
            BudgetTrip::Cost => "cost",
        };
        emit(
            "termination",
            serde_json::json!({
                "session_id": session_id,
                "reason": "Budget",
                "axis": axis,
                "message": format!("budget exceeded (axis={axis})")
            }),
        );
        return (
            0,
            allow_output(
                "allow",
                Some(TerminationReason::Budget),
                &format!("budget exceeded (axis={axis})"),
                0,
                None,
            ),
        );
    }

    // ── Check gh binary availability ──────────────────────────────────────────
    // Probe by attempting to spawn; if the binary doesn't exist at all (NotFound
    // error kind), treat as absent. Exit-code failures from valid gh commands
    // are handled per-read below as transient failures, not absence.
    let gh_bin = &parsed.gh_bin;
    let gh_available = {
        // Use a harmless read-only probe: `gh auth status` exits non-zero when
        // not logged in, but the binary IS present. We only care about
        // NotFound (binary missing from path entirely).
        match Command::new(gh_bin).arg("--version").output() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => false,
            Ok(_) => true, // any exit code: binary exists
        }
    };

    if !gh_available {
        if !manifest.attended && !manifest.advisory {
            // Unattended + no advisory + no gh -> Interrupted
            emit(
                "termination",
                serde_json::json!({
                    "session_id": session_id,
                    "reason": "Interrupted",
                    "message": "gh binary not found; unattended sessions require gh"
                }),
            );
            return (
                0,
                allow_output(
                    "allow",
                    Some(TerminationReason::Interrupted),
                    "gh binary not found; unattended sessions require gh",
                    0,
                    None,
                ),
            );
        }
        // Attended or declared advisory -> advisory mode (promise + budget only).
        // Budget was already checked above; honor intent here so a promise can
        // terminate an advisory session (AC5-ERR) - gh reads are impossible, so
        // the promise alone is the completion signal.
        emit(
            "loop_advisory_mode",
            serde_json::json!({
                "session_id": session_id,
                "attended": manifest.attended
            }),
        );
        let (advisory_intent, _advisory_intent_source) =
            detect_intent(last_assistant_message.as_deref(), &transcript_path);
        if let Intent::Aborted { ref reason } = advisory_intent {
            emit(
                "termination",
                serde_json::json!({
                    "session_id": session_id,
                    "reason": "Aborted",
                    "message": reason
                }),
            );
            return (
                0,
                allow_output(
                    "allow",
                    Some(TerminationReason::Aborted),
                    "aborted tag detected (advisory mode)",
                    0,
                    None,
                ),
            );
        }
        if advisory_intent == Intent::Promise {
            emit(
                "termination",
                serde_json::json!({
                    "session_id": session_id,
                    "reason": "DoneAdvisory",
                    "message": "promise accepted in advisory mode (gh unavailable)"
                }),
            );
            return (
                0,
                allow_output(
                    "allow",
                    Some(TerminationReason::DoneAdvisory),
                    "promise accepted in advisory mode (gh unavailable)",
                    0,
                    None,
                ),
            );
        }
        return (
            0,
            allow_output(
                "block",
                None,
                "gh binary not found; running in advisory mode (promise + budget only)",
                0,
                None,
            ),
        );
    }

    // ── Step 4: intent + backstop ─────────────────────────────────────────────
    let (intent, intent_source) =
        detect_intent(last_assistant_message.as_deref(), &transcript_path);
    let git_bin = &parsed.git_bin;
    let head_sha = git_head_sha(git_bin, &cwd);

    // Compute fingerprint from a quick PR state read (or "none" if no PR)
    // We do a lightweight fingerprint computation even when intent is None,
    // to check backstop.
    let backstop_n: u64 = if manifest.attended { 5 } else { 3 };

    // Read PR info for fingerprint.
    // On a hard gh failure (spawn error, non-zero exit, unparseable JSON), carry
    // forward the most recent prior fingerprint so the consecutive-unchanged streak
    // continues instead of resetting to "none|none|none" which would mask NoProgress.
    // fp_read_failed is recorded in the event payload for observability.
    let fp_read_result = Command::new(gh_bin)
        .args(["pr", "view", "--json", "state,number,headRefName"])
        .current_dir(&cwd)
        .output();
    let (fp_pr_state, fp_ci, fp_review_ts, fp_read_failed) = match fp_read_result {
        Ok(o) if o.status.success() => {
            let pv: Value = serde_json::from_slice(&o.stdout).unwrap_or(Value::Null);
            let state =
                PrState::from_gh_str(pv.get("state").and_then(|v| v.as_str()).unwrap_or("none"));

            // Get CI
            let ci = match Command::new(gh_bin)
                .args(["pr", "checks", "--json", "name,state,bucket"])
                .current_dir(&cwd)
                .output()
            {
                Ok(co) if co.status.success() => {
                    let cv: Value = serde_json::from_slice(&co.stdout).unwrap_or(Value::Null);
                    compute_ci_conclusion(&cv).unwrap_or(CiConclusion::None)
                }
                _ => CiConclusion::None,
            };

            // Get review ts (skipped for no_external sessions and declared
            // no-review repos, matching the done() Read 3/4 skip)
            let rv_ts = if !manifest.no_external && !required_bots.is_empty() {
                match Command::new(gh_bin)
                    .args(["pr", "view", "--json", "reviews,comments"])
                    .current_dir(&cwd)
                    .output()
                {
                    Ok(ro) if ro.status.success() => {
                        let rv: Value = serde_json::from_slice(&ro.stdout).unwrap_or(Value::Null);
                        compute_review_info(&rv, &required_bots).latest_ts
                    }
                    _ => "none".to_string(),
                }
            } else {
                "none".to_string()
            };

            (state, ci, rv_ts, false)
        }
        // No PR yet: a healthy fire with a "none" fingerprint (world-state,
        // not an outage) - the backstop keeps ticking for a session that
        // never ships a PR.
        Ok(o) if is_no_pr_stderr(&o.stderr) => {
            (PrState::None, CiConclusion::None, "none".to_string(), false)
        }
        // Hard gh failure (spawn error OR non-zero exit): mark as failed; we will
        // carry forward the prior fingerprint after reading the events log.
        _ => (PrState::None, CiConclusion::None, "none".to_string(), true),
    };

    // Build a tentative fingerprint from this fire's gh reads.
    let tentative_fp = make_fingerprint(
        &head_sha,
        fp_pr_state.as_str(),
        &fp_ci.render(),
        &fp_review_ts,
    );

    // Read prior fires. We pass the tentative_fp for streak counting; if the gh
    // read failed we'll override the fingerprint with the carried-forward value below.
    let (prior_fires, consecutive_unchanged, last_recorded_fp) =
        read_prior_fires(&project_events, &session_id, &tentative_fp);

    // If the pre-read gh call hard-failed, carry forward the prior fingerprint
    // (so the streak continues) rather than resetting to "none|none|none".
    let fingerprint = if fp_read_failed {
        last_recorded_fp.unwrap_or(tentative_fp)
    } else {
        tentative_fp
    };

    // Recount consecutive streak with the (possibly carried-forward) fingerprint.
    // We already counted against the tentative_fp; if different, recount from the log.
    let consecutive_unchanged = if fp_read_failed {
        // Re-read the streak against the carried-forward fingerprint.
        let (_, streak, _) = read_prior_fires(&project_events, &session_id, &fingerprint);
        streak
    } else {
        consecutive_unchanged
    };

    let this_fire = prior_fires + 1;
    // consecutive_unchanged counts prior identical fires; adding this fire.
    // US4: a gh-errored fire is itself transparent - the count holds at its
    // prior value instead of advancing (AC4-HP).
    let consecutive_after = if fp_read_failed {
        consecutive_unchanged
    } else {
        consecutive_unchanged + 1
    };

    let backstop_tripped = consecutive_after >= backstop_n;

    // D (ab-223d2dae): probe done() after MUTE_PROBE_N unchanged mute fires
    // instead of waiting out the full backstop streak. A done-but-mute
    // session (all reads pass, no promise as final text) now resolves as a
    // late DonePRGreen in ~2 fires instead of 5/3 - the post-wedge events
    // audit counted 337 backstop fires, i.e. ~1000 no-op confirmation laps.
    // NoProgress still requires the full backstop_n streak (unchanged below),
    // so the grilled-9 backstop semantics are intact; a probed fire whose
    // done() fails simply blocks with the named reason.
    const MUTE_PROBE_N: u64 = 2;

    // Run done() on intent OR backstop OR mute-probe
    if intent != Intent::None || backstop_tripped || consecutive_after >= MUTE_PROBE_N {
        // Handle aborted first
        if let Intent::Aborted { ref reason } = intent {
            emit(
                "termination",
                serde_json::json!({
                    "session_id": session_id,
                    "reason": "Aborted",
                    "message": reason
                }),
            );
            emit(
                "loop_check",
                serde_json::json!({
                    "session_id": session_id,
                    "fingerprint": fingerprint,
                    "fires": this_fire,
                    "consecutive_unchanged": consecutive_after,
                    "decision": "allow",
                    "intent": "aborted",
                    "intent_source": intent_source,
                    "pr_state": fp_pr_state.as_str(),
                    "ci": fp_ci.render(),
                    "reviewed": false,
                    "fp_read_failed": fp_read_failed
                }),
            );
            return (
                0,
                allow_output(
                    "allow",
                    Some(TerminationReason::Aborted),
                    "aborted tag detected",
                    this_fire,
                    Some(fingerprint),
                ),
            );
        }

        // Advisory unit (no_ship or manifest advisory)
        if (manifest.no_ship || manifest.advisory) && intent == Intent::Promise {
            emit(
                "termination",
                serde_json::json!({
                    "session_id": session_id,
                    "reason": "DoneAdvisory",
                    "message": "promise in advisory/no_ship unit"
                }),
            );
            emit(
                "loop_check",
                serde_json::json!({
                    "session_id": session_id,
                    "fingerprint": fingerprint,
                    "fires": this_fire,
                    "consecutive_unchanged": consecutive_after,
                    "decision": "allow",
                    "intent": "promise",
                    "intent_source": intent_source,
                    "pr_state": fp_pr_state.as_str(),
                    "ci": fp_ci.render(),
                    "reviewed": true,
                    "fp_read_failed": fp_read_failed
                }),
            );
            return (
                0,
                allow_output(
                    "allow",
                    Some(TerminationReason::DoneAdvisory),
                    "promise + advisory unit; done",
                    this_fire,
                    Some(fingerprint),
                ),
            );
        }

        // Batched unit (batch-lane Wave 2/3): the node's commits live on a
        // shared batch branch and ship via the batch PR, not its own, so
        // run_done() below would block forever waiting for a per-node PR that
        // never comes. The daemon set `batched: true` at dispatch; a promise
        // here means the member finished committing to the shared branch.
        // Terminal as DoneBatched - deliberately NOT a ship reason, so finalize
        // records the ledger entry but does NOT stamp/graduate the plan (the
        // batch's own `/pr create` graduates it once, for all members). Comes
        // AFTER the advisory arm (a batched unit is not advisory: it sets
        // neither no_ship nor advisory) and BEFORE run_done so no PR is polled.
        if manifest.batched && intent == Intent::Promise {
            emit(
                "termination",
                serde_json::json!({
                    "session_id": session_id,
                    "reason": "DoneBatched",
                    "message": "promise in batched unit; commit landed on shared branch"
                }),
            );
            emit(
                "loop_check",
                serde_json::json!({
                    "session_id": session_id,
                    "fingerprint": fingerprint,
                    "fires": this_fire,
                    "consecutive_unchanged": consecutive_after,
                    "decision": "allow",
                    "intent": "promise",
                    "intent_source": intent_source,
                    "pr_state": fp_pr_state.as_str(),
                    "ci": fp_ci.render(),
                    "reviewed": true,
                    "fp_read_failed": fp_read_failed
                }),
            );
            return (
                0,
                allow_output(
                    "allow",
                    Some(TerminationReason::DoneBatched),
                    "promise + batched unit; commit on shared branch, batch PR ships it",
                    this_fire,
                    Some(fingerprint),
                ),
            );
        }

        // Run done() for code units
        let done_result = run_done(
            gh_bin,
            &cwd,
            settings.ci_declared_none,
            manifest.no_external,
            &required_bots,
            &settings.external_reviewers,
        );

        match done_result {
            Ok(pr_info) => {
                // Read 4's newest activity timestamp folds into the
                // fingerprint's 4th component: a late inline finding advances
                // the fingerprint (re-block, not NoProgress - the codex
                // findings-minutes-after-summary shape). State/CI components
                // stay on the pre-read basis so quiet fires stay comparable.
                // Skipped entirely when the pre-read failed: its stale
                // none|none components would leak into done_fp and manufacture
                // a fingerprint change on a fire US4 declares transparent
                // (sigma-review finding on this branch).
                let (fingerprint, consecutive_after) = if !fp_read_failed {
                    let done_fp = make_fingerprint(
                        &head_sha,
                        fp_pr_state.as_str(),
                        &fp_ci.render(),
                        &max_ts(&fp_review_ts, &pr_info.latest_review_ts),
                    );
                    if done_fp != fingerprint {
                        let (_, streak, _) =
                            read_prior_fires(&project_events, &session_id, &done_fp);
                        (done_fp, streak + 1)
                    } else {
                        (fingerprint, consecutive_after)
                    }
                } else {
                    (fingerprint, consecutive_after)
                };
                let backstop_tripped = consecutive_after >= backstop_n;

                let ci_ok = pr_info.ci_conclusion.is_ok();
                let pr_open = pr_info.state.is_open_or_merged();
                // codex P1 on #447: a green PR must also contain the local
                // HEAD - otherwise unpushed work terminates as DonePRGreen
                // without ever shipping. MERGED PRs are exempt only when the
                // local HEAD matches too; an unpushed commit on top of a
                // merged PR is still unshipped work.
                let head_shipped = !pr_info.head_oid.is_empty() && pr_info.head_oid == head_sha;

                if pr_open && ci_ok && pr_info.reviewed && head_shipped {
                    emit(
                        "termination",
                        serde_json::json!({
                            "session_id": session_id,
                            "reason": "DonePRGreen",
                            "message": format!("PR #{} green and reviewed", pr_info.number)
                        }),
                    );
                    emit(
                        "loop_check",
                        serde_json::json!({
                            "session_id": session_id,
                            "fingerprint": fingerprint,
                            "fires": this_fire,
                            "consecutive_unchanged": consecutive_after,
                            "decision": "allow",
                            "intent": if intent == Intent::Promise { "promise" } else { "backstop" },
                            "intent_source": intent_source,
                            "pr_state": pr_info.state.as_str(),
                            "ci": pr_info.ci_conclusion.render(),
                            "reviewed": pr_info.reviewed,
                            "review_skipped": pr_info.review_skipped,
                            "unaddressed_blocking": pr_info.unaddressed_findings.len(),
                            "fp_read_failed": fp_read_failed
                        }),
                    );
                    return (
                        0,
                        allow_output(
                            "allow",
                            Some(TerminationReason::DonePRGreen),
                            &format!("PR #{} is green and reviewed", pr_info.number),
                            this_fire,
                            Some(fingerprint),
                        ),
                    );
                }

                if backstop_tripped && (!pr_open || !ci_ok || !pr_info.reviewed) {
                    // Backstop tripped + done() false -> NoProgress
                    emit(
                        "termination",
                        serde_json::json!({
                            "session_id": session_id,
                            "reason": "NoProgress",
                            "message": format!("fingerprint unchanged for {} consecutive fires; PR not done", consecutive_after)
                        }),
                    );
                    emit(
                        "loop_check",
                        serde_json::json!({
                            "session_id": session_id,
                            "fingerprint": fingerprint,
                            "fires": this_fire,
                            "consecutive_unchanged": consecutive_after,
                            "decision": "allow",
                            "intent": "backstop",
                            "intent_source": intent_source,
                            "pr_state": pr_info.state.as_str(),
                            "ci": pr_info.ci_conclusion.render(),
                            "reviewed": pr_info.reviewed,
                            "review_skipped": pr_info.review_skipped,
                            "unaddressed_blocking": pr_info.unaddressed_findings.len(),
                            "fp_read_failed": fp_read_failed
                        }),
                    );
                    return (0, allow_output(
                        "allow",
                        Some(TerminationReason::NoProgress),
                        &format!(
                            "fingerprint unchanged for {} fires; HEAD={}, PR={}, CI={}, reviewed={}",
                            consecutive_after, &head_sha[..8.min(head_sha.len())],
                            pr_info.state.as_str(), pr_info.ci_conclusion.render(), pr_info.reviewed
                        ),
                        this_fire,
                        Some(fingerprint),
                    ));
                }

                // done() false on promise -> block with named reason. P2
                // (ab-098967b4): enrich with a loop-boundary inbox nudge.
                let reason = crate::nudge::append_inbox_nudge(
                    &build_block_reason(&pr_info, &head_sha),
                    &cwd,
                    &session_id,
                );
                emit(
                    "loop_check",
                    serde_json::json!({
                        "session_id": session_id,
                        "fingerprint": fingerprint,
                        "fires": this_fire,
                        "consecutive_unchanged": consecutive_after,
                        "decision": "block",
                        "intent": if intent == Intent::Promise { "promise" } else { "none" },
                        "intent_source": intent_source,
                        "pr_state": pr_info.state.as_str(),
                        "ci": pr_info.ci_conclusion.render(),
                        "reviewed": pr_info.reviewed,
                        "review_skipped": pr_info.review_skipped,
                        "unaddressed_blocking": pr_info.unaddressed_findings.len(),
                        "fp_read_failed": fp_read_failed
                    }),
                );
                return (
                    0,
                    allow_output("block", None, &reason, this_fire, Some(fingerprint)),
                );
            }
            Err((failed_read, failed_stderr)) => {
                // US4 (locked decision 6, REVERSES the wedge's behavior): a
                // gh-errored done() read NEVER terminates NoProgress, even
                // with the backstop tripped - a healthy session must not be
                // killed because GitHub blipped. The fire blocks-and-retries
                // and is recorded fp_read_failed=true, keeping it transparent
                // to the streak. Budget remains the sole ceiling during a
                // sustained outage (AC4-EDGE; budget is checked before any
                // gh read, so the outage never makes a session immortal).
                emit(
                    "loop_check_gh_error",
                    serde_json::json!({
                        "session_id": session_id,
                        "read": failed_read,
                        "stderr_tail": failed_stderr
                    }),
                );
                emit(
                    "loop_check",
                    serde_json::json!({
                        "session_id": session_id,
                        "fingerprint": fingerprint,
                        "fires": this_fire,
                        "consecutive_unchanged": consecutive_after,
                        "decision": "block",
                        "intent": if intent == Intent::Promise { "promise" } else { "none" },
                        "intent_source": intent_source,
                        "pr_state": "unknown",
                        "ci": "unknown",
                        "reviewed": false,
                        "fp_read_failed": true
                    }),
                );
                return (
                    0,
                    allow_output(
                        "block",
                        None,
                        &format!("gh read '{failed_read}' failed; retrying next fire"),
                        this_fire,
                        Some(fingerprint),
                    ),
                );
            }
        }
    }

    // ── Step 5: no intent, no backstop -> block, record fingerprint ───────────
    emit(
        "loop_check",
        serde_json::json!({
            "session_id": session_id,
            "fingerprint": fingerprint,
            "fires": this_fire,
            "consecutive_unchanged": consecutive_after,
            "decision": "block",
            "intent": "none",
            "intent_source": intent_source,
            "pr_state": fp_pr_state.as_str(),
            "ci": fp_ci.render(),
            "reviewed": false,
            "fp_read_failed": fp_read_failed
        }),
    );

    // P2 (ab-098967b4): the dominant loop-yield boundary. Enrich the continue
    // message with a one-line inbox nudge so an autonomous loop surfaces mail.
    let continue_msg = crate::nudge::append_inbox_nudge(
        "continue working; no completion signal",
        &cwd,
        &session_id,
    );
    (
        0,
        allow_output("block", None, &continue_msg, this_fire, Some(fingerprint)),
    )
}

fn run_done(
    gh_bin: &str,
    cwd: &Path,
    ci_declared_none: bool,
    no_external: bool,
    required_bots: &[String],
    external_reviewers: &[String],
) -> Result<PrInfo, (String, String)> {
    read_pr_info(
        gh_bin,
        cwd,
        ci_declared_none,
        no_external,
        required_bots,
        external_reviewers,
    )
}

fn build_block_reason(pr: &PrInfo, local_head: &str) -> String {
    if !pr.state.is_open_or_merged() {
        return format!(
            "no PR for HEAD (pr_state={}); keep working",
            pr.state.as_str()
        );
    }

    if !pr.head_oid.is_empty() && pr.head_oid != local_head {
        return format!(
            "PR #{} head {} != local HEAD {}: push the latest commits before completing",
            pr.number,
            &pr.head_oid[..8.min(pr.head_oid.len())],
            &local_head[..8.min(local_head.len())]
        );
    }

    if !pr.ci_conclusion.is_ok() {
        if pr.ci_conclusion == CiConclusion::None {
            return format!(
                "no CI checks found on PR #{}; declare ci.declared_none: true in settings if intentional",
                pr.number
            );
        }
        // Pending is "not green YET", not red. The MUTE_PROBE_N probe
        // (ab-223d2dae) runs done() while CI is commonly still in flight,
        // so a "CI failed" message here would mislead the blocked agent
        // into debugging a nonexistent failure on every quiet fire.
        if pr.ci_conclusion == CiConclusion::Pending {
            return format!(
                "CI still running on PR #{}; wait for it to finish",
                pr.number
            );
        }
        let check_name = match &pr.ci_conclusion {
            CiConclusion::Failure(Some(name)) => name.as_str(),
            _ => "CI",
        };
        return format!("CI red on PR #{}: {} failed", pr.number, check_name);
    }

    if !pr.reviewed {
        if !pr.missing_bots.is_empty() {
            // AC1-UI: name the specific missing bot(s), not a generic
            // "not reviewed".
            return format!(
                "PR #{}: {} has not reviewed",
                pr.number,
                pr.missing_bots.join(", ")
            );
        }
        if !pr.unaddressed_findings.is_empty() {
            // AC2-UI: name the specific finding (path:line) and the remedy.
            let f = &pr.unaddressed_findings[0];
            let more = if pr.unaddressed_findings.len() > 1 {
                format!(" [+{} more]", pr.unaddressed_findings.len() - 1)
            } else {
                String::new()
            };
            return format!(
                "PR #{}: {} {} at {}:{} unaddressed (reply in-thread or wontfix:){}",
                pr.number, f.author, f.severity, f.path, f.line, more
            );
        }
        return format!("PR #{} not yet reviewed by a bot reviewer", pr.number);
    }

    format!("PR #{} done() returned false (unknown reason)", pr.number)
}

fn allow_output(
    decision: &str,
    termination_reason: Option<TerminationReason>,
    message: &str,
    fires: u64,
    fingerprint: Option<String>,
) -> String {
    let out = LoopCheckOutput {
        decision: decision.to_string(),
        termination_reason,
        message: message.to_string(),
        fires,
        fingerprint,
    };
    serde_json::to_string(&out).unwrap_or_else(|_| r#"{"decision":"allow","termination_reason":null,"message":"serialization error","fires":0,"fingerprint":null}"#.to_string())
}

// ── public entry points ───────────────────────────────────────────────────────

/// Entry point called from `bin/client.rs` direct dispatch.
/// Prints JSON to stdout, returns exit code.
pub fn run_loop_check(args: &[String]) -> i32 {
    let (code, json) = decide(args);
    println!("{json}");
    code
}

/// Test-friendly variant that returns (exit_code, json_string) without printing.
/// Used by integration tests in tests/loop_check.rs.
pub fn run_loop_check_capture(args: &[String]) -> (i32, String) {
    decide(args)
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_manifest_minimal() {
        let content =
            "---\nsession_id: abc\ncreated_at: 2026-06-05T00:00:00Z\nattended: true\n---\n";
        let m = parse_manifest(content).unwrap();
        assert_eq!(m.session_id.as_deref(), Some("abc"));
        assert_eq!(m.created_at.as_deref(), Some("2026-06-05T00:00:00Z"));
        assert!(m.attended);
        assert!(m.legacy_status.is_none());
        // Absent claim fields default to None (renewal is then skipped).
        assert!(m.target_claim_key.is_none());
        assert!(m.target_claim_holder.is_none());
    }

    #[test]
    fn parse_manifest_target_claim_fields() {
        // x-ba4b: the node-claim key + holder drive loop-check lease renewal;
        // quotes are stripped like every other string field.
        let content = "---\nsession_id: s1\ntarget_claim_key: \"node:x-ba4b\"\ntarget_claim_holder: \"target-session:s1\"\n---\n";
        let m = parse_manifest(content).unwrap();
        assert_eq!(m.target_claim_key.as_deref(), Some("node:x-ba4b"));
        assert_eq!(m.target_claim_holder.as_deref(), Some("target-session:s1"));
    }

    #[test]
    fn parse_manifest_legacy_complete() {
        let content =
            "---\nsession_id: s\ncreated_at: 2026-06-05T00:00:00Z\nstatus: COMPLETE\n---\n";
        let m = parse_manifest(content).unwrap();
        assert_eq!(m.legacy_status.as_deref(), Some("COMPLETE"));
    }

    #[test]
    fn parse_manifest_legacy_blocked() {
        let content =
            "---\nsession_id: s\ncreated_at: 2026-06-05T00:00:00Z\nstatus: BLOCKED\n---\n";
        let m = parse_manifest(content).unwrap();
        assert_eq!(m.legacy_status.as_deref(), Some("BLOCKED"));
    }

    #[test]
    fn parse_manifest_no_ship() {
        let content = "---\nsession_id: s\ncreated_at: 2026-06-05T00:00:00Z\nno_ship: true\n---\n";
        let m = parse_manifest(content).unwrap();
        assert!(m.no_ship);
        assert!(!m.no_external);
    }

    #[test]
    fn parse_manifest_strips_quotes() {
        // gemini MEDIUM on #447: quoted YAML values must parse identically.
        let content = "---\nsession_id: \"s-quoted\"\ncreated_at: '2026-06-05T00:00:00Z'\n---\n";
        let m = parse_manifest(content).unwrap();
        assert_eq!(m.session_id.as_deref(), Some("s-quoted"));
        assert_eq!(m.created_at.as_deref(), Some("2026-06-05T00:00:00Z"));
    }

    #[test]
    fn parse_settings_four_space_indent() {
        // gemini HIGH on #447: indent unit is derived, not assumed 2-space.
        let yaml = "config:\n    budget:\n        unattended:\n            cost_cap_usd: 7.5\n    ci:\n        declared_none: true\n";
        let s = parse_settings(yaml);
        assert_eq!(s.unattended_cost_cap_usd, Some(Ok(7.5)));
        assert!(s.ci_declared_none);
    }

    #[test]
    fn stderr_tail_multibyte_boundary_no_panic() {
        // gemini HIGH on #447: tail slice must land on a char boundary.
        let mut payload = String::new();
        while payload.len() < 300 {
            payload.push('\u{00e9}'); // 2-byte char so len-200 can split one
        }
        let tail = stderr_tail(payload.as_bytes());
        assert!(tail.len() <= 200);
        assert!(!tail.is_empty());
    }

    #[test]
    fn parse_manifest_attended_default_true() {
        let content = "---\nsession_id: s\ncreated_at: 2026-06-05T00:00:00Z\n---\n";
        let m = parse_manifest(content).unwrap();
        assert!(m.attended, "attended should default to true when absent");
    }

    #[test]
    fn parse_manifest_budget_caps() {
        let content =
            "---\nsession_id: s\ncreated_at: 2026-06-05T00:00:00Z\nbudget_wall_clock_cap_minutes: 120\nbudget_cost_cap_usd: 5.0\n---\n";
        let m = parse_manifest(content).unwrap();
        assert_eq!(m.budget_wall_clock_cap_minutes, Some(Ok(120)));
        assert_eq!(m.budget_cost_cap_usd, Some(Ok(5.0)));
    }

    #[test]
    fn parse_manifest_no_frontmatter_returns_none() {
        let content = "no frontmatter here";
        assert!(parse_manifest(content).is_none());
    }

    #[test]
    fn parse_settings_flat_budget_cap() {
        let yaml = "budget_cap: 2.5\n";
        let s = parse_settings(yaml);
        assert_eq!(s.flat_budget_cap, Some(Ok(2.5)));
    }

    #[test]
    fn parse_settings_nested_budget() {
        let yaml = "config:\n  budget:\n    attended:\n      wall_clock_cap_minutes: 90\n      cost_cap_usd: 10.0\n    unattended:\n      wall_clock_cap_minutes: 60\n      cost_cap_usd: 5.0\n";
        let s = parse_settings(yaml);
        assert_eq!(s.attended_wall_cap_minutes, Some(Ok(90)));
        assert_eq!(s.attended_cost_cap_usd, Some(Ok(10.0)));
        assert_eq!(s.unattended_wall_cap_minutes, Some(Ok(60)));
        assert_eq!(s.unattended_cost_cap_usd, Some(Ok(5.0)));
    }

    #[test]
    fn parse_settings_ci_declared_none() {
        let yaml = "config:\n  ci:\n    declared_none: true\n";
        let s = parse_settings(yaml);
        assert!(s.ci_declared_none);
    }

    #[test]
    fn parse_settings_comments_ignored() {
        let yaml = "# top comment\nbudget_cap: 1.0\n# another\nconfig:\n  # inner\n  ci:\n    declared_none: true\n";
        let s = parse_settings(yaml);
        assert_eq!(s.flat_budget_cap, Some(Ok(1.0)));
        assert!(s.ci_declared_none);
    }

    #[test]
    fn detect_intent_promise() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let line = serde_json::json!({
            "message": {"role": "assistant", "content": "done <promise>COMPLETE</promise>"}
        });
        std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();
        assert_eq!(detect_intent_full(&path), Intent::Promise);
    }

    #[test]
    fn detect_intent_aborted_beats_promise() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        // Last line has aborted (even if earlier had promise, aborted in same msg wins)
        let line = serde_json::json!({
            "message": {"role": "assistant", "content": "<aborted reason=\"user\">done</aborted>"}
        });
        std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();
        assert!(matches!(detect_intent_full(&path), Intent::Aborted { .. }));
    }

    #[test]
    fn detect_intent_tool_result_ignored() {
        // Tool result content with promise-like text should not trigger
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let user_line = serde_json::json!({
            "message": {"role": "user", "content": "<promise>fake</promise>"}
        });
        std::fs::write(&path, serde_json::to_string(&user_line).unwrap() + "\n").unwrap();
        assert_eq!(detect_intent_full(&path), Intent::None);
    }

    #[test]
    fn detect_intent_none_when_no_assistant() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let line = serde_json::json!({"message": {"role": "user", "content": "go"}});
        std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();
        assert_eq!(detect_intent_full(&path), Intent::None);
    }

    #[test]
    fn detect_intent_array_content_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let line = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "<promise>done</promise>"},
                    {"type": "tool_use", "name": "Bash"}
                ]
            }
        });
        std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();
        assert_eq!(detect_intent_full(&path), Intent::Promise);
    }

    #[test]
    fn extract_last_assistant_message_plain_string() {
        let payload = r#"{"transcript_path":"/t.jsonl","last_assistant_message":"  done <promise>MISSION COMPLETE: x</promise>  "}"#;
        assert_eq!(
            extract_last_assistant_message(payload).as_deref(),
            Some("done <promise>MISSION COMPLETE: x</promise>")
        );
    }

    #[test]
    fn extract_last_assistant_message_degrades_to_none() {
        // Missing field, malformed JSON, non-string value, and empty/blank
        // strings all degrade to None (transcript fallback), never an error.
        assert_eq!(
            extract_last_assistant_message(r#"{"transcript_path":"/t.jsonl"}"#),
            None
        );
        assert_eq!(extract_last_assistant_message("not json {"), None);
        assert_eq!(
            extract_last_assistant_message(r#"{"last_assistant_message":{"text":"obj"}}"#),
            None
        );
        assert_eq!(
            extract_last_assistant_message(r#"{"last_assistant_message":"   "}"#),
            None
        );
    }

    #[test]
    fn detect_intent_payload_promise_wins_over_stale_transcript() {
        // AC2-HP: at the promise turn's own fire the transcript does NOT yet
        // contain the final message; the payload alone must carry the intent.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let line = serde_json::json!({
            "message": {"role": "assistant", "content": "still working on it"}
        });
        std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();
        let (intent, source) =
            detect_intent(Some("<promise>MISSION COMPLETE: done</promise>"), &path);
        assert_eq!(intent, Intent::Promise);
        assert_eq!(source, "payload");
    }

    #[test]
    fn detect_intent_payload_no_tag_is_authoritative() {
        // A tag-less payload is the stopping turn's final text; it must NOT
        // fall through to the transcript (stale-promise containment).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let line = serde_json::json!({
            "message": {"role": "assistant", "content": "<promise>old stale promise</promise>"}
        });
        std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();
        let (intent, source) = detect_intent(Some("moving on to other work"), &path);
        assert_eq!(intent, Intent::None);
        assert_eq!(source, "payload");
    }

    #[test]
    fn detect_intent_payload_aborted_beats_promise() {
        let (intent, source) = detect_intent(
            Some("<promise>done</promise> <aborted reason=\"kill\">stop</aborted>"),
            Path::new("/nonexistent"),
        );
        assert!(matches!(intent, Intent::Aborted { ref reason } if reason == "kill"));
        assert_eq!(source, "payload");
    }

    #[test]
    fn detect_intent_absent_payload_falls_back_to_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let line = serde_json::json!({
            "message": {"role": "assistant", "content": "<promise>COMPLETE</promise>"}
        });
        std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();
        let (intent, source) = detect_intent(None, &path);
        assert_eq!(intent, Intent::Promise);
        assert_eq!(source, "transcript");
    }

    #[test]
    fn detect_intent_lookback_finds_promise_behind_block_feedback() {
        // AC2-EDGE ("the block destroys the evidence"): promise 3 assistant
        // text entries back - block feedback reply + a follow-up on top -
        // must still be detected by the bounded fallback scan.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let mut content = String::new();
        for text in [
            "<promise>MISSION COMPLETE: shipped</promise>",
            "acknowledged the block; checking CI",
            "CI is still pending, waiting",
        ] {
            let line = serde_json::json!({
                "message": {"role": "assistant", "content": text}
            });
            content.push_str(&serde_json::to_string(&line).unwrap());
            content.push('\n');
        }
        std::fs::write(&path, content).unwrap();
        assert_eq!(detect_intent_full(&path), Intent::Promise);
    }

    #[test]
    fn detect_intent_lookback_bound_holds() {
        // AC2-EDGE ("grill the stale-promise edge"): a promise older than
        // INTENT_LOOKBACK_ENTRIES assistant text entries must NOT ride the
        // window.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let mut content = String::new();
        let line = serde_json::json!({
            "message": {"role": "assistant", "content": "<promise>stale</promise>"}
        });
        content.push_str(&serde_json::to_string(&line).unwrap());
        content.push('\n');
        for i in 0..INTENT_LOOKBACK_ENTRIES {
            let line = serde_json::json!({
                "message": {"role": "assistant", "content": format!("pivoted work step {i}")}
            });
            content.push_str(&serde_json::to_string(&line).unwrap());
            content.push('\n');
        }
        std::fs::write(&path, content).unwrap();
        assert_eq!(detect_intent_full(&path), Intent::None);
    }

    #[test]
    fn parse_args_hook_input_stdin_flag() {
        let args: Vec<String> = [
            "loop-check",
            "--state",
            "/s.md",
            "--transcript",
            "/t.jsonl",
            "--cwd",
            "/w",
            "--hook-input-stdin",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let parsed = parse_args(&args).unwrap();
        assert!(parsed.hook_input_stdin);
        // Bare flag must not swallow a following flag as its value.
        assert_eq!(parsed.cwd, PathBuf::from("/w"));
    }

    #[test]
    fn block_reason_pending_ci_is_not_red() {
        // The MUTE_PROBE_N probe runs done() while CI is often still in
        // flight; a Pending conclusion must read as "still running", never
        // as the misleading "CI red ... failed" (observed live on PR #455).
        let pr = PrInfo {
            state: PrState::Open,
            number: 455,
            head_oid: "abc".to_string(),
            ci_conclusion: CiConclusion::Pending,
            latest_review_ts: "none".to_string(),
            reviewed: false,
            missing_bots: vec![],
            unaddressed_findings: vec![],
            review_skipped: false,
        };
        let reason = build_block_reason(&pr, "abc");
        assert!(
            reason.contains("still running"),
            "pending CI must not read as red; got: {reason}"
        );
        assert!(!reason.contains("failed"), "got: {reason}");
    }

    #[test]
    fn fingerprint_format() {
        let fp = make_fingerprint("sha123", "OPEN", "SUCCESS", "2026-06-05T01:00:00Z");
        assert_eq!(fp, "sha123|OPEN|SUCCESS|2026-06-05T01:00:00Z");
    }

    #[test]
    fn ci_conclusion_failure_extracts_name() {
        let checks = serde_json::json!([
            {"name": "unit-tests", "state": "FAILURE", "bucket": "fail"}
        ]);
        let result = compute_ci_conclusion(&checks).unwrap();
        assert_eq!(
            result,
            CiConclusion::Failure(Some("unit-tests".to_string()))
        );
        let rendered = result.render();
        assert!(rendered.starts_with("FAILURE:"), "got: {rendered}");
        assert!(rendered.contains("unit-tests"), "got: {rendered}");
    }

    /// A cancelled check is a failure, and a skipping sibling never masks it.
    #[test]
    fn ci_conclusion_cancel_is_failure() {
        let checks = serde_json::json!([
            {"name": "ci", "state": "SUCCESS", "bucket": "pass"},
            {"name": "deploy", "state": "CANCELLED", "bucket": "cancel"}
        ]);
        assert_eq!(
            compute_ci_conclusion(&checks).unwrap(),
            CiConclusion::Failure(Some("deploy".to_string()))
        );
    }

    /// pass + skipping rolls up green; a pending bucket blocks it.
    #[test]
    fn ci_conclusion_bucket_vocabulary() {
        let green = serde_json::json!([
            {"name": "ci", "state": "SUCCESS", "bucket": "pass"},
            {"name": "publish", "state": "SKIPPED", "bucket": "skipping"}
        ]);
        assert_eq!(
            compute_ci_conclusion(&green).unwrap(),
            CiConclusion::Success
        );

        let pending = serde_json::json!([
            {"name": "ci", "state": "SUCCESS", "bucket": "pass"},
            {"name": "smoke", "state": "IN_PROGRESS", "bucket": "pending"}
        ]);
        assert_eq!(
            compute_ci_conclusion(&pending).unwrap(),
            CiConclusion::Pending
        );
    }

    /// An unknown or missing bucket fails closed as Pending, never green.
    #[test]
    fn ci_conclusion_unknown_bucket_fails_closed() {
        let unknown = serde_json::json!([
            {"name": "ci", "state": "SUCCESS", "bucket": "mystery"}
        ]);
        assert_eq!(
            compute_ci_conclusion(&unknown).unwrap(),
            CiConclusion::Pending
        );

        let missing = serde_json::json!([{"name": "ci", "state": "SUCCESS"}]);
        assert_eq!(
            compute_ci_conclusion(&missing).unwrap(),
            CiConclusion::Pending
        );
    }

    #[test]
    fn ci_conclusion_empty_returns_none() {
        let checks = serde_json::json!([]);
        let result = compute_ci_conclusion(&checks).unwrap();
        assert_eq!(result, CiConclusion::None);
        assert_eq!(result.render(), "none");
    }

    #[test]
    fn ci_conclusion_all_success() {
        let checks = serde_json::json!([
            {"name": "ci", "state": "SUCCESS", "bucket": "pass"}
        ]);
        let result = compute_ci_conclusion(&checks).unwrap();
        assert_eq!(result, CiConclusion::Success);
        assert_eq!(result.render(), "SUCCESS");
    }

    /// AC5-HP: enums parse known gh strings.
    #[test]
    fn pr_state_parses_known_gh_strings() {
        assert_eq!(PrState::from_gh_str("OPEN"), PrState::Open);
        assert_eq!(PrState::from_gh_str("MERGED"), PrState::Merged);
        assert_eq!(PrState::from_gh_str("CLOSED"), PrState::Closed);
        assert_eq!(PrState::from_gh_str("none"), PrState::None);
    }

    /// AC5-EDGE: an unexpected gh state string maps to PrState::None
    /// (fail-closed), never panics.
    #[test]
    fn pr_state_unknown_string_fails_closed() {
        assert_eq!(PrState::from_gh_str("DRAFT"), PrState::None);
        assert_eq!(PrState::from_gh_str(""), PrState::None);
        assert_eq!(PrState::from_gh_str("open"), PrState::None);
    }

    /// AC5-UI: as_str/render reproduce the exact legacy fingerprint vocabulary.
    #[test]
    fn enum_rendering_byte_identical_to_legacy_strings() {
        assert_eq!(PrState::Open.as_str(), "OPEN");
        assert_eq!(PrState::Merged.as_str(), "MERGED");
        assert_eq!(PrState::Closed.as_str(), "CLOSED");
        assert_eq!(PrState::None.as_str(), "none");
        assert_eq!(CiConclusion::Success.render(), "SUCCESS");
        assert_eq!(
            CiConclusion::Failure(Some("lint".into())).render(),
            "FAILURE:lint"
        );
        assert_eq!(CiConclusion::Failure(None).render(), "FAILURE");
        assert_eq!(CiConclusion::Pending.render(), "PENDING");
        assert_eq!(CiConclusion::Skipped.render(), "skipped");
        assert_eq!(CiConclusion::None.render(), "none");
    }

    /// AC5-ERR: required flags validated in parse_args, which returns Err.
    #[test]
    fn parse_args_missing_required_flags_err() {
        let no_state: Vec<String> = vec![
            "loop-check".into(),
            "--transcript".into(),
            "/t".into(),
            "--cwd".into(),
            "/c".into(),
        ];
        assert_eq!(
            parse_args(&no_state).unwrap_err(),
            "--state is required".to_string()
        );

        let no_transcript: Vec<String> = vec!["loop-check".into(), "--state".into(), "/s".into()];
        assert_eq!(
            parse_args(&no_transcript).unwrap_err(),
            "--transcript is required".to_string()
        );

        let no_cwd: Vec<String> = vec![
            "loop-check".into(),
            "--state".into(),
            "/s".into(),
            "--transcript".into(),
            "/t".into(),
        ];
        assert_eq!(
            parse_args(&no_cwd).unwrap_err(),
            "--cwd is required".to_string()
        );
    }

    /// AC5-FR: an unknown flag is tolerated (forward-compat for the shim).
    #[test]
    fn parse_args_unknown_flag_tolerated() {
        let args: Vec<String> = vec![
            "loop-check".into(),
            "--state".into(),
            "/s".into(),
            "--transcript".into(),
            "/t".into(),
            "--cwd".into(),
            "/c".into(),
            "--future-flag=whatever".into(),
            "--another-unknown".into(),
            "value".into(),
        ];
        let parsed = parse_args(&args).expect("unknown flags must be ignored");
        assert_eq!(parsed.state_path, PathBuf::from("/s"));
        assert_eq!(parsed.transcript_path, PathBuf::from("/t"));
        assert_eq!(parsed.cwd, PathBuf::from("/c"));
    }

    #[test]
    fn budget_flat_key_enforces_cost_cap_ab41b13d9d() {
        // Prove the flat budget_cap key enforces as cost cap for BOTH attended and
        // unattended - this is the ab-41b13d9d fold-in test.
        let settings_yaml = "budget_cap: 0.10\n";
        let settings = parse_settings(settings_yaml);
        assert_eq!(settings.flat_budget_cap, Some(Ok(0.10)));
        // No nested blocks configured
        assert!(settings.attended_cost_cap_usd.is_none());
        assert!(settings.unattended_cost_cap_usd.is_none());
        // The budget resolver picks flat_budget_cap as cost cap fallback
        // for both attended=true and attended=false (tested in check_budget)

        let manifest_att = Manifest {
            session_id: Some("s1".into()),
            created_at: Some("2026-06-05T00:00:00Z".into()),
            attended: true,
            ..Default::default()
        };
        let manifest_unatt = Manifest {
            session_id: Some("s1".into()),
            created_at: Some("2026-06-05T00:00:00Z".into()),
            attended: false,
            ..Default::default()
        };

        // Ledger with cost > 0.10
        let tmp = tempfile::tempdir().unwrap();
        let ledger = tmp.path().join("ledger.json");
        std::fs::write(&ledger, r#"[{"session_id":"s1","cost_usd":0.50}]"#).unwrap();

        let now: DateTime<Utc> = "2026-06-05T01:00:00Z".parse().unwrap();

        assert_eq!(
            check_budget(&manifest_att, &settings, &now, &ledger),
            Some(BudgetTrip::Cost),
            "flat budget_cap must enforce for attended"
        );
        assert_eq!(
            check_budget(&manifest_unatt, &settings, &now, &ledger),
            Some(BudgetTrip::Cost),
            "flat budget_cap must enforce for unattended"
        );
    }

    #[test]
    fn is_bot_reviewer_known_patterns() {
        assert!(is_bot_reviewer("gemini-code-assist[bot]", &[]));
        assert!(is_bot_reviewer("chatgpt-codex-connector", &[]));
        assert!(is_bot_reviewer("some-bot[bot]", &[]));
        assert!(!is_bot_reviewer("human-reviewer", &[]));
    }

    #[test]
    fn is_bot_reviewer_with_external_list() {
        let external = vec!["my-bot".to_string()];
        // "my-bot" is a substring of "my-bot" -> match via configured list
        assert!(is_bot_reviewer("my-bot", &external));
        // "other-bot[bot]" doesn't match "my-bot" substring, but falls back to
        // the [bot] suffix heuristic (configured list must not make reviewed unreachable)
        assert!(is_bot_reviewer("other-bot[bot]", &external));
    }

    #[test]
    fn session_cost_from_ledger_sums_session_only() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = tmp.path().join("l.json");
        std::fs::write(
            &ledger,
            r#"[{"session_id":"a","cost_usd":1.0},{"session_id":"b","cost_usd":0.5},{"session_id":"a","cost_usd":0.25}]"#,
        )
        .unwrap();
        let cost = session_cost_from_ledger(&ledger, "a");
        assert!((cost - 1.25).abs() < 0.001, "expected 1.25, got {cost}");
    }

    #[test]
    fn session_cost_missing_ledger_returns_zero() {
        let cost = session_cost_from_ledger(Path::new("/nonexistent/l.json"), "s");
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn allow_output_serializes_correctly() {
        let json = allow_output(
            "allow",
            Some(TerminationReason::DonePRGreen),
            "done",
            3,
            Some("fp".into()),
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["decision"], "allow");
        // Verify variant names serialize byte-identically to the spec strings.
        assert_eq!(v["termination_reason"], "DonePRGreen");
        assert_eq!(v["fires"], 3);
        assert_eq!(v["fingerprint"], "fp");
    }

    #[test]
    fn allow_output_null_termination_reason() {
        let json = allow_output("block", None, "continue", 1, None);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["termination_reason"].is_null());
        assert!(v["fingerprint"].is_null());
    }

    #[test]
    fn termination_reason_variant_names_byte_identical() {
        // Fix 6: all TerminationReason variants must serialize to the exact strings
        // the spec names - no rename attributes applied.
        let cases = [
            (TerminationReason::DonePRGreen, "DonePRGreen"),
            (TerminationReason::DoneAdvisory, "DoneAdvisory"),
            (TerminationReason::NoWork, "NoWork"),
            (TerminationReason::Budget, "Budget"),
            (TerminationReason::NoProgress, "NoProgress"),
            (TerminationReason::Interrupted, "Interrupted"),
            (TerminationReason::Aborted, "Aborted"),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            // serde serializes enum unit variants as "\"VariantName\""
            assert_eq!(
                json,
                format!("\"{expected}\""),
                "variant {expected} serialized incorrectly"
            );
        }
    }

    #[test]
    fn manifest_default_attended_is_true() {
        // Fix 7: manual Default impl must set attended=true (derive would give false)
        let m = Manifest::default();
        assert!(m.attended, "Manifest::default() must have attended=true");
        assert!(!m.advisory);
        assert!(!m.no_ship);
        assert!(!m.no_external);
        assert!(m.session_id.is_none());
        assert!(m.budget_cost_cap_usd.is_none());
        assert!(m.budget_wall_clock_cap_minutes.is_none());
    }

    #[test]
    fn parse_manifest_malformed_cost_cap_fail_closed() {
        // Fix 2: a present but unparseable cost cap must be Err (fail-closed)
        let content =
            "---\nsession_id: s\ncreated_at: 2026-06-05T00:00:00Z\nbudget_cost_cap_usd: 5.OO\n---\n";
        let m = parse_manifest(content).unwrap();
        assert!(
            matches!(m.budget_cost_cap_usd, Some(Err(_))),
            "malformed cost cap must be Some(Err(...))"
        );
    }

    #[test]
    fn parse_manifest_malformed_wall_cap_fail_closed() {
        let content =
            "---\nsession_id: s\ncreated_at: 2026-06-05T00:00:00Z\nbudget_wall_clock_cap_minutes: abc\n---\n";
        let m = parse_manifest(content).unwrap();
        assert!(
            matches!(m.budget_wall_clock_cap_minutes, Some(Err(_))),
            "malformed wall cap must be Some(Err(...))"
        );
    }

    #[test]
    fn parse_settings_malformed_flat_cap_fail_closed() {
        let yaml = "budget_cap: not_a_number\n";
        let s = parse_settings(yaml);
        assert!(
            matches!(s.flat_budget_cap, Some(Err(_))),
            "malformed flat_budget_cap must be Some(Err(...))"
        );
    }

    #[test]
    fn check_budget_malformed_cost_cap_trips_budget() {
        // Fix 2: malformed cap in manifest -> Budget termination (fail-closed)
        let m = Manifest {
            session_id: Some("s".into()),
            created_at: Some("2026-06-05T00:00:00Z".into()),
            budget_cost_cap_usd: Some(Err("5.OO".into())),
            ..Default::default()
        };
        let s = Settings::default();
        let now: DateTime<Utc> = "2026-06-05T01:00:00Z".parse().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let ledger = tmp.path().join("ledger.json");
        std::fs::write(&ledger, r#"[{"session_id":"s","cost_usd":0.0}]"#).unwrap();
        assert_eq!(
            check_budget(&m, &s, &now, &ledger),
            Some(BudgetTrip::Cost),
            "malformed cost cap must fail closed"
        );
    }

    #[test]
    fn check_budget_absent_cap_is_unlimited() {
        // ABSENT caps stay unlimited - must not trip
        let m = Manifest {
            session_id: Some("s".into()),
            created_at: Some("2026-06-05T00:00:00Z".into()),
            ..Default::default()
        };
        let s = Settings::default();
        let now: DateTime<Utc> = "2026-06-05T01:00:00Z".parse().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let ledger = tmp.path().join("ledger.json");
        std::fs::write(&ledger, r#"[{"session_id":"s","cost_usd":9999.0}]"#).unwrap();
        assert_eq!(
            check_budget(&m, &s, &now, &ledger),
            None,
            "absent cap must be unlimited"
        );
    }

    #[test]
    fn check_budget_negative_elapsed_no_trip() {
        // Fix 3: created_at in the future (clock skew) -> elapsed=0 -> no wall-clock trip
        let m = Manifest {
            session_id: Some("s".into()),
            // created_at is 1 hour in the future
            created_at: Some("2026-06-05T02:00:00Z".into()),
            budget_wall_clock_cap_minutes: Some(Ok(30)),
            ..Default::default()
        };
        let s = Settings::default();
        // now is earlier than created_at
        let now: DateTime<Utc> = "2026-06-05T01:00:00Z".parse().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let ledger = tmp.path().join("ledger.json");
        std::fs::write(&ledger, "[]").unwrap();
        assert_eq!(
            check_budget(&m, &s, &now, &ledger),
            None,
            "negative elapsed (future created_at) must not trip wall clock cap"
        );
    }

    #[test]
    fn is_bot_reviewer_configured_short_names_match_real_logins() {
        // Fix 1: configured entries use substring matching.
        // "gemini" (short config name) must match "gemini-code-assist[bot]"
        // "codex" must match "chatgpt-codex-connector"
        let external = vec!["gemini".to_string(), "codex".to_string()];
        assert!(
            is_bot_reviewer("gemini-code-assist[bot]", &external),
            "gemini short name must substring-match gemini-code-assist[bot]"
        );
        assert!(
            is_bot_reviewer("chatgpt-codex-connector", &external),
            "codex short name must substring-match chatgpt-codex-connector"
        );
    }

    #[test]
    fn is_bot_reviewer_configured_list_falls_back_to_bot_heuristic() {
        // Fix 1: when configured list has [some-human] but a bot review arrives,
        // fallback to endswith-[bot] heuristic so reviewed remains reachable.
        let external = vec!["some-human".to_string()];
        assert!(
            is_bot_reviewer("gemini-code-assist[bot]", &external),
            "configured list with no match must still fall back to [bot] heuristic"
        );
    }

    #[test]
    fn is_bot_reviewer_empty_config_human_only_returns_false() {
        // Fix 1: empty config + human-only review -> false
        assert!(
            !is_bot_reviewer("alice-the-human", &[]),
            "human reviewer with empty config must return false"
        );
    }

    // ── step 2: required_bots parsing + resolution (US1/US3) ────────────────

    #[test]
    fn parse_settings_required_bots_block_list() {
        let yaml = "config:\n  review:\n    required_bots:\n      - chatgpt-codex-connector\n      - gemini-code-assist\n";
        let s = parse_settings(yaml);
        assert_eq!(
            s.required_bots,
            Some(vec![
                "chatgpt-codex-connector".to_string(),
                "gemini-code-assist".to_string()
            ])
        );
    }

    #[test]
    fn parse_settings_required_bots_inline_empty_is_declared_empty() {
        // The explicit [] form is the ONLY way to declare the no-review-gate
        // path (US3, locked decision 2).
        let yaml = "config:\n  review:\n    required_bots: []\n";
        let s = parse_settings(yaml);
        assert_eq!(s.required_bots, Some(Vec::new()));
    }

    #[test]
    fn parse_settings_required_bots_inline_list() {
        let yaml = "config:\n  review:\n    required_bots: [\"codex\", 'gemini']\n";
        let s = parse_settings(yaml);
        assert_eq!(
            s.required_bots,
            Some(vec!["codex".to_string(), "gemini".to_string()])
        );
    }

    /// AC3-ERR: a non-list value fails closed to the code default (None).
    #[test]
    fn parse_settings_required_bots_scalar_malformed_defaults() {
        let yaml = "config:\n  review:\n    required_bots: gemini\n";
        let s = parse_settings(yaml);
        assert_eq!(s.required_bots, None, "scalar must fail closed to default");
    }

    /// A bare `required_bots:` key with no items is malformed (YAML null, not
    /// []), so it must NOT accidentally disable the review gate.
    #[test]
    fn parse_settings_required_bots_bare_key_no_items_defaults() {
        let yaml = "config:\n  review:\n    required_bots:\n  ci:\n    declared_none: true\n";
        let s = parse_settings(yaml);
        assert_eq!(
            s.required_bots, None,
            "bare key must fail closed to default"
        );
        assert!(s.ci_declared_none, "following keys still parse");
    }

    /// codex P2 on #448: YAML inline comments must not change the parsed
    /// value - `required_bots: []  # no review gate` is still the declared
    /// empty form, and commented list forms still parse.
    #[test]
    fn parse_settings_required_bots_inline_comments_stripped() {
        let empty = parse_settings("config:\n  review:\n    required_bots: []  # no review gate\n");
        assert_eq!(empty.required_bots, Some(Vec::new()));

        let inline = parse_settings(
            "config:\n  review:\n    required_bots: [chatgpt-codex-connector] # required\n",
        );
        assert_eq!(
            inline.required_bots,
            Some(vec!["chatgpt-codex-connector".to_string()])
        );

        let block = parse_settings(
            "config:\n  review:\n    required_bots: # the gate\n      - chatgpt-codex-connector # codex\n",
        );
        assert_eq!(
            block.required_bots,
            Some(vec!["chatgpt-codex-connector".to_string()])
        );

        // A scalar with a comment is still malformed -> default.
        let scalar = parse_settings("config:\n  review:\n    required_bots: gemini # oops\n");
        assert_eq!(scalar.required_bots, None);
    }

    #[test]
    fn parse_settings_required_bots_four_space_indent() {
        let yaml =
            "config:\n    review:\n        required_bots:\n            - chatgpt-codex-connector\n";
        let s = parse_settings(yaml);
        assert_eq!(
            s.required_bots,
            Some(vec!["chatgpt-codex-connector".to_string()])
        );
    }

    #[test]
    fn resolved_required_bots_default_is_empty() {
        // Fresh-install default: no required review bot, so a clone with no
        // review configuration is not blocked waiting for a bot it never set up.
        let s = Settings::default();
        assert!(
            resolved_required_bots(&s).is_empty(),
            "absent required_bots config must resolve to no review gate"
        );
    }

    #[test]
    fn resolved_required_bots_explicit_list_wins() {
        let s = Settings {
            required_bots: Some(vec!["my-bot".to_string()]),
            ..Default::default()
        };
        assert_eq!(resolved_required_bots(&s), vec!["my-bot".to_string()]);
        let empty = Settings {
            required_bots: Some(Vec::new()),
            ..Default::default()
        };
        assert!(resolved_required_bots(&empty).is_empty());
    }

    #[test]
    fn login_matches_bot_cases() {
        // Full login, [bot]-suffixed login, and short config names all match.
        assert!(login_matches_bot(
            "chatgpt-codex-connector",
            "chatgpt-codex-connector"
        ));
        assert!(login_matches_bot(
            "chatgpt-codex-connector[bot]",
            "chatgpt-codex-connector"
        ));
        assert!(login_matches_bot("chatgpt-codex-connector", "codex"));
        assert!(login_matches_bot("Gemini-Code-Assist[bot]", "gemini"));
        assert!(!login_matches_bot("alice-the-human", "codex"));
        // Empty config entry must never match every login.
        assert!(!login_matches_bot("anyone", ""));
    }

    #[test]
    fn compute_review_info_per_bot_verdict() {
        let required = vec![
            "chatgpt-codex-connector".to_string(),
            "gemini-code-assist".to_string(),
        ];
        // Only codex posted a completed pass (COMMENTED counts).
        let json = serde_json::json!({
            "reviews": [
                {"author": {"login": "chatgpt-codex-connector"}, "state": "COMMENTED",
                 "submittedAt": "2026-06-05T01:00:00Z"}
            ],
            "comments": []
        });
        let info = compute_review_info(&json, &required);
        assert!(!info.all_required_passed());
        assert_eq!(info.missing_bots, vec!["gemini-code-assist".to_string()]);
        assert_eq!(info.latest_ts, "2026-06-05T01:00:00Z");
    }

    #[test]
    fn compute_review_info_empty_state_not_a_pass() {
        // A review row with an empty state is not a completed pass.
        let required = vec!["chatgpt-codex-connector".to_string()];
        let json = serde_json::json!({
            "reviews": [
                {"author": {"login": "chatgpt-codex-connector"}, "state": "",
                 "submittedAt": "2026-06-05T01:00:00Z"}
            ],
            "comments": []
        });
        let info = compute_review_info(&json, &required);
        assert!(!info.all_required_passed());
    }

    // ── step 2: inline findings + severity + addressed (US2) ────────────────

    #[test]
    fn blocking_severity_codex_p1_both_forms() {
        // The exact markup codex emits (pinned from PR #447).
        assert_eq!(
            blocking_severity("![P1 Badge](https://img.shields.io/badge/P1-orange?style=flat) Bug"),
            Some("P1")
        );
        // Alt-text only and URL only each match.
        assert_eq!(blocking_severity("![P1 Badge] something"), Some("P1"));
        assert_eq!(
            blocking_severity("see https://img.shields.io/badge/P1-orange"),
            Some("P1")
        );
    }

    #[test]
    fn blocking_severity_codex_p2_p3_advisory() {
        assert_eq!(
            blocking_severity("![P2 Badge](https://img.shields.io/badge/P2-yellow) nit"),
            None
        );
        assert_eq!(
            blocking_severity("![P3 Badge](https://img.shields.io/badge/P3-green) nit"),
            None
        );
    }

    #[test]
    fn blocking_severity_gemini_critical_high_blocking() {
        assert_eq!(
            blocking_severity(
                "![critical](https://www.gstatic.com/codereviewagent/critical-priority.svg) bad"
            ),
            Some("critical")
        );
        assert_eq!(
            blocking_severity(
                "![high](https://www.gstatic.com/codereviewagent/high-priority.svg) bad"
            ),
            Some("high")
        );
    }

    #[test]
    fn blocking_severity_gemini_medium_low_advisory() {
        assert_eq!(
            blocking_severity(
                "![medium](https://www.gstatic.com/codereviewagent/medium-priority.svg) hmm"
            ),
            None
        );
        assert_eq!(
            blocking_severity(
                "![low](https://www.gstatic.com/codereviewagent/low-priority.svg) hmm"
            ),
            None
        );
    }

    /// Boundaries: unrecognized / absent severity tokens classify advisory,
    /// never blocking (locked decision 4).
    #[test]
    fn blocking_severity_unparseable_is_advisory() {
        assert_eq!(blocking_severity("just a comment with no badge"), None);
        assert_eq!(blocking_severity(""), None);
        assert_eq!(blocking_severity("P1 mentioned in prose only"), None);
    }

    #[test]
    fn max_ts_none_handling() {
        assert_eq!(
            max_ts("none", "2026-06-05T01:00:00Z"),
            "2026-06-05T01:00:00Z"
        );
        assert_eq!(
            max_ts("2026-06-05T01:00:00Z", "none"),
            "2026-06-05T01:00:00Z"
        );
        assert_eq!(max_ts("none", "none"), "none");
        assert_eq!(max_ts("", ""), "none");
        assert_eq!(
            max_ts("2026-06-05T01:00:00Z", "2026-06-05T02:00:00Z"),
            "2026-06-05T02:00:00Z"
        );
    }

    fn finding_comment(id: i64, body: &str, created_at: &str) -> Value {
        serde_json::json!({
            "id": id,
            "in_reply_to_id": null,
            "user": {"login": "chatgpt-codex-connector[bot]"},
            "body": body,
            "path": "src/x.rs",
            "line": 42,
            "created_at": created_at
        })
    }

    fn reply_comment(id: i64, parent: i64, login: &str, body: &str, created_at: &str) -> Value {
        serde_json::json!({
            "id": id,
            "in_reply_to_id": parent,
            "user": {"login": login},
            "body": body,
            "created_at": created_at
        })
    }

    const REQ: &[&str] = &["chatgpt-codex-connector"];

    fn req_vec() -> Vec<String> {
        REQ.iter().map(|s| s.to_string()).collect()
    }

    /// AC2-ERR core: a P1 with no reply is unaddressed.
    #[test]
    fn finding_no_reply_is_unaddressed() {
        let comments = vec![finding_comment(
            100,
            "![P1 Badge](https://img.shields.io/badge/P1-orange) bug",
            "2026-06-05T01:10:00Z",
        )];
        let (ts, unaddressed) = compute_unaddressed_findings(&comments, &[], &req_vec(), &[]);
        assert_eq!(ts, "2026-06-05T01:10:00Z");
        assert_eq!(unaddressed.len(), 1);
        assert_eq!(unaddressed[0].path, "src/x.rs");
        assert_eq!(unaddressed[0].line, 42);
        assert_eq!(unaddressed[0].severity, "P1");
    }

    /// AC2-HP commit arm: non-bot reply + commit after the finding -> addressed.
    #[test]
    fn finding_reply_plus_commit_after_is_addressed() {
        let comments = vec![
            finding_comment(
                100,
                "![P1 Badge](https://img.shields.io/badge/P1-orange) bug",
                "2026-06-05T01:10:00Z",
            ),
            reply_comment(
                101,
                100,
                "bllshttng",
                "fixed in abc123",
                "2026-06-05T01:20:00Z",
            ),
        ];
        let commits = vec!["2026-06-05T01:30:00Z".to_string()];
        let (_, unaddressed) = compute_unaddressed_findings(&comments, &commits, &req_vec(), &[]);
        assert!(unaddressed.is_empty(), "commit-after arm must address");
    }

    /// AC2-FR wontfix arm: non-bot reply carrying wontfix:, NO commit after.
    #[test]
    fn finding_wontfix_reply_is_addressed_without_commit() {
        let comments = vec![
            finding_comment(
                100,
                "![P1 Badge](https://img.shields.io/badge/P1-orange) bug",
                "2026-06-05T01:10:00Z",
            ),
            reply_comment(
                101,
                100,
                "bllshttng",
                "wontfix: intentional - documented tradeoff",
                "2026-06-05T01:20:00Z",
            ),
        ];
        // Only commit predates the finding -> commit arm unsatisfied.
        let commits = vec!["2026-06-05T01:00:00Z".to_string()];
        let (_, unaddressed) = compute_unaddressed_findings(&comments, &commits, &req_vec(), &[]);
        assert!(unaddressed.is_empty(), "wontfix arm must address alone");
    }

    /// Anti-gaming: a commit alone (no reply) does NOT address (locked
    /// decision 3 - any unrelated commit would silently clear a P1).
    #[test]
    fn finding_commit_without_reply_is_unaddressed() {
        let comments = vec![finding_comment(
            100,
            "![P1 Badge](https://img.shields.io/badge/P1-orange) bug",
            "2026-06-05T01:10:00Z",
        )];
        let commits = vec!["2026-06-05T01:30:00Z".to_string()];
        let (_, unaddressed) = compute_unaddressed_findings(&comments, &commits, &req_vec(), &[]);
        assert_eq!(unaddressed.len(), 1, "commit alone must not address");
    }

    /// A bot's own reply in the thread is not an ack.
    #[test]
    fn finding_bot_reply_only_is_unaddressed() {
        let comments = vec![
            finding_comment(
                100,
                "![P1 Badge](https://img.shields.io/badge/P1-orange) bug",
                "2026-06-05T01:10:00Z",
            ),
            reply_comment(
                101,
                100,
                "chatgpt-codex-connector[bot]",
                "elaborating on my finding",
                "2026-06-05T01:15:00Z",
            ),
        ];
        let commits = vec!["2026-06-05T01:30:00Z".to_string()];
        let (_, unaddressed) = compute_unaddressed_findings(&comments, &commits, &req_vec(), &[]);
        assert_eq!(unaddressed.len(), 1, "bot self-reply must not count as ack");
    }

    /// Reply present but neither commit-after nor wontfix -> still unaddressed.
    #[test]
    fn finding_reply_without_commit_or_wontfix_is_unaddressed() {
        let comments = vec![
            finding_comment(
                100,
                "![P1 Badge](https://img.shields.io/badge/P1-orange) bug",
                "2026-06-05T01:10:00Z",
            ),
            reply_comment(
                101,
                100,
                "bllshttng",
                "looking into it",
                "2026-06-05T01:20:00Z",
            ),
        ];
        let commits = vec!["2026-06-05T01:00:00Z".to_string()]; // predates finding
        let (_, unaddressed) = compute_unaddressed_findings(&comments, &commits, &req_vec(), &[]);
        assert_eq!(unaddressed.len(), 1);
    }

    /// A finding from a NON-required bot does not gate.
    #[test]
    fn finding_from_non_required_bot_ignored() {
        let comments = vec![serde_json::json!({
            "id": 200,
            "in_reply_to_id": null,
            "user": {"login": "gemini-code-assist[bot]"},
            "body": "![high](https://www.gstatic.com/codereviewagent/high-priority.svg) eh",
            "path": "src/y.rs",
            "line": 7,
            "created_at": "2026-06-05T01:10:00Z"
        })];
        // required = codex only; gemini finding is not gate-relevant
        let (ts, unaddressed) = compute_unaddressed_findings(&comments, &[], &req_vec(), &[]);
        assert!(unaddressed.is_empty());
        // ...but its timestamp still feeds the fingerprint.
        assert_eq!(ts, "2026-06-05T01:10:00Z");
    }

    /// Boundaries: empty comments array -> no findings, ts "none".
    #[test]
    fn empty_comments_no_findings() {
        let (ts, unaddressed) = compute_unaddressed_findings(&[], &[], &req_vec(), &[]);
        assert_eq!(ts, "none");
        assert!(unaddressed.is_empty());
    }

    /// sigma-review: a blocking finding row with a missing id is SKIPPED
    /// (under-block per locked decision 4), never pooled on a default id
    /// where one stray reply could clear multiple findings.
    #[test]
    fn finding_missing_id_skipped_not_pooled() {
        let no_id = serde_json::json!({
            "in_reply_to_id": null,
            "user": {"login": "chatgpt-codex-connector[bot]"},
            "body": "![P1 Badge](https://img.shields.io/badge/P1-orange) idless",
            "path": "src/z.rs", "line": 3,
            "created_at": "2026-06-05T01:05:00Z"
        });
        let real = finding_comment(
            100,
            "![P1 Badge](https://img.shields.io/badge/P1-orange) real",
            "2026-06-05T01:10:00Z",
        );
        // A stray reply keyed to id 0 must not ack anything.
        let stray = reply_comment(
            101,
            0,
            "bllshttng",
            "wontfix: stray",
            "2026-06-05T01:20:00Z",
        );
        let comments = vec![no_id, real, stray];
        let (_, unaddressed) = compute_unaddressed_findings(&comments, &[], &req_vec(), &[]);
        assert_eq!(unaddressed.len(), 1, "only the real finding remains");
        assert_eq!(unaddressed[0].id, 100);
    }

    /// sigma-review: commit-after comparison parses timestamps instead of
    /// string-comparing - an offset-suffixed commit date that lexicographically
    /// sorts above a Zulu finding date but is EARLIER in UTC must not clear
    /// the finding.
    #[test]
    fn ts_after_parses_offsets_correctly() {
        // 23:30+13:00 == 10:30Z, which is BEFORE 11:00Z - but the raw string
        // "2026-06-05T23:30:00+13:00" > "2026-06-05T11:00:00Z".
        assert!(!ts_after(
            "2026-06-05T23:30:00+13:00",
            "2026-06-05T11:00:00Z"
        ));
        // POSITIVE direction proves chrono's FromStr for DateTime<Utc>
        // parses offset-suffixed RFC3339 and converts to UTC (gemini's
        // #448 critical claimed it errors; empirically it returns
        // Ok(2026-06-05T13:30:00Z) here). Without this assertion the
        // offset case above could pass vacuously via the Err arm.
        assert!(ts_after(
            "2026-06-05T23:30:00+10:00", // == 13:30Z
            "2026-06-05T11:00:00Z"
        ));
        assert!(ts_after("2026-06-05T11:00:01Z", "2026-06-05T11:00:00Z"));
        assert!(!ts_after("2026-06-05T11:00:00Z", "2026-06-05T11:00:00Z"));
        // Unparseable on either side never clears a finding.
        assert!(!ts_after("garbage", "2026-06-05T11:00:00Z"));
        assert!(!ts_after("2026-06-05T11:00:00Z", "garbage"));
        assert!(!ts_after("2026-06-05T11:00:00Z", ""));
    }

    /// gemini high on #448: max_ts compares chronologically when both sides
    /// parse, returning the original string either way (byte-stable
    /// fingerprint).
    #[test]
    fn max_ts_chronological_with_offsets() {
        // +13:00 form is EARLIER in UTC despite sorting higher as a string.
        assert_eq!(
            max_ts("2026-06-05T23:30:00+13:00", "2026-06-05T11:00:00Z"),
            "2026-06-05T11:00:00Z"
        );
        // The winner is returned verbatim.
        assert_eq!(
            max_ts("2026-06-05T23:30:00+10:00", "2026-06-05T11:00:00Z"),
            "2026-06-05T23:30:00+10:00"
        );
    }

    /// Concurrency (Failure Modes): a reply arriving BEFORE its parent
    /// finding in the comments array (REST ordering is not guaranteed across
    /// pagination) still acks the finding - no order dependence.
    #[test]
    fn finding_reply_listed_before_finding_still_addressed() {
        let comments = vec![
            reply_comment(
                101,
                100,
                "bllshttng",
                "wontfix: ordering test",
                "2026-06-05T01:20:00Z",
            ),
            finding_comment(
                100,
                "![P1 Badge](https://img.shields.io/badge/P1-orange) bug",
                "2026-06-05T01:10:00Z",
            ),
        ];
        let (_, unaddressed) = compute_unaddressed_findings(&comments, &[], &req_vec(), &[]);
        assert!(
            unaddressed.is_empty(),
            "reply-before-finding ordering must still ack"
        );
    }

    // ── step 2: outage vs no-PR discrimination (US4) ─────────────────────────

    #[test]
    fn no_pr_stderr_detected() {
        assert!(is_no_pr_stderr(
            b"no pull requests found for branch \"feat\""
        ));
        assert!(is_no_pr_stderr(b"No pull requests found for branch \"x\""));
        // Outage shapes are NOT no-PR.
        assert!(!is_no_pr_stderr(b"connect: network is unreachable"));
        assert!(!is_no_pr_stderr(b"API rate limit exceeded"));
        assert!(!is_no_pr_stderr(b""));
    }
}
