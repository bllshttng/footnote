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
    /// Work complete (PR open, mergeable, reviewed, HEAD shipped) but `done()`
    /// fails SOLELY on CI-green because main itself is red on the same checks,
    /// and a bg agent cannot merge. Proven pre-existing main-red (strict
    /// check-name subset against current main HEAD) terminates the loop with a
    /// one-shot merge-recommendation notify instead of burning to NoProgress.
    /// Terminal, but NOT a ship reason (like DoneBatched): never merges, never
    /// marks the node done - a human merge then the out-of-band-merge reconcile
    /// path closes it, and DonePRGreen always wins when observable.
    DoneAwaitingMerge,
    /// A plan-only thread reached the plan boundary cleanly (manifest `planned`
    /// flag + a promise). It produced planning output, not a delivery, so it is
    /// terminal but deliberately NOT a ship reason (out of finalize.SHIP_REASONS
    /// -> no plan stamp/graduate) and NOT a postmortem reason (a plan is not
    /// stuck). Benign like NoWork; distinct from DoneAdvisory, which DOES
    /// graduate. The scoreboard's `planned` bucket is keyed on the phase set,
    /// never on this terminal.
    DonePlanned,
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
    /// plan-only thread: reaches the plan boundary and terminates DonePlanned
    /// (not DoneAdvisory, which would graduate the plan).
    planned: bool,
    legacy_status: Option<String>, // COMPLETE | BLOCKED | ABORTED
    /// None = absent (unlimited). Some(Ok(v)) = valid cap. Some(Err(s)) = malformed raw value.
    budget_wall_clock_cap_minutes: Option<Result<u64, String>>,
    /// None = absent (unlimited). Some(Ok(v)) = valid cap. Some(Err(s)) = malformed raw value.
    budget_cost_cap_usd: Option<Result<f64, String>>,
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
            planned: false,
            legacy_status: None,
            budget_wall_clock_cap_minutes: None, // None = absent = unlimited
            budget_cost_cap_usd: None,           // None = absent = unlimited
        }
    }
}

/// Read a single `^<field>: value` line from ANYWHERE in the manifest, not just
/// the frontmatter block. `fno target init` writes the immutable frontmatter
/// first, then APPENDS the node-claim fields (`target_claim_key/holder/ttl`)
/// after the closing `---`, so `parse_manifest` (frontmatter-bounded) never sees
/// them. Renewal reads them here instead (x-ba4b). Surrounding quotes stripped.
fn scan_manifest_field(content: &str, field: &str) -> Option<String> {
    let prefix = format!("{field}:");
    content.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix(&prefix)
            .map(|v| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
            .filter(|v| !v.is_empty())
    })
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
                // fno_id is canonical and wins; session_id is the one-release
                // legacy fallback (never overwrites a resolved fno_id).
                "fno_id" => m.session_id = Some(v.to_string()),
                "session_id" => {
                    if m.session_id.is_none() {
                        m.session_id = Some(v.to_string());
                    }
                }
                "created_at" => m.created_at = Some(v.to_string()),
                "attended" => m.attended = v == "true",
                "advisory" => m.advisory = v == "true",
                "no_ship" => m.no_ship = v == "true",
                "no_external" => m.no_external = v == "true",
                "batched" => m.batched = v == "true",
                "planned" => m.planned = v == "true",
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
    /// config.review.github_apps (x-4baa; the GitHub App bot logins gate).
    /// None = key absent -> code default (empty, no gate).
    /// Some([]) = explicitly `[]` -> declared no-review-gate path.
    /// Some(list) = every listed login must have a completed review pass.
    github_apps: Option<Vec<String>>,
    /// config.review.required_bots: legacy alias for `github_apps` (a straight
    /// rename). `github_apps` wins when both are set. Same fail-closed rules.
    required_bots: Option<Vec<String>>,
    /// config.review.peers (x-4baa): harness peers that post a real PR review
    /// under `peer_identity` (or their own map `identity`). loop-check only
    /// needs each peer's posting identity - the gate is login-based.
    peers: Vec<PeerEntry>,
    /// config.review.peer_identity: the shared login peers post under.
    peer_identity: Option<String>,
    /// config.review.optional_apps: reviewer logins honored-if-present but NOT
    /// required. The gate never WAITS for them (their absence never blocks -
    /// this kills the App-bot usage-limit wedge), but a blocking finding from
    /// one still holds the gate until addressed ("honor if present"). None =
    /// no optional reviewers.
    optional_apps: Option<Vec<String>>,
    /// config.review.reviewers (x-e703, Phase 2): local reviewer names (sigma |
    /// code-review | declare) satisfied by a head-pinned `review_attestation`
    /// event in events.jsonl, NOT a GitHub login. Empty = no reviewers gate
    /// (additive to the login gate; no "declared empty" distinction needed). A
    /// leading '/' is stripped on store so `/code-review` == `code-review`.
    /// Resolvability is validated Python-side; Rust fails closed by matching
    /// evidence, so an unresolvable name is simply never satisfied.
    reviewers: Vec<String>,
}

/// Normalize a config.review.reviewers entry / an event's reviewer name: strip a
/// leading '/' so `/code-review` and `code-review` name the same reviewer
/// (parity with the Python validator). Quote/comment stripping is the caller's.
fn normalize_reviewer(raw: &str) -> String {
    raw.trim().trim_start_matches('/').to_string()
}

/// Fail-closed sentinel for a structurally-malformed `reviewers:` value (e.g. a
/// `{...}` mapping). Python raises loudly on such a value; the Rust parser must
/// NOT silently drop it to an empty list (= no gate, fail OPEN). Instead it
/// stores this sentinel so the gate stays active but UNSATISFIABLE - the NUL
/// byte can never appear in an emitted `review_attestation.reviewer`, so no
/// evidence ever clears it (codex peer review P1; mirrors x-4baa's
/// UNRESOLVED_PEER_SENTINEL).
const MALFORMED_REVIEWERS_SENTINEL: &str = "\u{0}malformed-reviewers";

/// A `config.review.peers` entry. `provider` is kept for messaging and the
/// same-model guard; `model` carries an optional `"route_provider,route_model"`
/// route (the claude CLI as transport for a genuinely different model); the gate
/// itself only matches the resolved posting `identity` (own, or peer_identity).
#[derive(Debug, Default, Clone)]
struct PeerEntry {
    provider: String,
    model: Option<String>,
    identity: Option<String>,
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

/// Fail-closed sentinel for an unparseable config.toml (x-81d9 (c)). A
/// scanner error (e.g. tab-indentation, which YAML forbids) previously caused
/// the hand-parser to silently drop the whole config.review subtree, yielding
/// zero required_bots and shipping the PR unreviewed. Now such a file fails
/// CLOSED: this sentinel is placed in the login gate so it can never be
/// satisfied (no real bot login contains a NUL), the gate blocks visibly, and a
/// `loop_check_settings_unparseable` event records it. Distinct from
/// MALFORMED_REVIEWERS_SENTINEL so an audit sees which gate the config tripped.
const UNPARSEABLE_SETTINGS_SENTINEL: &str = "\u{0}unparseable-settings\u{0}";

/// A bare scalar RHS (`key: value`) as a single-item login list. Used when a
/// list key was written scalar-form: it must GATE on that one login, never
/// silently fail open to "no gate" (codex P1 on #205). A structurally-malformed
/// value (a `{...}` flow mapping) is NOT a login - degrade to None so both
/// parsers agree (Python's typed reader drops a mapping to None too; codex P1 on
/// the two-parser-agreement invariant). Empty -> None.
fn scalar_as_singleton(rest: &str) -> Option<Vec<String>> {
    let v = strip_inline_comment(rest.trim())
        .trim_matches(|c| c == '"' || c == '\'')
        .to_string();
    if v.is_empty() || v.contains('{') || v.contains('}') {
        None
    } else {
        Some(vec![v])
    }
}

/// A TOML scalar (string / integer / float / bool) as a String; None for
/// structured values (array / table). Numbers and bools stringify so a
/// `required_bots = 123` or a stray bool still coerces to a login string,
/// matching the old scalar-tolerant behavior.
fn scalar_string(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Boolean(b) => Some(b.to_string()),
        toml::Value::Integer(n) => Some(n.to_string()),
        toml::Value::Float(f) => Some(f.to_string()),
        _ => None,
    }
}

/// Classify a config.review LOGIN list value (`required_bots` / `github_apps` /
/// `optional_apps`) off a typed TOML Value, matching the Python loader:
///   absent        -> None            (key absent; code default = no gate)
///   array         -> Some(items)     (empty stays Some(empty) = declared no-gate)
///   scalar        -> singleton gate  (a bare `key = "codex"` still GATES on codex)
///   table/other   -> None            (an inline table is not a login; Python drops it)
fn value_as_login_list(v: &toml::Value) -> Option<Vec<String>> {
    match v {
        toml::Value::Array(items) => Some(items.iter().filter_map(scalar_string).collect()),
        // A bare scalar routes through scalar_as_singleton so its brace/empty
        // semantics (and the direct unit test) stay live and Python-aligned.
        toml::Value::String(_)
        | toml::Value::Boolean(_)
        | toml::Value::Integer(_)
        | toml::Value::Float(_) => scalar_string(v).and_then(|s| scalar_as_singleton(&s)),
        // Table / other: not a login gate -> None (Python parity).
        _ => None,
    }
}

/// Classify a config.review.reviewers value (x-e703 local-attestation gate).
/// Unlike the login lists, a structurally-wrong mapping fails CLOSED (Python
/// raises) via the unsatisfiable sentinel, never a silent empty gate. A leading
/// '/' is normalized off each entry.
fn value_as_reviewers(v: &toml::Value) -> Vec<String> {
    match v {
        toml::Value::Array(items) => {
            let mut out = Vec::new();
            for it in items {
                match scalar_string(it) {
                    Some(s) => {
                        let n = normalize_reviewer(&s);
                        if !n.is_empty() {
                            out.push(n);
                        }
                    }
                    // A non-scalar item (nested table/array) is structurally
                    // wrong; Python raises on it, so fail CLOSED with the
                    // sentinel rather than silently dropping it (gemini medium) -
                    // matches the top-level-table arm below.
                    None => return vec![MALFORMED_REVIEWERS_SENTINEL.to_string()],
                }
            }
            out
        }
        toml::Value::String(s) => {
            let n = normalize_reviewer(s);
            if n.is_empty() {
                Vec::new()
            } else {
                vec![n]
            }
        }
        // A table (or other structural shape) fails closed, not empty.
        _ => vec![MALFORMED_REVIEWERS_SENTINEL.to_string()],
    }
}

/// Classify a config.review.peers value into PeerEntry list. A sequence item is
/// either a scalar (provider only) or a mapping whose `provider`/`identity` keys
/// are read order-independently (a real map, so no hand key-order handling). A
/// bare scalar `peers: codex` is one provider (Python's coerce_peers).
fn value_as_peers(v: &toml::Value) -> Vec<PeerEntry> {
    let scalar_entry = |s: String| PeerEntry {
        provider: s,
        model: None,
        identity: None,
    };
    // One table entry -> a PeerEntry (provider/model/identity read order-independently).
    let map_entry = |it: &toml::Value| -> Option<PeerEntry> {
        let provider = it
            .get("provider")
            .and_then(scalar_string)
            .unwrap_or_default();
        let model = it
            .get("model")
            .and_then(scalar_string)
            .filter(|s| !s.is_empty());
        let identity = it
            .get("identity")
            .and_then(scalar_string)
            .filter(|s| !s.is_empty());
        if provider.is_empty() && identity.is_none() {
            None
        } else {
            Some(PeerEntry {
                provider,
                model,
                identity,
            })
        }
    };
    match v {
        toml::Value::Array(items) => items
            .iter()
            .filter_map(|it| match it {
                toml::Value::Table(_) => map_entry(it),
                _ => scalar_string(it)
                    .filter(|s| !s.is_empty())
                    .map(scalar_entry),
            })
            .collect(),
        toml::Value::String(s) if !s.is_empty() => vec![scalar_entry(s.clone())],
        // A single top-level table is ONE peer - parity with Python's
        // coerce_peers, which wraps a dict as [dict]. Dropping it to empty (as
        // this arm did before the codex peer review) silently discards a
        // configured peer gate -> fail-open, the class this PR removes.
        toml::Value::Table(_) => map_entry(v).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Read an f64 budget cap off a typed Value: a number is Ok, a non-numeric
/// scalar fails CLOSED as Some(Err(raw)) (so check_budget trips), an
/// absent/null key is None (unlimited). Mirrors the manifest cap semantics.
fn read_f64_cap(v: &toml::Value, ctx: &str) -> Option<Result<f64, String>> {
    match v {
        toml::Value::Integer(n) => Some(Ok(*n as f64)),
        toml::Value::Float(f) => Some(Ok(*f)),
        other => {
            let raw = scalar_string(other).unwrap_or_default();
            Some(raw.parse::<f64>().map_err(|_| {
                eprintln!(
                    "loop-check: malformed budget cap '{ctx}: {raw}' - failing closed; fix the config"
                );
                raw
            }))
        }
    }
}

/// Read a u64 budget cap off a typed Value (same fail-closed rule as f64).
fn read_u64_cap(v: &toml::Value, ctx: &str) -> Option<Result<u64, String>> {
    match v {
        toml::Value::Integer(n) => Some(u64::try_from(*n).map_err(|_| {
            eprintln!(
                "loop-check: malformed budget cap '{ctx}: {n}' - failing closed; fix the config"
            );
            n.to_string()
        })),
        other => {
            let raw = scalar_string(other).unwrap_or_default();
            Some(raw.parse::<u64>().map_err(|_| {
                eprintln!(
                    "loop-check: malformed budget cap '{ctx}: {raw}' - failing closed; fix the config"
                );
                raw
            }))
        }
    }
}

/// Settings with the login gate pinned unsatisfiable - the fail-closed result
/// when config.toml cannot be parsed as TOML at all (x-81d9 (c)). The
/// sentinel goes into BOTH github_apps and required_bots: resolved_required_bots
/// prefers github_apps.or(required_bots), so pinning required_bots alone would
/// be silently outranked by a parseable global file's github_apps during the
/// global+local merge (an unparseable LOCAL file would then resolve to the
/// global gate, re-opening the fail-open this fix removes).
fn fail_closed_settings() -> Settings {
    let sentinel = Some(vec![UNPARSEABLE_SETTINGS_SENTINEL.to_string()]);
    Settings {
        github_apps: sentinel.clone(),
        required_bots: sentinel,
        ..Default::default()
    }
}

/// Parse config.toml with the `toml` crate (stage 3), replacing the
/// former hand-rolled indent state machine that derived one global indent unit
/// and silently dropped the config.review subtree on tabs or mixed widths
/// (x-81d9 (c)). A genuine YAML scanner error (e.g. tab indentation) returns
/// Err so the caller can fail closed + emit an event, rather than silently
/// zeroing the gate. The typed-Value classification preserves every semantic
/// the old ListForm branches encoded (see the value_as_* helpers).
fn parse_settings_result(content: &str) -> Result<Settings, String> {
    let root: toml::Value = content.parse::<toml::Value>().map_err(|e| e.to_string())?;
    let mut s = Settings::default();

    // Top-level flat budget cap.
    if let Some(v) = root.get("budget_cap") {
        s.flat_budget_cap = read_f64_cap(v, "budget_cap");
    }

    // Flat config.toml: budget / ci / external_reviewers / review are top-level
    // blocks (no `config:` wrapper). Read them straight off root.
    if let Some(budget) = root.get("budget") {
        if let Some(att) = budget.get("attended") {
            if let Some(v) = att.get("wall_clock_cap_minutes") {
                s.attended_wall_cap_minutes = read_u64_cap(v, "attended.wall_clock_cap_minutes");
            }
            if let Some(v) = att.get("cost_cap_usd") {
                s.attended_cost_cap_usd = read_f64_cap(v, "attended.cost_cap_usd");
            }
        }
        if let Some(un) = budget.get("unattended") {
            if let Some(v) = un.get("wall_clock_cap_minutes") {
                s.unattended_wall_cap_minutes =
                    read_u64_cap(v, "unattended.wall_clock_cap_minutes");
            }
            if let Some(v) = un.get("cost_cap_usd") {
                s.unattended_cost_cap_usd = read_f64_cap(v, "unattended.cost_cap_usd");
            }
        }
    }

    if let Some(ci) = root.get("ci") {
        s.ci_declared_none = ci
            .get("declared_none")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    }

    if let Some(er) = root.get("external_reviewers") {
        if let Some(items) = er.as_array() {
            s.external_reviewers = items.iter().filter_map(scalar_string).collect();
        }
    }

    if let Some(review) = root.get("review") {
        if let Some(v) = review.get("required_bots") {
            s.required_bots = value_as_login_list(v);
        }
        if let Some(v) = review.get("github_apps") {
            s.github_apps = value_as_login_list(v);
        }
        if let Some(v) = review.get("optional_apps") {
            s.optional_apps = value_as_login_list(v);
        }
        if let Some(v) = review.get("reviewers") {
            s.reviewers = value_as_reviewers(v);
        }
        if let Some(v) = review.get("peers") {
            s.peers = value_as_peers(v);
        }
        if let Some(v) = review.get("peer_identity") {
            s.peer_identity = scalar_string(v).filter(|s| !s.is_empty());
        }
    }

    Ok(s)
}

/// Infallible wrapper: an unparseable file fails CLOSED (unsatisfiable login
/// gate) rather than silently defaulting to no gate. Test-only - production
/// calls parse_settings_result directly so it can also emit the
/// `loop_check_settings_unparseable` event on the Err path.
#[cfg(test)]
fn parse_settings(content: &str) -> Settings {
    parse_settings_result(content).unwrap_or_else(|_| fail_closed_settings())
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
        // Either key: new rows carry fno_id, pre-rename rows only session_id.
        let matches = entry.get("fno_id").and_then(|v| v.as_str()) == Some(session_id)
            || entry.get("session_id").and_then(|v| v.as_str()) == Some(session_id);
        if matches {
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
    /// Every failing check/job name on the PR head (bucket fail|cancel), at the
    /// same granularity as `gh pr checks .name`. Feeds the DoneAwaitingMerge
    /// subset rule against main's failing set. Empty when CI is green/pending.
    failing_checks: Vec<String>,
    /// True iff any check on the PR head is still pending (a non-terminal
    /// bucket). `ci_conclusion` reports `Failure` as soon as ONE check fails even
    /// while others run, so the DoneAwaitingMerge terminal must consult this to
    /// avoid firing while the session's own in-flight job could still turn red.
    ci_has_pending: bool,
    /// GitHub mergeable state ("MERGEABLE" | "CONFLICTING" | "UNKNOWN"). The
    /// DoneAwaitingMerge terminal must not fire on a "CONFLICTING" PR: the human
    /// cannot merge past main-red until the branch is rebased, and the terminal
    /// would drop the node from retry circulation while it is un-mergeable.
    mergeable: String,
    /// Newest review/comment/inline-comment activity (ISO8601 or "none");
    /// folded into the fingerprint's 4th component on done() fires.
    latest_review_ts: String,
    reviewed: bool, // every required bot passed AND no unaddressed blocking finding
    /// Required bots with no completed review pass (names the gap in the
    /// block message, AC1-UI).
    missing_bots: Vec<String>,
    /// Required bots dropped from the gate because they are rate-limited (a
    /// usage-limit comment, no review). Named in the terminal-allow message so
    /// an operator sees why the gate proceeded without them (AC1-UI).
    usage_limited: Vec<String>,
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

/// True iff EVERY reviewer is satisfied by a head-pinned `review_attestation`
/// event (x-e703, Phase 2). A reviewer is satisfied when events.jsonl carries a
/// line with `type == "review_attestation"`, `data.reviewer` matching (leading
/// '/' stripped on both sides), `data.head_sha == head_sha`, and
/// `data.verdict == "pass"`. This is the trust-core seam: today loop-check reads
/// events.jsonl only for prior `loop_check` fires; here it reads a local
/// attestation into the `reviewed` decision.
///
/// Fail closed everywhere: an empty/unreadable events file, a stale head_sha
/// (attestation for a prior commit), or a `fail` verdict leaves the reviewer
/// UNSATISFIED, mirroring how a missing bot review holds the login gate. An
/// empty reviewer list is vacuously satisfied (no reviewers gate).
fn reviewers_all_attested(events_path: &Path, reviewers: &[String], head_sha: &str) -> bool {
    if reviewers.is_empty() {
        return true;
    }
    let Ok(content) = std::fs::read_to_string(events_path) else {
        return false; // no evidence file -> gate unmet (fail closed)
    };
    // Single pass (gemini review): record the LATEST verdict per reviewer at the
    // current head. events.jsonl is append-ordered, so a later attestation
    // supersedes an earlier one for the same reviewer - a `fail` posted after a
    // `pass` must revoke it, and a re-run `pass` after a `fail` must restore it
    // (codex peer review P1: a later fail was previously ignored). A reviewer is
    // satisfied iff its latest head-pinned verdict is exactly `pass`. O(lines).
    let mut latest_pass: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("review_attestation") {
            continue;
        }
        if val.pointer("/data/head_sha").and_then(|v| v.as_str()) != Some(head_sha) {
            continue;
        }
        if let Some(r) = val.pointer("/data/reviewer").and_then(|v| v.as_str()) {
            let is_pass = val.pointer("/data/verdict").and_then(|v| v.as_str()) == Some("pass");
            latest_pass.insert(r.trim_start_matches('/').to_string(), is_pass);
        }
    }
    reviewers
        .iter()
        .all(|entry| latest_pass.get(entry.trim_start_matches('/')) == Some(&true))
}

/// An operator review finding (x-f8d4) still open: a `review_finding` event for
/// the node with no later `review_finding_resolved` for the same id.
#[derive(Debug, Clone)]
struct OpenFinding {
    id: String,
    first_line: String,
}

/// Scan events.jsonl for OPEN operator review findings scoped to `node`.
///
/// Returns `(open findings sorted by id, malformed-line count)`. A finding is
/// open until an explicit `review_finding_resolved` clears it - node-scoped and
/// NOT head-pinned, so a new commit never auto-clears an operator's comment
/// (Locked Decision 2). Malformed finding lines notice-not-block (AC3-FR): a
/// line that is unparseable JSON but carries the literal `review_finding`, or a
/// parsed `review_finding` missing its id, is our own writer's corrupted output;
/// it is counted for the deny/audit notice but NEVER holds the gate. Any read
/// failure yields no findings (the gate is only ADDED by evidence, never
/// invented from an unreadable file).
fn open_review_findings(events_path: &Path, node: &str) -> (Vec<OpenFinding>, usize) {
    let Ok(content) = std::fs::read_to_string(events_path) else {
        return (Vec::new(), 0);
    };
    // Preserve first-seen order via a Vec of (id, first_line); a later duplicate
    // id (shouldn't happen - ids are minted) just refreshes the first_line.
    let mut findings: Vec<(String, String)> = Vec::new();
    let mut resolved: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut malformed = 0usize;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            // Only OUR corrupted output counts toward the notice; unrelated
            // corruption from another writer is not a finding concern.
            if line.contains("review_finding") {
                malformed += 1;
            }
            continue;
        };
        match val.get("type").and_then(|v| v.as_str()) {
            Some("review_finding") => {
                if val.pointer("/data/node").and_then(|v| v.as_str()) != Some(node) {
                    continue;
                }
                match val.pointer("/data/finding_id").and_then(|v| v.as_str()) {
                    Some(id) => {
                        let first = val
                            .pointer("/data/text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .lines()
                            .next()
                            .unwrap_or("")
                            .to_string();
                        if let Some(slot) = findings.iter_mut().find(|(fid, _)| fid == id) {
                            slot.1 = first;
                        } else {
                            findings.push((id.to_string(), first));
                        }
                    }
                    None => malformed += 1, // review_finding without an id
                }
            }
            Some("review_finding_resolved") => {
                if let Some(id) = val.pointer("/data/finding_id").and_then(|v| v.as_str()) {
                    resolved.insert(id.to_string());
                }
            }
            _ => {}
        }
    }
    let mut open: Vec<OpenFinding> = findings
        .into_iter()
        .filter(|(id, _)| !resolved.contains(id))
        .map(|(id, first_line)| OpenFinding { id, first_line })
        .collect();
    open.sort_by(|a, b| a.id.cmp(&b.id)); // deterministic deny reason
    (open, malformed)
}

/// Deny reason for an open-finding gate: quote the first finding (id + first
/// line) + the resolve remedy, plus a `[+N more]` count and any malformed-line
/// notice so nothing vanishes silently.
fn build_findings_block_reason(open: &[OpenFinding], malformed: usize) -> String {
    let f = &open[0];
    let more = if open.len() > 1 {
        format!(" [+{} more]", open.len() - 1)
    } else {
        String::new()
    };
    let notice = if malformed > 0 {
        format!(" ({malformed} malformed finding line(s) ignored)")
    } else {
        String::new()
    };
    format!(
        "open review finding {}: {} - address it, then `fno annotate resolve {}`{}{}",
        f.id, f.first_line, f.id, more, notice
    )
}

/// Run done() reads. Returns Ok(PrInfo) or Err((read_name, stderr_tail)) on gh failure.
#[allow(clippy::too_many_arguments)]
fn read_pr_info(
    gh_bin: &str,
    cwd: &Path,
    ci_declared_none: bool,
    no_external: bool,
    required_bots: &[String],
    optional_bots: &[String],
    external_reviewers: &[String],
    reviewers: &[String],
    head_sha: &str,
    events_path: &Path,
) -> Result<PrInfo, (String, String)> {
    // Read 1: PR state + number + head OID + mergeability
    let pr_view_out = Command::new(gh_bin)
        .args([
            "pr",
            "view",
            "--json",
            "state,number,headRefName,headRefOid,mergeable",
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
                failing_checks: Vec::new(),
                ci_has_pending: false,
                mergeable: "UNKNOWN".to_string(),
                latest_review_ts: "none".to_string(),
                reviewed: false,
                missing_bots: Vec::new(),
                usage_limited: Vec::new(),
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
    // GitHub's mergeable state: "MERGEABLE" | "CONFLICTING" | "UNKNOWN" (still
    // computing). Only "CONFLICTING" is a definitive no; UNKNOWN must not hold
    // the terminal (it clears on its own). Missing field -> "UNKNOWN".
    let mergeable = pr_json
        .get("mergeable")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN")
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
            failing_checks: Vec::new(),
            ci_has_pending: false,
            mergeable,
            latest_review_ts: "none".to_string(),
            reviewed: true,
            missing_bots: Vec::new(),
            usage_limited: Vec::new(),
            unaddressed_findings: Vec::new(),
            review_skipped: true,
        });
    }

    // Read 2: CI checks. Compute the conclusion, the full failing-check-name set,
    // AND whether any check is still pending from the same payload (the set feeds
    // the DoneAwaitingMerge subset rule; the pending flag gates that terminal so
    // it never fires on partial CI).
    let (ci_conclusion, failing_checks, ci_has_pending) = if ci_declared_none {
        (CiConclusion::Skipped, Vec::new(), false)
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

        let failing = failing_check_names(&checks);
        let has_pending = ci_has_pending_checks(&checks);
        (
            compute_ci_conclusion(&checks).map_err(|e| (e, String::new()))?,
            failing,
            has_pending,
        )
    };

    // Reads 3+4: reviews + inline findings. Skipped when the session declares
    // no_external OR the repo declares `required_bots: []` (the no-review-gate
    // path, US3 - mirrors ci.declared_none; PR + CI carry the gate). The two
    // skips are orthogonal: one is per-session, the other repo config.
    // Skip the review reads only when there is NOTHING to honor: no required
    // login AND no optional login. An optional-only gate still reads (to catch
    // an optional blocking finding), but its presence is never required.
    // x-e703: the gate is a strict conjunction over the union of GitHub-login
    // evidence (github_apps/peers via optional_bots+required_bots) AND the
    // local-attestation `reviewers`. Each satisfied by its own evidence source,
    // so the two skips are INDEPENDENT: `no_external` (and an empty login set)
    // skips only the EXTERNAL GitHub-login reads - it is scoped to external
    // review (control-plane-loop.md step 2), NOT the local attestation gate. A
    // repo that pins `reviewers: [sigma]` still requires that local pass even
    // when a session runs `--no-external` to skip usage-wedged App bots
    // (fixes a fail-open the sigma review caught). `reviewers` is empty for
    // every pre-x-e703 config, so `reviewers_all_attested` is vacuously true
    // there and this changes nothing for them.
    let login_gate_active = !required_bots.is_empty() || !optional_bots.is_empty();
    let login_skipped = no_external || !login_gate_active;
    let reviewers_ok = reviewers_all_attested(events_path, reviewers, head_sha);
    let (latest_review_ts, reviewed, missing_bots, usage_limited, unaddressed_findings) =
        if login_skipped {
            // No GitHub logins to poll (nothing configured, or no_external): skip
            // the gh review reads entirely (fewer calls + no spurious gh-error
            // block). The local attestation gate still applies - reviewers_ok is
            // true when unconfigured, so a login-only or no-gate config is
            // unaffected.
            (
                "none".to_string(),
                reviewers_ok,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
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

            // PRESENCE is required-only: an optional login's absence must never
            // create a missing_bot (never wait for it). FINDINGS honor the union:
            // an optional login's blocking P1 still holds the gate ("honor if
            // present"). A dedup keeps a login that is in both lists counted once.
            let info = compute_review_info(&reviews_json, required_bots);
            let mut findings_bots: Vec<String> = required_bots.to_vec();
            for b in optional_bots {
                if !findings_bots.iter().any(|x| x == b) {
                    findings_bots.push(b.clone());
                }
            }

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
            for page in
                serde_json::Deserializer::from_slice(&comments_out.stdout).into_iter::<Value>()
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
                    && blocking_severity(c.get("body").and_then(|v| v.as_str()).unwrap_or(""))
                        .is_some()
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
                &findings_bots,
                external_reviewers,
            );

            // Read 4's newest comment timestamp joins the activity timestamp so
            // inline-only review traffic advances the fingerprint (closes the
            // false-NoProgress hole).
            let activity_ts = max_ts(&info.latest_ts, &inline_ts);
            // x-e703: the login gate AND the local-attestation reviewers gate must
            // both clear. reviewers is usually empty (vacuously true) so this is a
            // no-op for login-only configs.
            let reviewed = info.all_required_passed() && unaddressed.is_empty() && reviewers_ok;
            // (a) Record the rate-limit drop so a post-hoc audit sees why the gate
            // proceeded without a required bot (AC1-UI). append_loop_event, not
            // Branch-B emit: these are target-stream events (see the doc comment on
            // append_loop_event), deliberately unregistered in KNOWN_EVENT_KINDS.
            if !info.usage_limited.is_empty() {
                append_loop_event(
                    events_path,
                    "review_gate_bot_usage_limited",
                    serde_json::json!({"pr": number, "bots": info.usage_limited.clone()}),
                );
            }
            (
                activity_ts,
                reviewed,
                info.missing_bots,
                info.usage_limited,
                unaddressed,
            )
        };

    Ok(PrInfo {
        state,
        number,
        head_oid,
        ci_conclusion,
        failing_checks,
        ci_has_pending,
        mergeable,
        latest_review_ts,
        reviewed,
        missing_bots,
        usage_limited,
        unaddressed_findings,
        // Telemetry only (no decision reads this): "no review gate of any kind
        // applied" = the login reads were skipped AND no local reviewers gate.
        // A reviewers-only config did gate, so it is NOT review_skipped.
        review_skipped: login_skipped && reviewers.is_empty(),
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

// ── DoneAwaitingMerge classifier ───────────────────────────────────────────────
//
// When done() fails SOLELY on CI-green (PR open+mergeable, reviewed, HEAD
// shipped) the loop would burn to NoProgress while a bg agent waits on a merge
// it cannot perform - but only pathologically so when main ITSELF is red on the
// same checks. `pre_existing_main_red` proves that condition mechanically:
// every failing PR check name must also be failing on current main HEAD (strict
// subset, check-name granularity so the mux flakes rotating test names between
// runs stay matched). Any PR-unique red, or any gh uncertainty, holds as today.

/// How many latest completed main runs to scan. `main_head_failing_checks` keeps
/// only the runs whose headSha equals the newest run's (the current main HEAD),
/// so this bound just needs to comfortably cover ONE commit's workflow fan-out
/// (this repo fires ~4-5 workflow runs per push); a value above that is harmless
/// because the headSha scope discards any older commit's runs. Bounded so the
/// per-fire gh cost stays constant.
const MAIN_RUN_LOOKBACK: usize = 10;

/// Failing check/job names on a `gh pr checks --json name,bucket` payload
/// (bucket fail|cancel), the same granularity a main-HEAD job carries. Non-fail
/// buckets (pass|pending|skipping) are ignored. Malformed entries are skipped.
fn failing_check_names(checks: &Value) -> Vec<String> {
    let Some(arr) = checks.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter(|c| {
            let bucket = c
                .get("bucket")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            matches!(bucket.as_str(), "fail" | "cancel")
        })
        .filter_map(|c| c.get("name").and_then(|v| v.as_str()).map(str::to_string))
        .collect()
}

/// True iff any check is still in a non-terminal bucket (`pending`, or an
/// unrecognized bucket that is not one of pass|fail|cancel|skipping). The
/// DoneAwaitingMerge terminal must not fire while any check is unresolved: a
/// still-running check (e.g. the session's own new job) could turn red, so a
/// partial `Failure` is not yet proof that the ONLY problem is pre-existing
/// main-red.
fn ci_has_pending_checks(checks: &Value) -> bool {
    let Some(arr) = checks.as_array() else {
        return false;
    };
    arr.iter().any(|c| {
        let bucket = c
            .get("bucket")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        !matches!(bucket.as_str(), "pass" | "fail" | "cancel" | "skipping")
    })
}

/// databaseIds of failed workflow runs from a `gh run list --json
/// databaseId,conclusion,headSha` payload, scoped to a single `head_sha`. Only
/// conclusion=="failure" runs whose headSha equals the current main HEAD count
/// (a cancelled or in-progress run is not proof; a run from an OLDER main commit
/// that has since been fixed is not proof of CURRENT main-red).
fn parse_failing_run_ids(run_list: &Value, head_sha: &str) -> Vec<i64> {
    let Some(arr) = run_list.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter(|r| r.get("conclusion").and_then(|v| v.as_str()) == Some("failure"))
        .filter(|r| r.get("headSha").and_then(|v| v.as_str()) == Some(head_sha))
        .filter_map(|r| r.get("databaseId").and_then(|v| v.as_i64()))
        .collect()
}

/// Failing job names from a `gh run view <id> --json jobs` payload. The `jobs`
/// `.name` field is the same namespace as `gh pr checks .name` (both are the
/// check-run/job name), so a name from here matches a PR failing-check name.
fn parse_failing_job_names(jobs_json: &Value) -> Vec<String> {
    let Some(jobs) = jobs_json.get("jobs").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    jobs.iter()
        .filter(|j| j.get("conclusion").and_then(|v| v.as_str()) == Some("failure"))
        .filter_map(|j| j.get("name").and_then(|v| v.as_str()).map(str::to_string))
        .collect()
}

/// The strict subset rule: main's failing set must COVER every failing PR check.
/// Empty PR-failing is never eligible (that is the DonePRGreen path, not here);
/// any PR-unique failing check blocks the terminal (the session's own breakage).
fn is_pre_existing_main_red(pr_failing: &[String], main_failing: &[String]) -> bool {
    if pr_failing.is_empty() {
        return false;
    }
    pr_failing.iter().all(|c| main_failing.contains(c))
}

/// Union of failing job names on the CURRENT main HEAD commit, scanning the
/// latest N completed runs on `--branch main` and keeping only those whose
/// headSha matches the newest run's (i.e. the current main HEAD). N is sized to
/// cover one commit's workflow fan-out with margin; scoping by headSha means a
/// larger N never pulls in a stale older commit's failures. Fail-CLOSED: any gh
/// error, non-zero exit, malformed JSON, ZERO completed runs, or a missing
/// headSha returns `None` (unknown -> the caller holds as today). A clean read
/// with no failures on HEAD returns `Some(empty)` -> the subset rule then fails
/// and the caller holds; only positive proof fires the terminal.
fn main_head_failing_checks(gh_bin: &str, cwd: &Path, n: usize) -> Option<Vec<String>> {
    let list_out = Command::new(gh_bin)
        .args([
            "run",
            "list",
            "--branch",
            "main",
            "--status",
            "completed",
            "--limit",
            &n.to_string(),
            "--json",
            "databaseId,conclusion,headSha",
        ])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !list_out.status.success() {
        return None; // gh error -> unknown -> hold
    }
    let list: Value = serde_json::from_slice(&list_out.stdout).ok()?;
    let arr = list.as_array()?;
    // Zero completed runs (new/quiet repo) is not proof -> unknown.
    // The newest run's headSha IS the current main HEAD; classify against only
    // that commit's runs so a failure fixed on a later commit never counts.
    let head_sha = arr
        .first()
        .and_then(|r| r.get("headSha"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let failing_run_ids = parse_failing_run_ids(&list, head_sha);

    let mut names: Vec<String> = Vec::new();
    for id in failing_run_ids {
        let view_out = Command::new(gh_bin)
            .args(["run", "view", &id.to_string(), "--json", "jobs"])
            .current_dir(cwd)
            .output()
            .ok()?;
        if !view_out.status.success() {
            return None; // any per-run gh error -> unknown -> hold (fail closed)
        }
        let view: Value = serde_json::from_slice(&view_out.stdout).ok()?;
        for name in parse_failing_job_names(&view) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    Some(names)
}

/// Idempotency guard (Concurrency AC): true iff a prior `termination` event with
/// reason `DoneAwaitingMerge` for this session already exists, so a re-evaluation
/// (crash restart, or the two consumers racing) does not double-emit or
/// double-notify. Fail-open (false) on an unreadable events file: at worst one
/// extra notify, never a silent skip of the terminal.
fn already_emitted_awaiting_merge(events_path: &Path, session_id: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(events_path) else {
        return false;
    };
    content.lines().any(|line| {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        val.get("type").and_then(|v| v.as_str()) == Some("termination")
            && val.pointer("/data/session_id").and_then(|v| v.as_str()) == Some(session_id)
            && val.pointer("/data/reason").and_then(|v| v.as_str()) == Some("DoneAwaitingMerge")
    })
}

/// Best-effort `fno notify TITLE BODY`. Spawned detached and never waited on;
/// any failure (missing binary, non-zero exit) is non-fatal - the terminal
/// completes on the durable event row alone (AC2-FR). Suppressed under
/// `FNO_LOOPCHECK_NO_NOTIFY=1` so unit tests never spawn a real notifier.
fn best_effort_notify(title: &str, body: &str) {
    if std::env::var("FNO_LOOPCHECK_NO_NOTIFY").as_deref() == Ok("1") {
        return;
    }
    // var_os avoids a lossy UTF-8 conversion on a path/binary env value and
    // hands the raw OsString straight to Command (gemini review).
    let fno_bin = std::env::var_os("FNO_LOOPCHECK_FNO_BIN").unwrap_or_else(|| "fno".into());
    let _ = Command::new(fno_bin).args(["notify", title, body]).spawn();
}

/// Known bot logins that count as reviewers when external_reviewers is not configured.
const KNOWN_BOTS: &[&str] = &["chatgpt-codex-connector", "gemini-code-assist"];

/// Default must-have-reviewed list when config.review.github_apps is absent.
/// EMPTY for fresh installs: a clone with no review configuration completes on
/// PR + CI green without hanging on a review bot it has never set up (a fresh
/// `/target` otherwise runs to the budget cap waiting for a codex review that
/// never arrives). Maintainers who want an external-review gate pin it
/// explicitly via config.review.github_apps (e.g. ["chatgpt-codex-connector"]).
const DEFAULT_REQUIRED_BOTS: &[&str] = &[];

/// A login that no real GitHub account can equal, pushed when a `peers` entry
/// has no resolvable posting identity so the gate stays UNMET (fail closed)
/// rather than silently going green without that reviewer (x-4baa).
const UNRESOLVED_PEER_SENTINEL: &str = "\u{0}fno-peer-without-identity\u{0}";

/// A login no real GitHub account can equal, pushed when a required peer login is
/// backed ONLY by peers whose model is the author's own (same-model guard). It
/// REPLACES the clearable login so a same-model review can never satisfy the
/// cross-model gate. Distinct from UNRESOLVED_PEER_SENTINEL for greppability.
const SAME_MODEL_PEER_SENTINEL: &str = "\u{0}fno-peer-same-model\u{0}";

/// Model family of a harness or provider name - the same-model guard's proxy for
/// "which model". The author's family is its invoking harness's family
/// (claude->anthropic, codex->openai, gemini->google); a peer's family is its
/// route provider (else its bare provider). An unknown name is None and so never
/// equals any author family (fail open per-peer). A routed-transport author
/// (claude CLI over GLM) still reads as anthropic here - a known limitation that
/// errs toward HOLDING the gate, never wrongly clearing it.
fn harness_family(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "claude" | "anthropic" => Some("anthropic"),
        "codex" | "openai" => Some("openai"),
        "gemini" | "google" => Some("google"),
        _ => None,
    }
}

/// The route provider of a peers `model` route: `"route_provider,route_model"`
/// -> `route_provider`. None unless there are exactly two non-empty comma parts,
/// matching the loader's parse rule (config/__init__.py coerce_peers), so a
/// malformed route falls back to the bare provider.
fn route_provider(model: &str) -> Option<&str> {
    let mut parts = model.split(',').map(str::trim);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(prov), Some(rest), None) if !prov.is_empty() && !rest.is_empty() => Some(prov),
        _ => None,
    }
}

/// A peer's effective model family: its route provider's family when it names a
/// valid route, else its bare provider's family. A `model` route is only honored
/// for a **claude** peer, because only the claude transport actually executes a
/// route (`claude -p` over the routed model); codex/gemini dispatch ignores the
/// route and runs the bare provider, so trusting a codex/gemini route would
/// classify a same-model review as cross-model and re-open the bypass this guard
/// exists to close. Matches the loader, which validates routes for claude only.
fn peer_family(peer: &PeerEntry) -> Option<&'static str> {
    let effective = peer
        .model
        .as_deref()
        .filter(|_| peer.provider.trim().eq_ignore_ascii_case("claude"))
        .and_then(route_provider)
        .unwrap_or(peer.provider.as_str());
    harness_family(effective)
}

/// Thin wrapper: resolve the must-have-reviewed login set with NO author-harness
/// awareness (the same-model guard is inert). Test-only convenience so existing
/// tests stay byte-identical; production passes the resolved harness via
/// [`resolved_required_bots_for_author`].
#[cfg(test)]
fn resolved_required_bots(settings: &Settings) -> Vec<String> {
    resolved_required_bots_for_author(settings, None)
}

/// The set of expected review logins that must have passed for the gate to
/// clear (x-4baa): `github_apps` (or its legacy `required_bots` alias) UNION
/// the resolved posting identity of each `peers` entry. loop-check stays
/// login-based; a peer with no resolvable identity fails closed via a sentinel.
///
/// `author_harness` is the invoking harness (`claude`/`codex`/`gemini`), resolved
/// from the ambient env markers by the caller. When it resolves to a model
/// family, the same-model guard (x-c2e7) replaces any peer login backed ONLY by
/// the author's own model with SAME_MODEL_PEER_SENTINEL, so a codex-authored run
/// with `peers: [codex]` can no longer review its own work and clear the gate.
/// `None` (unknown authorship) leaves the login set byte-identical - fail open.
fn resolved_required_bots_for_author(
    settings: &Settings,
    author_harness: Option<&str>,
) -> Vec<String> {
    // github_apps wins over the legacy required_bots alias when both are set.
    if settings.github_apps.is_some() && settings.required_bots.is_some() {
        eprintln!(
            "loop-check: both config.review.github_apps and required_bots set - using github_apps"
        );
    }
    let mut logins: Vec<String> = match settings
        .github_apps
        .as_ref()
        .or(settings.required_bots.as_ref())
    {
        Some(list) => list.clone(),
        None => DEFAULT_REQUIRED_BOTS
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };

    // Each peer contributes its posting identity to the expected-login set.
    // Shared identity (scalar peers) collapses to one login; per-peer map
    // identities each add their own. A dedup keeps a shared identity single.
    for peer in &settings.peers {
        let id = peer
            .identity
            .clone()
            .or_else(|| settings.peer_identity.clone());
        match id {
            Some(id) if !logins.iter().any(|l| l == &id) => logins.push(id),
            Some(_) => {} // already present (shared identity)
            None => {
                eprintln!(
                    "loop-check: config.review.peers entry '{}' has no posting identity (peer_identity unset) - gate fails closed",
                    peer.provider
                );
                if !logins.iter().any(|l| l == UNRESOLVED_PEER_SENTINEL) {
                    logins.push(UNRESOLVED_PEER_SENTINEL.to_string());
                }
            }
        }
    }

    // Same-model guard (x-c2e7): a peer login backed ONLY by the author's own
    // model cannot honestly satisfy the cross-model gate. Inert unless the
    // author harness resolves to a family (fail open on unknown authorship, so
    // the block above stays byte-identical). The GITHUB_APPS base set is never
    // touched - only logins contributed by `peers` are eligible.
    if let Some(author) = author_harness.filter(|_| !settings.peers.is_empty()) {
        if let Some(author_fam) = harness_family(author) {
            apply_same_model_guard(&mut logins, settings, author, author_fam);
        }
    }
    logins
}

/// Replace every peer-contributed login backed ONLY by same-model peers with
/// SAME_MODEL_PEER_SENTINEL and print one loud line per such login. A login with
/// >=1 cross-model peer (a different family, or an unknown provider) is left
/// alone. When a same-model peer login COLLIDES with a github_apps/required_bots
/// base login (`peer_identity` == an App login), the base login is kept (its App
/// requirement is not loosened) AND the sentinel is appended, so a same-model
/// review posted under that shared login can never be the thing that clears the
/// gate - the collision is a fail-closed hold, not an exemption (codex peer
/// review on PR #375). Peers are walked in config order so output is deterministic.
fn apply_same_model_guard(
    logins: &mut Vec<String>,
    settings: &Settings,
    author_harness: &str,
    author_fam: &str,
) {
    let base_set = settings
        .github_apps
        .as_ref()
        .or(settings.required_bots.as_ref());

    // Per distinct peer login, in first-seen order: does any backing peer differ
    // in model family, and the first same-model provider (for the message)?
    let mut seen: Vec<(String, bool, String)> = Vec::new();
    for peer in &settings.peers {
        let Some(login) = peer
            .identity
            .as_deref()
            .or(settings.peer_identity.as_deref())
        else {
            continue;
        };
        let cross = peer_family(peer) != Some(author_fam);
        match seen.iter_mut().find(|(l, _, _)| l.as_str() == login) {
            Some(entry) => entry.1 = entry.1 || cross,
            None => seen.push((login.to_string(), cross, peer.provider.clone())),
        }
    }

    for (login, any_cross, provider) in seen {
        if any_cross {
            continue;
        }
        if base_set.is_some_and(|set| set.contains(&login)) {
            // Collision: the peer posts under a required App login. Keep the App
            // requirement, but add the sentinel so this same-model login can't be
            // what clears the gate (never an exemption - fail closed).
            if !logins.iter().any(|l| l == SAME_MODEL_PEER_SENTINEL) {
                logins.push(SAME_MODEL_PEER_SENTINEL.to_string());
            }
        } else if let Some(slot) = logins.iter_mut().find(|l| **l == login) {
            // Peer-only login: replace it with the sentinel.
            *slot = SAME_MODEL_PEER_SENTINEL.to_string();
        }
        eprintln!(
            "loop-check: peer '{provider}' is the author's own model ({author_harness}-authored run) - the cross-model gate cannot be satisfied by it; configure a cross-model peer or a model route"
        );
    }
}

/// The OPTIONAL reviewer logins (config.review.optional_apps): honored-if-
/// present but never required. Their blocking findings hold the gate, but their
/// absence never does (x-4baa "honor if present"). Empty when unset.
fn resolved_optional_bots(settings: &Settings) -> Vec<String> {
    settings.optional_apps.clone().unwrap_or_default()
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

/// Pinned usage-limit markers a rate-limited review bot posts as an ISSUE
/// comment when it never posts a review object (PR #214). Matched
/// case-insensitively via `contains` against a lowercased body, mirroring the
/// pinned-string approach in `blocking_severity`. Kept tight: an under-match
/// degrades to the safe old block behavior; an over-match risks a false drop.
const USAGE_LIMIT_MARKERS: &[&str] = &["usage limits for code reviews", "codex usage limits"];

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
    /// Required bots dropped from `missing_bots` because they are env-blocked
    /// (rate-limited): they posted only a usage-limit comment, never a review.
    /// Keeping them in `missing_bots` wedged the gate until budget death
    /// (PR #214); dropping them lets the gate proceed on remaining evidence
    /// while the caller records the drop (AC1-UI). A bot is never in both
    /// lists - it is scanned only while still in `missing_bots`.
    usage_limited: Vec<String>,
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

    let mut missing_bots: Vec<String> = required_bots
        .iter()
        .zip(passed.iter())
        .filter(|(_, ok)| !**ok)
        .map(|(bot, _)| bot.clone())
        .collect();

    // (a) Usage-limit detection. A still-missing required bot that authored a
    // comment carrying a pinned usage-limit marker is env-blocked, not
    // hasn't-reviewed-yet: it will never post a review, so leaving it in
    // missing_bots blocks every fire until the budget cap kills the session
    // (PR #214). Move it OUT of missing_bots into usage_limited so the gate
    // proceeds on remaining evidence (the caller logs the drop + names the bot,
    // and the merge stays human-gated). Scoped to the bot's OWN author.login so
    // a stranger's comment never drops a required bot (AC1-ERR). Only
    // still-missing bots are scanned, so a bot that actually reviewed is never
    // usage-limited-dropped (AC1-EDGE).
    let mut usage_limited: Vec<String> = Vec::new();
    missing_bots.retain(|bot| {
        let rate_limited = comments.iter().any(|c| {
            let login = c
                .pointer("/author/login")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !login_matches_bot(login, bot) {
                return false;
            }
            let body = c
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            USAGE_LIMIT_MARKERS.iter().any(|m| body.contains(m))
        });
        if rate_limited {
            usage_limited.push(bot.clone());
            false
        } else {
            true
        }
    });

    ReviewInfo {
        latest_ts: final_ts,
        missing_bots,
        usage_limited,
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
    // (the loop never blocks on it). The claim key/holder/ttl are APPENDED after
    // the frontmatter by `fno target init`, so scan the whole manifest for them
    // (parse_manifest is frontmatter-bounded and would miss them). Root=None
    // routes node:<id> to the global claims root inside renew.
    if let (Some(key), Some(holder)) = (
        scan_manifest_field(&manifest_content, "target_claim_key"),
        scan_manifest_field(&manifest_content, "target_claim_holder"),
    ) {
        // Renew for the SAME window the claim was acquired with (default 2h,
        // matching init's `_CLAIM_TTL`), so the deadline never grows.
        let ttl_ms = scan_manifest_field(&manifest_content, "target_claim_ttl")
            .and_then(|s| crate::claims::parse_ttl_ms(&s))
            .unwrap_or(7_200_000);
        match crate::claims::renew(&key, &holder, ttl_ms, None) {
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
    //
    // x-81d9 (c): a genuinely unparseable settings.yaml fails CLOSED (the login
    // gate is pinned unsatisfiable) and emits loop_check_settings_unparseable,
    // rather than silently zeroing the required bots and shipping unreviewed.
    let parse_or_emit = |content: &str, path: &Path| -> Settings {
        match parse_settings_result(content) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "loop-check: config.toml unparseable ({}): {e} - failing the login gate closed",
                    path.display()
                );
                emit_to_both(
                    &project_events,
                    &global_events,
                    "loop_check_settings_unparseable",
                    serde_json::json!({"path": path.display().to_string(), "error": e}),
                );
                fail_closed_settings()
            }
        }
    };
    let settings = if let Some(ref explicit) = parsed.settings_path {
        if let Ok(sc) = std::fs::read_to_string(explicit) {
            parse_or_emit(&sc, explicit)
        } else {
            Settings::default()
        }
    } else {
        let global_path = parsed
            .global_settings_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(&home).join(".fno/config.toml"));
        let mut merged = std::fs::read_to_string(&global_path)
            .map(|sc| parse_or_emit(&sc, &global_path))
            .unwrap_or_default();
        let local_path = cwd.join(".fno/config.toml");
        if let Ok(sc) = std::fs::read_to_string(&local_path) {
            let local = parse_or_emit(&sc, &local_path);
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
            if local.github_apps.is_some() {
                merged.github_apps = local.github_apps;
            }
            if local.optional_apps.is_some() {
                merged.optional_apps = local.optional_apps;
            }
            if !local.reviewers.is_empty() {
                merged.reviewers = local.reviewers;
            }
            if !local.peers.is_empty() {
                merged.peers = local.peers;
            }
            if local.peer_identity.is_some() {
                merged.peer_identity = local.peer_identity;
            }
        }
        merged
    };

    // Resolve the must-have-reviewed list once (code default when unset). The
    // author harness (from the ambient env markers, shared with claims.rs) drives
    // the same-model peer guard (x-c2e7); None leaves the set unchanged.
    let author_harness = crate::claims::resolve_harness();
    let required_bots = resolved_required_bots_for_author(&settings, author_harness.as_deref());
    let optional_bots = resolved_optional_bots(&settings);

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

    // Operator review-finding gate input (x-f8d4). Resolve this session's node
    // (graph_node_id in the frontmatter; the appended target_claim_key is the
    // fallback) and scan events.jsonl for open findings. Node-scoped, NOT
    // head-pinned: read here so both the promise arms and the mute-probe
    // DonePRGreen path below see the same evidence. A malformed finding line
    // never blocks (AC3-FR) but is surfaced as an audit notice so a truncated
    // write can't vanish.
    let node_id = scan_manifest_field(&manifest_content, "graph_node_id").or_else(|| {
        scan_manifest_field(&manifest_content, "target_claim_key")
            .and_then(|k| k.strip_prefix("node:").map(|s| s.to_string()))
    });
    let (open_findings, malformed_findings) = match node_id.as_deref() {
        Some(n) => open_review_findings(&project_events, n),
        None => (Vec::new(), 0),
    };
    if malformed_findings > 0 {
        emit(
            "loop_check_malformed_finding",
            serde_json::json!({
                "session_id": session_id,
                "node": node_id,
                "malformed_lines": malformed_findings
            }),
        );
    }

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

        // Operator review-finding gate (x-f8d4, Locked Decision 3): an open
        // review_finding for this node HOLDS every success terminal-allow
        // (DonePlanned / DoneAdvisory / DoneBatched / DonePRGreen) until an
        // explicit resolve - a promise cannot self-authorize past an operator's
        // open comment. Placed AFTER the Aborted arm and gated on
        // `!backstop_tripped` so the anti-wedge safety valves still win: an
        // Aborted tag exits, and once the NoProgress backstop streak is reached
        // the session gives up rather than looping forever on an unresolved
        // finding. Fires on a promise OR a mute-probe (the paths that would
        // otherwise terminate-allow), never on an ordinary working fire.
        if !open_findings.is_empty()
            && !backstop_tripped
            && (intent == Intent::Promise || consecutive_after >= MUTE_PROBE_N)
        {
            let reason = build_findings_block_reason(&open_findings, malformed_findings);
            emit(
                "loop_check",
                serde_json::json!({
                    "session_id": session_id,
                    "fingerprint": fingerprint,
                    "fires": this_fire,
                    "consecutive_unchanged": consecutive_after,
                    "decision": "block",
                    "intent": if intent == Intent::Promise { "promise" } else { "backstop" },
                    "intent_source": intent_source,
                    "pr_state": fp_pr_state.as_str(),
                    "ci": fp_ci.render(),
                    "reviewed": false,
                    "open_findings": open_findings.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
                    "malformed_findings": malformed_findings,
                    "fp_read_failed": fp_read_failed
                }),
            );
            return (
                0,
                allow_output("block", None, &reason, this_fire, Some(fingerprint)),
            );
        }

        // Plan-only unit: a plan-only thread reached the plan boundary. Checked
        // BEFORE the advisory unit because DoneAdvisory is a ship reason (it
        // graduates the plan) and a plan-only thread must not graduate its own
        // plan. DonePlanned is benign: not a ship reason, not a postmortem.
        if manifest.planned && intent == Intent::Promise {
            emit(
                "termination",
                serde_json::json!({
                    "session_id": session_id,
                    "reason": "DonePlanned",
                    "message": "promise in plan-only unit"
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
                    Some(TerminationReason::DonePlanned),
                    "promise + plan-only unit; done",
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
            &optional_bots,
            &settings.external_reviewers,
            &settings.reviewers,
            &head_sha,
            &project_events,
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
                    // AC1-UI: name any rate-limited bot the gate proceeded
                    // without, so the terminal message and the emitted event
                    // agree on why a required bot is absent from the evidence.
                    let done_msg = if pr_info.usage_limited.is_empty() {
                        format!("PR #{} is green and reviewed", pr_info.number)
                    } else {
                        format!(
                            "PR #{} is green and reviewed (rate-limited, dropped from gate: {})",
                            pr_info.number,
                            pr_info.usage_limited.join(", ")
                        )
                    };
                    emit(
                        "termination",
                        serde_json::json!({
                            "session_id": session_id,
                            "reason": "DonePRGreen",
                            "message": done_msg.clone()
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
                            &done_msg,
                            this_fire,
                            Some(fingerprint),
                        ),
                    );
                }

                // DoneAwaitingMerge: done() failed SOLELY on CI-green
                // (PR open, reviewed, HEAD shipped, but CI red). Reached only
                // when !ci_ok because the DonePRGreen arm above returned - so
                // DonePRGreen precedence holds, and a merge that flipped the PR
                // green would have been caught by the fresh run_done this fire
                // (AC1-FR). If current main HEAD is red on the SAME checks
                // (strict subset, check-name granularity), a bg agent cannot
                // merge past it: terminate clean with a one-shot notify instead
                // of burning to NoProgress. Any PR-unique red or any gh
                // uncertainty falls through to the hold below (fail closed).
                //
                // `!pr_info.ci_has_pending` is load-bearing: ci_conclusion
                // reports Failure as soon as ONE check fails while others still
                // run, so without this guard the terminal could fire on a
                // partial-CI fire where the session's OWN new job is still
                // pending and about to turn red. The terminal must see fully
                // settled-red CI, never partial.
                //
                // `mergeable != "CONFLICTING"` guards a reviewed PR whose branch
                // conflicts with main: the human cannot merge past main-red until
                // it is rebased, so terminating here would drop the node from
                // retry circulation while it is un-mergeable. UNKNOWN (still
                // computing) is allowed - it clears on its own.
                if pr_open
                    && pr_info.reviewed
                    && head_shipped
                    && !ci_ok
                    && !pr_info.ci_has_pending
                    && pr_info.mergeable != "CONFLICTING"
                {
                    if let Some(main_failing) =
                        main_head_failing_checks(gh_bin, &cwd, MAIN_RUN_LOOKBACK)
                    {
                        if is_pre_existing_main_red(&pr_info.failing_checks, &main_failing) {
                            let proof = format!(
                                "same checks red on main (last {} completed runs): {}",
                                MAIN_RUN_LOOKBACK,
                                pr_info.failing_checks.join(", ")
                            );
                            let msg = format!(
                                "PR #{} complete and reviewed; awaiting merge past pre-existing main-red ({proof})",
                                pr_info.number
                            );
                            // Idempotency (Concurrency AC): emit + notify at most
                            // once per session; a re-eval or the two consumers
                            // racing still returns the terminal but does not
                            // double-notify.
                            if !already_emitted_awaiting_merge(&project_events, &session_id) {
                                emit(
                                    "termination",
                                    serde_json::json!({
                                        "session_id": session_id,
                                        "reason": "DoneAwaitingMerge",
                                        "message": msg.clone()
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
                                best_effort_notify(
                                    &format!(
                                        "PR #{} ready - merge past pre-existing main-red",
                                        pr_info.number
                                    ),
                                    &msg,
                                );
                            }
                            return (
                                0,
                                allow_output(
                                    "allow",
                                    Some(TerminationReason::DoneAwaitingMerge),
                                    &msg,
                                    this_fire,
                                    Some(fingerprint),
                                ),
                            );
                        }
                    }
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
        "continue working; no completion signal. If you are only waiting on an async check (CI/review) with nothing to do, wait silently and do not reply or narrate until the state changes.",
        &cwd,
        &session_id,
    );
    (
        0,
        allow_output("block", None, &continue_msg, this_fire, Some(fingerprint)),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_done(
    gh_bin: &str,
    cwd: &Path,
    ci_declared_none: bool,
    no_external: bool,
    required_bots: &[String],
    optional_bots: &[String],
    external_reviewers: &[String],
    reviewers: &[String],
    head_sha: &str,
    events_path: &Path,
) -> Result<PrInfo, (String, String)> {
    read_pr_info(
        gh_bin,
        cwd,
        ci_declared_none,
        no_external,
        required_bots,
        optional_bots,
        external_reviewers,
        reviewers,
        head_sha,
        events_path,
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
                "CI still running on PR #{}; wait for it to finish. Nothing to do here: wait silently and do not reply or narrate until this state changes.",
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
    }

    #[test]
    fn scan_manifest_field_reads_claim_fields_after_frontmatter() {
        // x-ba4b regression: `fno target init` APPENDS the node-claim fields
        // AFTER the closing `---`, so the frontmatter-bounded parse_manifest must
        // NOT be relied on for them - the whole-file scanner is what drives
        // renewal. Mirrors init's real manifest shape.
        let content = "---\nsession_id: s1\nattended: false\n---\n\
                       Immutable session manifest.\n\
                       target_claim_key: \"node:x-ba4b\"\n\
                       target_claim_holder: \"target-session:s1\"\n\
                       target_claim_ttl: \"2h\"\n";
        // parse_manifest (frontmatter-bounded) never sees the appended fields.
        let m = parse_manifest(content).unwrap();
        assert_eq!(m.session_id.as_deref(), Some("s1"));
        // The whole-file scanner does.
        assert_eq!(
            scan_manifest_field(content, "target_claim_key").as_deref(),
            Some("node:x-ba4b")
        );
        assert_eq!(
            scan_manifest_field(content, "target_claim_holder").as_deref(),
            Some("target-session:s1")
        );
        assert_eq!(
            scan_manifest_field(content, "target_claim_ttl")
                .as_deref()
                .and_then(crate::claims::parse_ttl_ms),
            Some(7_200_000)
        );
        assert_eq!(scan_manifest_field(content, "nonexistent_field"), None);
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
    fn parse_manifest_planned() {
        let content = "---\nsession_id: s\ncreated_at: 2026-06-05T00:00:00Z\nplanned: true\n---\n";
        let m = parse_manifest(content).unwrap();
        assert!(m.planned);
        assert!(!m.advisory); // planned is distinct from advisory (which graduates)
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
    fn parse_settings_nested_budget_and_ci() {
        // Flat config.toml: budget / ci are top-level tables (no config: wrapper).
        let cfg = "[budget.unattended]\ncost_cap_usd = 7.5\n\n[ci]\ndeclared_none = true\n";
        let s = parse_settings(cfg);
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
        let cfg = "budget_cap = 2.5\n";
        let s = parse_settings(cfg);
        assert_eq!(s.flat_budget_cap, Some(Ok(2.5)));
    }

    #[test]
    fn parse_settings_nested_budget() {
        let cfg = "[budget.attended]\nwall_clock_cap_minutes = 90\ncost_cap_usd = 10.0\n\n[budget.unattended]\nwall_clock_cap_minutes = 60\ncost_cap_usd = 5.0\n";
        let s = parse_settings(cfg);
        assert_eq!(s.attended_wall_cap_minutes, Some(Ok(90)));
        assert_eq!(s.attended_cost_cap_usd, Some(Ok(10.0)));
        assert_eq!(s.unattended_wall_cap_minutes, Some(Ok(60)));
        assert_eq!(s.unattended_cost_cap_usd, Some(Ok(5.0)));
    }

    #[test]
    fn parse_settings_ci_declared_none() {
        let cfg = "[ci]\ndeclared_none = true\n";
        let s = parse_settings(cfg);
        assert!(s.ci_declared_none);
    }

    #[test]
    fn parse_settings_comments_ignored() {
        let cfg =
            "# top comment\nbudget_cap = 1.0\n# another\n[ci]\n# inner\ndeclared_none = true\n";
        let s = parse_settings(cfg);
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
            failing_checks: vec![],
            ci_has_pending: false,
            mergeable: "UNKNOWN".to_string(),
            latest_review_ts: "none".to_string(),
            reviewed: false,
            missing_bots: vec![],
            usage_limited: vec![],
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

    // ── DoneAwaitingMerge classifier ───────────────────────────────────────

    #[test]
    fn failing_check_names_collects_fail_and_cancel_only() {
        let checks = serde_json::json!([
            {"name": "smoke",        "bucket": "fail"},
            {"name": "loc-ratchet",  "bucket": "pass"},
            {"name": "prompt-drift", "bucket": "cancel"},
            {"name": "self-test",    "bucket": "pending"},
            {"name": "doc-colo",     "bucket": "skipping"},
        ]);
        let mut got = failing_check_names(&checks);
        got.sort();
        assert_eq!(got, vec!["prompt-drift".to_string(), "smoke".to_string()]);
    }

    #[test]
    fn failing_check_names_empty_when_all_green() {
        let checks = serde_json::json!([{"name": "smoke", "bucket": "pass"}]);
        assert!(failing_check_names(&checks).is_empty());
        // Malformed input never panics, yields empty.
        assert!(failing_check_names(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn ci_has_pending_gates_partial_ci() {
        // One check failed while another still runs -> pending (must hold, not
        // terminate: the pending job could be the session's own new red).
        let partial = serde_json::json!([
            {"name": "smoke",   "bucket": "fail"},
            {"name": "rust-ci", "bucket": "pending"},
        ]);
        assert!(ci_has_pending_checks(&partial));
        // Fully settled red -> no pending -> eligible for the terminal.
        let settled = serde_json::json!([
            {"name": "smoke",   "bucket": "fail"},
            {"name": "rust-ci", "bucket": "pass"},
            {"name": "doc",     "bucket": "skipping"},
        ]);
        assert!(!ci_has_pending_checks(&settled));
        // Unrecognized bucket is treated as pending (fail safe).
        let unknown = serde_json::json!([{"name": "x", "bucket": "queued"}]);
        assert!(ci_has_pending_checks(&unknown));
        // Malformed input never panics.
        assert!(!ci_has_pending_checks(&serde_json::json!({})));
    }

    #[test]
    fn parse_failing_run_ids_only_failures_on_head_sha() {
        // Only failures whose headSha matches the current main HEAD count. Run 4
        // failed but belongs to an OLDER commit (headSha "old"), so a check it
        // failed that main HEAD has since fixed must NOT be classified pre-existing.
        let list = serde_json::json!([
            {"databaseId": 1, "conclusion": "failure", "headSha": "head"},
            {"databaseId": 2, "conclusion": "success", "headSha": "head"},
            {"databaseId": 3, "conclusion": "cancelled", "headSha": "head"},
            {"databaseId": 4, "conclusion": "failure", "headSha": "old"},
            {"databaseId": 5, "conclusion": "failure", "headSha": "head"},
        ]);
        assert_eq!(parse_failing_run_ids(&list, "head"), vec![1, 5]);
        // A different HEAD sha selects that commit's failures only.
        assert_eq!(parse_failing_run_ids(&list, "old"), vec![4]);
    }

    #[test]
    fn parse_failing_job_names_only_failed_jobs() {
        let view = serde_json::json!({
            "jobs": [
                {"name": "codex",   "conclusion": "success"},
                {"name": "cargo test + schema parity", "conclusion": "failure"},
                {"name": "gemini",  "conclusion": "failure"},
            ]
        });
        let mut got = parse_failing_job_names(&view);
        got.sort();
        assert_eq!(
            got,
            vec![
                "cargo test + schema parity".to_string(),
                "gemini".to_string()
            ]
        );
        // No jobs key -> empty, never panics.
        assert!(parse_failing_job_names(&serde_json::json!({})).is_empty());
    }

    /// AC1-HP: the core shape - PR fails only the one check main also fails.
    #[test]
    fn subset_rule_pr_failing_is_covered_by_main() {
        let pr = vec!["cargo test + schema parity".to_string()];
        let main = vec![
            "cargo test + schema parity".to_string(),
            "some other main-only red".to_string(),
        ];
        assert!(is_pre_existing_main_red(&pr, &main));
    }

    /// AC1-EDGE: a PR-unique failing check (its own breakage) blocks the terminal.
    #[test]
    fn subset_rule_pr_unique_red_blocks() {
        let pr = vec![
            "cargo test + schema parity".to_string(),
            "fmt gate".to_string(), // the session's own breakage
        ];
        let main = vec!["cargo test + schema parity".to_string()];
        assert!(!is_pre_existing_main_red(&pr, &main));
    }

    #[test]
    fn subset_rule_empty_pr_failing_never_eligible() {
        // Empty PR-failing is the DonePRGreen path, not this one.
        assert!(!is_pre_existing_main_red(&[], &["x".to_string()]));
        // Non-empty PR vs green main (empty) -> hold.
        assert!(!is_pre_existing_main_red(&["x".to_string()], &[]));
    }

    #[test]
    fn already_emitted_awaiting_merge_detects_prior_and_absence() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("events.jsonl");
        // Absent file -> false (fail open).
        assert!(!already_emitted_awaiting_merge(&events, "sess-A"));
        // A DonePRGreen termination for the same session must NOT count.
        std::fs::write(
            &events,
            "{\"type\":\"termination\",\"data\":{\"session_id\":\"sess-A\",\"reason\":\"DonePRGreen\"}}\n",
        )
        .unwrap();
        assert!(!already_emitted_awaiting_merge(&events, "sess-A"));
        // A prior DoneAwaitingMerge for sess-A counts; a different session does not.
        std::fs::write(
            &events,
            "{\"type\":\"termination\",\"data\":{\"session_id\":\"sess-A\",\"reason\":\"DoneAwaitingMerge\"}}\n",
        )
        .unwrap();
        assert!(already_emitted_awaiting_merge(&events, "sess-A"));
        assert!(!already_emitted_awaiting_merge(&events, "sess-B"));
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
        let settings_cfg = "budget_cap = 0.10\n";
        let settings = parse_settings(settings_cfg);
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
        let cfg = "budget_cap = \"not_a_number\"\n";
        let s = parse_settings(cfg);
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
        let cfg = "[review]\nrequired_bots = [\n  \"chatgpt-codex-connector\",\n  \"gemini-code-assist\",\n]\n";
        let s = parse_settings(cfg);
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
        let cfg = "[review]\nrequired_bots = []\n";
        let s = parse_settings(cfg);
        assert_eq!(s.required_bots, Some(Vec::new()));
    }

    #[test]
    fn parse_settings_required_bots_inline_list() {
        let cfg = "[review]\nrequired_bots = [\"codex\", \"gemini\"]\n";
        let s = parse_settings(cfg);
        assert_eq!(
            s.required_bots,
            Some(vec!["codex".to_string(), "gemini".to_string()])
        );
    }

    /// A bare scalar `required_bots = "gemini"` GATES on that one login (parity
    /// with peers + Python), rather than failing OPEN to no-gate on a
    /// bracket-less typo (codex P1 on #205).
    #[test]
    fn parse_settings_required_bots_scalar_is_singleton() {
        let cfg = "[review]\nrequired_bots = \"gemini\"\n";
        let s = parse_settings(cfg);
        assert_eq!(s.required_bots, Some(vec!["gemini".to_string()]));
        // github_apps behaves identically.
        let g = parse_settings("[review]\ngithub_apps = \"chatgpt-codex-connector\"\n");
        assert_eq!(
            g.github_apps,
            Some(vec!["chatgpt-codex-connector".to_string()])
        );
    }

    /// An ABSENT required_bots key resolves to the default (no gate), and a
    /// following block still parses.
    #[test]
    fn parse_settings_absent_required_bots_defaults() {
        let cfg = "[review]\ngithub_apps = []\n\n[ci]\ndeclared_none = true\n";
        let s = parse_settings(cfg);
        assert_eq!(
            s.required_bots, None,
            "absent key resolves to the no-gate default"
        );
        assert!(s.ci_declared_none, "following blocks still parse");
    }

    /// TOML strips inline comments natively - a `required_bots = []  # note` is
    /// still the declared empty form, and commented list forms still parse.
    #[test]
    fn parse_settings_required_bots_inline_comments_stripped() {
        let empty = parse_settings("[review]\nrequired_bots = []  # no review gate\n");
        assert_eq!(empty.required_bots, Some(Vec::new()));

        let inline =
            parse_settings("[review]\nrequired_bots = [\"chatgpt-codex-connector\"] # required\n");
        assert_eq!(
            inline.required_bots,
            Some(vec!["chatgpt-codex-connector".to_string()])
        );

        let block = parse_settings(
            "[review]\nrequired_bots = [ # the gate\n  \"chatgpt-codex-connector\", # codex\n]\n",
        );
        assert_eq!(
            block.required_bots,
            Some(vec!["chatgpt-codex-connector".to_string()])
        );

        // A scalar (with a trailing comment stripped) coerces to a single-login
        // gate, not no-gate (codex P1 on #205).
        let scalar = parse_settings("[review]\nrequired_bots = \"gemini\" # oops\n");
        assert_eq!(scalar.required_bots, Some(vec!["gemini".to_string()]));
    }

    #[test]
    fn parse_settings_required_bots_multiline_array() {
        let cfg = "[review]\nrequired_bots = [\n  \"chatgpt-codex-connector\",\n]\n";
        let s = parse_settings(cfg);
        assert_eq!(
            s.required_bots,
            Some(vec!["chatgpt-codex-connector".to_string()])
        );
    }

    #[test]
    fn parse_settings_required_bots_reads_under_review_table() {
        // required_bots lives under the flat [review] table (no config: wrapper).
        let cfg = "[review]\nrequired_bots = [\"chatgpt-codex-connector\"]\n";
        let s = parse_settings(cfg);
        assert_eq!(
            s.required_bots,
            Some(vec!["chatgpt-codex-connector".to_string()])
        );
    }

    #[test]
    fn parse_settings_malformed_fails_closed_not_zeroed() {
        // A malformed config.toml must NOT silently zero the gate (the old
        // fail-open); it fails CLOSED with an unsatisfiable sentinel so the ship
        // gate blocks visibly. Here: an unclosed table header.
        let cfg = "[review\nrequired_bots = []\n";
        assert!(
            parse_settings_result(cfg).is_err(),
            "malformed TOML must be a parse error"
        );
        let s = parse_settings(cfg);
        assert_eq!(
            s.required_bots,
            Some(vec![UNPARSEABLE_SETTINGS_SENTINEL.to_string()]),
            "a malformed file must fail closed, not zero the gate"
        );
        // The sentinel can never be satisfied by a real bot login.
        assert!(!login_matches_bot(
            "chatgpt-codex-connector",
            UNPARSEABLE_SETTINGS_SENTINEL
        ));
    }

    #[test]
    fn parse_settings_unparseable_fails_closed() {
        // AC3-UI: a genuinely malformed config file leaves the login gate
        // unsatisfiable (fail closed), never a silent no-gate. The production
        // caller additionally emits loop_check_settings_unparseable.
        let cfg = "[review]\nrequired_bots = [1, 2, 3\n"; // unclosed array
        assert!(parse_settings_result(cfg).is_err());
        let s = parse_settings(cfg);
        assert_eq!(
            resolved_required_bots(&s),
            vec![UNPARSEABLE_SETTINGS_SENTINEL.to_string()]
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

    // --- github_apps rename + required_bots alias (x-4baa US3/US4) ---

    // --- optional_apps: honored-if-present, never required (x-4baa) ---

    #[test]
    fn parse_settings_structural_scalar_degrades_like_python() {
        // A `{...}` flow-mapping value is not a login: scalar_as_singleton
        // returns None so the Rust reader agrees with Python's typed reader
        // (which drops a mapping to None), honoring the two-parser invariant
        // (codex P1 on #205). A numeric scalar stays a singleton (parity too).
        assert_eq!(scalar_as_singleton(" {login: codex}"), None);
        assert_eq!(scalar_as_singleton(" 123"), Some(vec!["123".to_string()]));
        let g = parse_settings("[review]\ngithub_apps = {login = \"codex\"}\n");
        assert_eq!(g.github_apps, None, "an inline table is not a login gate");
        let o = parse_settings("[review]\noptional_apps = {a = \"b\"}\n");
        assert_eq!(o.optional_apps, None);
    }

    #[test]
    fn parse_settings_optional_apps_forms() {
        // Inline, multi-line, and bare-scalar all parse.
        let inline = parse_settings("[review]\noptional_apps = [\"chatgpt-codex-connector\"]\n");
        assert_eq!(
            inline.optional_apps,
            Some(vec!["chatgpt-codex-connector".to_string()])
        );
        let block =
            parse_settings("[review]\noptional_apps = [\n  \"chatgpt-codex-connector\",\n]\n");
        assert_eq!(
            block.optional_apps,
            Some(vec!["chatgpt-codex-connector".to_string()])
        );
        let scalar = parse_settings("[review]\noptional_apps = \"chatgpt-codex-connector\"\n");
        assert_eq!(
            scalar.optional_apps,
            Some(vec!["chatgpt-codex-connector".to_string()])
        );
    }

    // --- reviewers: local-attestation gate (x-e703, Phase 2) ---

    #[test]
    fn parse_settings_reviewers_forms() {
        // Inline, block-under, key-aligned (PyYAML), bare scalar all parse; a
        // leading '/' is normalized off (parity with the Python validator).
        let inline = parse_settings("[review]\nreviewers = [\"sigma\", \"/code-review\"]\n");
        assert_eq!(
            inline.reviewers,
            vec!["sigma".to_string(), "code-review".to_string()]
        );
        let block = parse_settings("[review]\nreviewers = [\n  \"sigma\",\n]\n");
        assert_eq!(block.reviewers, vec!["sigma".to_string()]);
        let scalar = parse_settings("[review]\nreviewers = \"/code-review\"\n");
        assert_eq!(scalar.reviewers, vec!["code-review".to_string()]);
        let absent = parse_settings("[review]\ngithub_apps = []\n");
        assert!(absent.reviewers.is_empty());
    }

    #[test]
    fn parse_settings_reviewers_distinct_from_external_reviewers() {
        // Top-level external_reviewers and review.reviewers must not
        // cross-contaminate their list items.
        let cfg = "external_reviewers = [\"gemini\"]\n\n[review]\nreviewers = [\"sigma\"]\n";
        let s = parse_settings(cfg);
        assert_eq!(s.external_reviewers, vec!["gemini".to_string()]);
        assert_eq!(s.reviewers, vec!["sigma".to_string()]);
    }

    fn write_events(dir: &Path, lines: &[&str]) -> std::path::PathBuf {
        let p = dir.join("events.jsonl");
        std::fs::write(&p, lines.join("\n")).unwrap();
        p
    }

    #[test]
    fn reviewers_all_attested_empty_is_vacuously_true() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("nonexistent.jsonl");
        assert!(reviewers_all_attested(&p, &[], "abc"));
    }

    #[test]
    fn reviewers_all_attested_head_pinned_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"abc123","verdict":"pass"}}"#,
            ],
        );
        assert!(reviewers_all_attested(&p, &["sigma".to_string()], "abc123"));
    }

    #[test]
    fn reviewers_all_attested_stale_head_is_unsatisfied() {
        // Head-pin: a pass for a PRIOR commit must not satisfy the current HEAD
        // (AC1-EDGE / AC8-HP). A new commit invalidates the old attestation.
        let tmp = tempfile::tempdir().unwrap();
        let p = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"pass"}}"#,
            ],
        );
        assert!(!reviewers_all_attested(&p, &["sigma".to_string()], "NEW"));
    }

    #[test]
    fn reviewers_all_attested_fail_and_missing_are_unsatisfied() {
        let tmp = tempfile::tempdir().unwrap();
        // fail verdict -> unsatisfied
        let fail = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"fail"}}"#,
            ],
        );
        assert!(!reviewers_all_attested(&fail, &["sigma".to_string()], "h"));
        // missing file -> fail closed
        let gone = tmp.path().join("gone.jsonl");
        assert!(!reviewers_all_attested(&gone, &["sigma".to_string()], "h"));
    }

    #[test]
    fn reviewers_all_attested_conjunction_and_slash_normalized() {
        // Every reviewer must be attested (strict conjunction); a '/'-prefixed
        // config entry matches an event that emits the bare name and vice-versa.
        let tmp = tempfile::tempdir().unwrap();
        let p = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"pass"}}"#,
                r#"{"ts":"t","type":"review_attestation","source":"target","data":{"reviewer":"code-review","head_sha":"h","verdict":"pass"}}"#,
            ],
        );
        // Both present -> satisfied ('/code-review' config vs 'code-review' event).
        assert!(reviewers_all_attested(
            &p,
            &["sigma".to_string(), "/code-review".to_string()],
            "h"
        ));
        // One missing -> unsatisfied.
        assert!(!reviewers_all_attested(
            &p,
            &["sigma".to_string(), "declare".to_string()],
            "h"
        ));
    }

    #[test]
    fn parse_settings_reviewers_malformed_mapping_fails_closed() {
        // A `{...}` mapping value must NOT drop to no-gate (Python raises here);
        // Rust stores an unsatisfiable sentinel so the gate stays active but can
        // never clear (codex peer review P1).
        let s = parse_settings("[review]\nreviewers = {a = \"b\"}\n");
        assert_eq!(s.reviewers, vec![MALFORMED_REVIEWERS_SENTINEL.to_string()]);
        let tmp = tempfile::tempdir().unwrap();
        let p = write_events(tmp.path(), &[]);
        assert!(
            !reviewers_all_attested(&p, &s.reviewers, "h"),
            "a malformed-reviewers sentinel must never be satisfiable"
        );
    }

    #[test]
    fn parse_settings_reviewers_seq_with_nonscalar_fails_closed() {
        // gemini medium: a non-scalar item INSIDE the reviewers list (Python
        // raises on it) must fail CLOSED with the sentinel, not silently drop
        // the entry and gate on the survivors.
        let bad = parse_settings("[review]\nreviewers = [\"sigma\", {a = \"b\"}]\n");
        assert_eq!(
            bad.reviewers,
            vec![MALFORMED_REVIEWERS_SENTINEL.to_string()]
        );
        // A clean all-scalar list still parses normally.
        let ok = parse_settings("[review]\nreviewers = [\"sigma\", \"declare\"]\n");
        assert_eq!(
            ok.reviewers,
            vec!["sigma".to_string(), "declare".to_string()]
        );
    }

    #[test]
    fn reviewers_all_attested_latest_verdict_wins() {
        // events.jsonl is append-ordered: a later attestation supersedes an
        // earlier one for the same reviewer at the same head (codex peer P1).
        let tmp = tempfile::tempdir().unwrap();
        // pass THEN fail -> latest is fail -> unsatisfied.
        let pf = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t1","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"pass"}}"#,
                r#"{"ts":"t2","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"fail"}}"#,
            ],
        );
        assert!(
            !reviewers_all_attested(&pf, &["sigma".to_string()], "h"),
            "a fail posted after a pass must revoke it"
        );
        // fail THEN pass -> latest is pass -> satisfied (re-review cleared it).
        let fp = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t1","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"fail"}}"#,
                r#"{"ts":"t2","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"pass"}}"#,
            ],
        );
        assert!(
            reviewers_all_attested(&fp, &["sigma".to_string()], "h"),
            "a pass posted after a fail must restore satisfaction"
        );
    }

    // ── operator review-finding gate (x-f8d4) ────────────────────────────────

    #[test]
    fn review_finding_open_then_resolved_clears() {
        // AC2-HP: an open review_finding gates; an explicit resolve clears it.
        let tmp = tempfile::tempdir().unwrap();
        let open = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t1","type":"review_finding","source":"observer","data":{"finding_id":"f1","node":"x-1","text":"off-by-one in the loop\nsecond line"}}"#,
            ],
        );
        let (findings, malformed) = open_review_findings(&open, "x-1");
        assert_eq!(malformed, 0);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].id, "f1");
        assert_eq!(findings[0].first_line, "off-by-one in the loop"); // first line only

        // resolve clears it (node-scoped, only an explicit resolve).
        let resolved = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t1","type":"review_finding","source":"observer","data":{"finding_id":"f1","node":"x-1","text":"off-by-one"}}"#,
                r#"{"ts":"t2","type":"review_finding_resolved","source":"observer","data":{"finding_id":"f1"}}"#,
            ],
        );
        assert!(open_review_findings(&resolved, "x-1").0.is_empty());
    }

    #[test]
    fn review_finding_is_node_scoped() {
        // A finding for a different node must not gate this node.
        let tmp = tempfile::tempdir().unwrap();
        let p = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t","type":"review_finding","source":"observer","data":{"finding_id":"f1","node":"x-OTHER","text":"not mine"}}"#,
            ],
        );
        assert!(open_review_findings(&p, "x-mine").0.is_empty());
        assert_eq!(open_review_findings(&p, "x-OTHER").0.len(), 1);
    }

    #[test]
    fn review_finding_malformed_notices_not_blocks() {
        // AC3-FR: a structurally-unparseable review_finding line does NOT block
        // (no open finding), but is counted for the audit notice. A review_finding
        // missing its id is likewise a malformed notice, never a gating finding.
        let tmp = tempfile::tempdir().unwrap();
        // A truncated (unparseable) line that still carries the review_finding marker.
        let truncated = r#"{"ts":"t","type":"review_finding","data":{"finding_id":"f1"#;
        let id_less = r#"{"ts":"t","type":"review_finding","source":"observer","data":{"node":"x-1","text":"no id"}}"#;
        let good = r#"{"ts":"t","type":"review_finding","source":"observer","data":{"finding_id":"good","node":"x-1","text":"real one"}}"#;
        let p = write_events(tmp.path(), &[truncated, id_less, good]);
        let (findings, malformed) = open_review_findings(&p, "x-1");
        assert_eq!(findings.len(), 1, "only the well-formed finding gates");
        assert_eq!(findings[0].id, "good");
        assert_eq!(
            malformed, 2,
            "the truncated line + the id-less line are noticed"
        );
    }

    #[test]
    fn review_finding_block_reason_quotes_first_plus_count() {
        let open = vec![
            OpenFinding {
                id: "aaa".into(),
                first_line: "the bug".into(),
            },
            OpenFinding {
                id: "bbb".into(),
                first_line: "another".into(),
            },
        ];
        let r = build_findings_block_reason(&open, 1);
        assert!(r.contains("aaa"));
        assert!(r.contains("the bug"));
        assert!(r.contains("fno annotate resolve aaa"));
        assert!(r.contains("[+1 more]"));
        assert!(r.contains("1 malformed"));
    }

    #[test]
    fn resolved_optional_is_separate_from_required() {
        // An optional-only config leaves the REQUIRED set empty (never waited
        // on) while the optional set carries the honored-if-present login.
        let s = parse_settings(
            "[review]\ngithub_apps = []\noptional_apps = [\"chatgpt-codex-connector\"]\n",
        );
        assert!(
            resolved_required_bots(&s).is_empty(),
            "optional must not be required"
        );
        assert_eq!(
            resolved_optional_bots(&s),
            vec!["chatgpt-codex-connector".to_string()]
        );
    }

    #[test]
    fn parse_settings_github_apps_block_list() {
        let cfg = "[review]\ngithub_apps = [\n  \"chatgpt-codex-connector\",\n]\n";
        let s = parse_settings(cfg);
        assert_eq!(
            s.github_apps,
            Some(vec!["chatgpt-codex-connector".to_string()])
        );
    }

    #[test]
    fn parse_settings_github_apps_inline_and_empty() {
        let s = parse_settings("[review]\ngithub_apps = [\"a\", \"b\"]\n");
        assert_eq!(s.github_apps, Some(vec!["a".to_string(), "b".to_string()]));
        let e = parse_settings("[review]\ngithub_apps = []\n");
        assert_eq!(e.github_apps, Some(Vec::new()));
    }

    #[test]
    fn resolved_github_apps_wins_over_required_bots_alias() {
        // Both set -> github_apps wins (Locked Decision 2).
        let s = Settings {
            github_apps: Some(vec!["new-bot".to_string()]),
            required_bots: Some(vec!["old-bot".to_string()]),
            ..Default::default()
        };
        assert_eq!(resolved_required_bots(&s), vec!["new-bot".to_string()]);
        // required_bots-only still gates (legacy alias, AC2-HP).
        let legacy = Settings {
            required_bots: Some(vec!["old-bot".to_string()]),
            ..Default::default()
        };
        assert_eq!(resolved_required_bots(&legacy), vec!["old-bot".to_string()]);
    }

    // --- peers -> gate union (x-4baa US4) ---

    #[test]
    fn parse_settings_peers_inline_scalars() {
        let cfg = "[review]\npeers = [\"codex\", \"gemini\"]\npeer_identity = \"fno-peer-bot\"\n";
        let s = parse_settings(cfg);
        assert_eq!(s.peers.len(), 2);
        assert_eq!(s.peers[0].provider, "codex");
        assert_eq!(s.peer_identity.as_deref(), Some("fno-peer-bot"));
    }

    #[test]
    fn parse_settings_peers_block_maps_with_identity() {
        // A heterogeneous array: an inline-table peer + a bare scalar provider.
        let cfg = "[review]\npeers = [{provider = \"codex\", identity = \"fno-codex-bot\"}, \"gemini\"]\n";
        let s = parse_settings(cfg);
        assert_eq!(s.peers.len(), 2);
        assert_eq!(s.peers[0].provider, "codex");
        assert_eq!(s.peers[0].identity.as_deref(), Some("fno-codex-bot"));
        assert_eq!(s.peers[1].provider, "gemini");
        assert_eq!(s.peers[1].identity, None);
    }

    #[test]
    fn resolved_peers_shared_identity_collapses_to_one_login() {
        // Scalar peers share peer_identity -> the gate is that one login on top
        // of github_apps (AC1-HP: no App bot, just the peer identity).
        let s = Settings {
            github_apps: Some(Vec::new()),
            peers: vec![
                PeerEntry {
                    provider: "codex".into(),
                    model: None,
                    identity: None,
                },
                PeerEntry {
                    provider: "gemini".into(),
                    model: None,
                    identity: None,
                },
            ],
            peer_identity: Some("fno-peer-bot".into()),
            ..Default::default()
        };
        assert_eq!(resolved_required_bots(&s), vec!["fno-peer-bot".to_string()]);
    }

    #[test]
    fn resolved_peers_per_entry_identities_each_add_a_login() {
        let s = Settings {
            github_apps: Some(vec!["chatgpt-codex-connector".into()]),
            peers: vec![
                PeerEntry {
                    provider: "codex".into(),
                    model: None,
                    identity: Some("fno-codex-bot".into()),
                },
                PeerEntry {
                    provider: "gemini".into(),
                    model: None,
                    identity: Some("fno-gemini-bot".into()),
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            resolved_required_bots(&s),
            vec![
                "chatgpt-codex-connector".to_string(),
                "fno-codex-bot".to_string(),
                "fno-gemini-bot".to_string(),
            ]
        );
    }

    #[test]
    fn parse_settings_github_apps_and_peers_together() {
        // github_apps + peers + peer_identity in one [review] table all parse.
        let cfg = "[review]\ngithub_apps = [\"chatgpt-codex-connector\"]\npeers = [\"codex\"]\npeer_identity = \"fno-peer-bot\"\n";
        let s = parse_settings(cfg);
        assert_eq!(
            s.github_apps,
            Some(vec!["chatgpt-codex-connector".to_string()]),
            "github_apps item must be collected"
        );
        assert_eq!(s.peers.len(), 1, "peers item must be collected");
        assert_eq!(s.peers[0].provider, "codex");
        assert_eq!(s.peer_identity.as_deref(), Some("fno-peer-bot"));
    }

    #[test]
    fn parse_settings_required_bots_single_item() {
        let cfg = "[review]\nrequired_bots = [\"chatgpt-codex-connector\"]\n";
        let s = parse_settings(cfg);
        assert_eq!(
            s.required_bots,
            Some(vec!["chatgpt-codex-connector".to_string()])
        );
    }

    #[test]
    fn parse_settings_peers_single_mapping_is_one_peer() {
        // codex peer review P1: a single top-level table for peers (what
        // Python's coerce_peers wraps as [dict]) must parse as ONE peer, not be
        // silently dropped - dropping it is a fail-open on a configured peer gate.
        let block = parse_settings(
            "[review]\npeers = {provider = \"codex\", identity = \"fno-codex-bot\"}\n",
        );
        assert_eq!(block.peers.len(), 1, "table peers must be one peer");
        assert_eq!(block.peers[0].provider, "codex");
        assert_eq!(block.peers[0].identity.as_deref(), Some("fno-codex-bot"));
        // A dotted-table form parses identically.
        let dotted = parse_settings(
            "[review.peers]\nprovider = \"gemini\"\nidentity = \"fno-gemini-bot\"\n",
        );
        assert_eq!(dotted.peers.len(), 1);
        assert_eq!(dotted.peers[0].provider, "gemini");
        assert_eq!(dotted.peers[0].identity.as_deref(), Some("fno-gemini-bot"));
    }

    #[test]
    fn parse_settings_peers_bare_scalar_is_one_provider() {
        // `peers = "codex"` (scalar) matches Python's coerce_peers -> one peer,
        // NOT a silent drop (which would fail open + diverge from Python).
        let cfg = "[review]\npeers = \"codex\"\npeer_identity = \"fno-peer-bot\"\n";
        let s = parse_settings(cfg);
        assert_eq!(s.peers.len(), 1);
        assert_eq!(s.peers[0].provider, "codex");
        // The gate then resolves on the shared identity (fail-closed if unset).
        assert_eq!(resolved_required_bots(&s), vec!["fno-peer-bot".to_string()]);
    }

    #[test]
    fn parse_settings_peers_array_of_tables() {
        // An array mixing an inline-table peer and a bare scalar provider.
        let cfg = "[review]\npeers = [{provider = \"codex\", identity = \"fno-codex-bot\"}, \"gemini\"]\n";
        let s = parse_settings(cfg);
        assert_eq!(s.peers.len(), 2);
        assert_eq!(s.peers[0].provider, "codex");
        assert_eq!(s.peers[0].identity.as_deref(), Some("fno-codex-bot"));
        assert_eq!(s.peers[1].provider, "gemini");
    }

    #[test]
    fn parse_settings_peers_map_identity_before_provider() {
        // The map parser is order-agnostic (gemini HIGH on #205): `identity`
        // before `provider` must still resolve both fields.
        let cfg = "[review]\npeers = [{identity = \"fno-codex-bot\", provider = \"codex\"}, {provider = \"gemini\", identity = \"fno-gemini-bot\"}]\n";
        let s = parse_settings(cfg);
        assert_eq!(s.peers.len(), 2);
        assert_eq!(s.peers[0].provider, "codex");
        assert_eq!(s.peers[0].identity.as_deref(), Some("fno-codex-bot"));
        assert_eq!(s.peers[1].provider, "gemini");
        assert_eq!(s.peers[1].identity.as_deref(), Some("fno-gemini-bot"));
    }

    #[test]
    fn resolved_peers_without_identity_fails_closed() {
        // A peer with no resolvable identity injects an unmatchable sentinel so
        // the gate can never go green (fail closed), rather than silently
        // dropping the reviewer (fail open).
        let s = Settings {
            github_apps: Some(Vec::new()),
            peers: vec![PeerEntry {
                provider: "codex".into(),
                model: None,
                identity: None,
            }],
            peer_identity: None,
            ..Default::default()
        };
        let logins = resolved_required_bots(&s);
        assert!(logins.iter().any(|l| l == UNRESOLVED_PEER_SENTINEL));
        // The sentinel matches no real login, so missing_bots stays non-empty.
        assert!(!login_matches_bot("fno-peer-bot", UNRESOLVED_PEER_SENTINEL));
        assert!(!login_matches_bot(
            "chatgpt-codex-connector[bot]",
            UNRESOLVED_PEER_SENTINEL
        ));
    }

    // ---- same-model peer guard (x-c2e7) -----------------------------------

    /// US5: effective model family resolution across bare providers, routes,
    /// malformed routes (fall back to provider), and unknown providers (None).
    #[test]
    fn peer_family_mapping_table() {
        let bare = |p: &str| PeerEntry {
            provider: p.into(),
            model: None,
            identity: None,
        };
        let routed = |p: &str, m: &str| PeerEntry {
            provider: p.into(),
            model: Some(m.into()),
            identity: None,
        };
        // harness_family: names + aliases + case-insensitivity; unknown -> None.
        assert_eq!(harness_family("claude"), Some("anthropic"));
        assert_eq!(harness_family("ANTHROPIC"), Some("anthropic"));
        assert_eq!(harness_family("codex"), Some("openai"));
        assert_eq!(harness_family("gemini"), Some("google"));
        assert_eq!(harness_family("zai"), None);
        // route_provider: exactly two non-empty parts, else None (fall back).
        assert_eq!(route_provider("zai,glm-5.2"), Some("zai"));
        assert_eq!(route_provider(" openai , gpt-5 "), Some("openai"));
        assert_eq!(route_provider("gpt-5"), None); // no comma -> malformed
        assert_eq!(route_provider("zai,"), None); // empty model -> malformed
        assert_eq!(route_provider(",glm"), None); // empty provider -> malformed
        assert_eq!(route_provider("a,b,c"), None); // three parts -> malformed

        // peer_family: bare provider, valid route wins, malformed falls back.
        assert_eq!(peer_family(&bare("codex")), Some("openai"));
        assert_eq!(peer_family(&bare("grok")), None); // unknown -> never matches
        assert_eq!(peer_family(&routed("claude", "zai,glm-5.2")), None); // route wins
        assert_eq!(
            peer_family(&routed("codex", "openai,gpt-5")),
            Some("openai")
        );
        assert_eq!(peer_family(&routed("codex", "gpt-5")), Some("openai")); // malformed -> provider
    }

    /// AC1-HP: codex author + `peers: [codex]` -> the peer login is replaced by
    /// the same-model sentinel so the gate cannot clear.
    #[test]
    fn same_model_peer_holds_gate() {
        let s = Settings {
            github_apps: Some(Vec::new()),
            peers: vec![PeerEntry {
                provider: "codex".into(),
                model: None,
                identity: None,
            }],
            peer_identity: Some("fno-peer-bot".into()),
            ..Default::default()
        };
        let logins = resolved_required_bots_for_author(&s, Some("codex"));
        assert!(logins.iter().any(|l| l == SAME_MODEL_PEER_SENTINEL));
        assert!(!logins.iter().any(|l| l == "fno-peer-bot"));
    }

    /// AC2-HP: codex author + `peers: [gemini]` (cross-model) clears exactly as
    /// today - the login stays, no sentinel.
    #[test]
    fn cross_model_peer_login_unchanged() {
        let s = Settings {
            github_apps: Some(Vec::new()),
            peers: vec![PeerEntry {
                provider: "gemini".into(),
                model: None,
                identity: None,
            }],
            peer_identity: Some("fno-peer-bot".into()),
            ..Default::default()
        };
        let logins = resolved_required_bots_for_author(&s, Some("codex"));
        assert_eq!(logins, vec!["fno-peer-bot".to_string()]);
    }

    /// US1 / step-3b: a claude author with a routed claude peer
    /// (`{provider: claude, model: "zai,glm-5.2"}`) is cross-model (GLM via zai)
    /// -> the login stays.
    #[test]
    fn routed_claude_peer_is_cross_model() {
        let s = Settings {
            github_apps: Some(Vec::new()),
            peers: vec![PeerEntry {
                provider: "claude".into(),
                model: Some("zai,glm-5.2".into()),
                identity: None,
            }],
            peer_identity: Some("fno-peer-bot".into()),
            ..Default::default()
        };
        let logins = resolved_required_bots_for_author(&s, Some("claude"));
        assert_eq!(logins, vec!["fno-peer-bot".to_string()]);
    }

    /// AC3-ERR: a claude peer routed back to the author's own family
    /// (`anthropic,...`, hand-edited past the loader) is same-model -> sentinel.
    #[test]
    fn same_family_route_holds_gate() {
        let s = Settings {
            github_apps: Some(Vec::new()),
            peers: vec![PeerEntry {
                provider: "claude".into(),
                model: Some("anthropic,claude-opus".into()),
                identity: None,
            }],
            peer_identity: Some("fno-peer-bot".into()),
            ..Default::default()
        };
        let logins = resolved_required_bots_for_author(&s, Some("claude"));
        assert!(logins.iter().any(|l| l == SAME_MODEL_PEER_SENTINEL));
        assert!(!logins.iter().any(|l| l == "fno-peer-bot"));
    }

    /// AC5-EDGE: codex author + `peers: [codex, gemini]` sharing one identity
    /// stays satisfiable (gemini backs the login) -> login kept, no sentinel.
    #[test]
    fn shared_identity_mixed_peers_stays_satisfiable() {
        let s = Settings {
            github_apps: Some(Vec::new()),
            peers: vec![
                PeerEntry {
                    provider: "codex".into(),
                    model: None,
                    identity: None,
                },
                PeerEntry {
                    provider: "gemini".into(),
                    model: None,
                    identity: None,
                },
            ],
            peer_identity: Some("fno-peer-bot".into()),
            ..Default::default()
        };
        let logins = resolved_required_bots_for_author(&s, Some("codex"));
        assert_eq!(logins, vec!["fno-peer-bot".to_string()]);
    }

    /// AC6-FR: unknown harness (None) leaves the login set byte-identical to the
    /// no-guard wrapper, even for a would-be same-model config.
    #[test]
    fn unknown_harness_is_byte_identical() {
        let s = Settings {
            github_apps: Some(vec!["chatgpt-codex-connector".into()]),
            peers: vec![PeerEntry {
                provider: "codex".into(),
                model: None,
                identity: None,
            }],
            peer_identity: Some("fno-peer-bot".into()),
            ..Default::default()
        };
        // None author => guard inert => equals the no-harness wrapper exactly.
        assert_eq!(
            resolved_required_bots_for_author(&s, None),
            resolved_required_bots(&s)
        );
        assert!(!resolved_required_bots_for_author(&s, None)
            .iter()
            .any(|l| l == SAME_MODEL_PEER_SENTINEL));
    }

    /// A same-model peer whose identity COLLIDES with a required App login is
    /// fail-closed, not exempt (codex peer review on PR #375): the App login is
    /// kept (its requirement is not loosened) AND the sentinel is added, so a
    /// same-model review under the shared login cannot clear the gate.
    #[test]
    fn base_app_login_collision_is_fail_closed() {
        let s = Settings {
            github_apps: Some(vec!["fno-peer-bot".into()]),
            peers: vec![PeerEntry {
                provider: "codex".into(),
                model: None,
                identity: None,
            }],
            peer_identity: Some("fno-peer-bot".into()),
            ..Default::default()
        };
        let logins = resolved_required_bots_for_author(&s, Some("codex"));
        assert!(logins.iter().any(|l| l == "fno-peer-bot")); // App requirement kept
        assert!(logins.iter().any(|l| l == SAME_MODEL_PEER_SENTINEL)); // gate held
    }

    /// A codex/gemini peer's `model` route is NOT honored (only claude transport
    /// executes a route; codex/gemini dispatch runs the bare provider). A codex
    /// peer with a zai route stays openai-family -> same-model on a codex author,
    /// closing the route-bypass codex flagged on PR #375.
    #[test]
    fn non_claude_route_is_ignored() {
        let routed_codex = PeerEntry {
            provider: "codex".into(),
            model: Some("zai,glm-5.2".into()),
            identity: None,
        };
        assert_eq!(peer_family(&routed_codex), Some("openai"));
        let s = Settings {
            github_apps: Some(Vec::new()),
            peers: vec![routed_codex],
            peer_identity: Some("fno-peer-bot".into()),
            ..Default::default()
        };
        let logins = resolved_required_bots_for_author(&s, Some("codex"));
        assert!(logins.iter().any(|l| l == SAME_MODEL_PEER_SENTINEL));
        assert!(!logins.iter().any(|l| l == "fno-peer-bot"));
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

    #[test]
    fn compute_review_info_usage_limited_bot_dropped() {
        // AC1-HP: a required bot that posted only a usage-limit comment (no
        // review) leaves missing_bots for usage_limited, so the gate proceeds.
        let required = vec!["chatgpt-codex-connector".to_string()];
        let json = serde_json::json!({
            "reviews": [],
            "comments": [
                {"author": {"login": "chatgpt-codex-connector"},
                 "body": "You have reached your Codex usage limits for code reviews.",
                 "createdAt": "2026-07-06T01:00:00Z"}
            ]
        });
        let info = compute_review_info(&json, &required);
        assert!(info.missing_bots.is_empty());
        assert_eq!(
            info.usage_limited,
            vec!["chatgpt-codex-connector".to_string()]
        );
        assert!(info.all_required_passed());
    }

    #[test]
    fn compute_review_info_usage_limit_only_own_comment_counts() {
        // AC1-ERR: a usage-limit marker in a HUMAN's comment must not drop the
        // bot - detection is scoped to the bot's own author.login.
        let required = vec!["chatgpt-codex-connector".to_string()];
        let json = serde_json::json!({
            "reviews": [],
            "comments": [
                {"author": {"login": "some-human"},
                 "body": "The bot hit its usage limits for code reviews, ugh.",
                 "createdAt": "2026-07-06T01:00:00Z"}
            ]
        });
        let info = compute_review_info(&json, &required);
        assert_eq!(
            info.missing_bots,
            vec!["chatgpt-codex-connector".to_string()]
        );
        assert!(info.usage_limited.is_empty());
        assert!(!info.all_required_passed());
    }

    #[test]
    fn compute_review_info_real_review_beats_ratelimit_comment() {
        // AC1-EDGE: a bot that posted a usage-limit comment earlier AND a real
        // COMMENTED review is counted as passed, never usage-limited (it is
        // never in missing_bots to be scanned).
        let required = vec!["chatgpt-codex-connector".to_string()];
        let json = serde_json::json!({
            "reviews": [
                {"author": {"login": "chatgpt-codex-connector"}, "state": "COMMENTED",
                 "submittedAt": "2026-07-06T02:00:00Z"}
            ],
            "comments": [
                {"author": {"login": "chatgpt-codex-connector"},
                 "body": "codex usage limits reached",
                 "createdAt": "2026-07-06T01:00:00Z"}
            ]
        });
        let info = compute_review_info(&json, &required);
        assert!(info.missing_bots.is_empty());
        assert!(info.usage_limited.is_empty());
        assert!(info.all_required_passed());
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
