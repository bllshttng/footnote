//! `fno-agents loop-check` verb (Task 1.1, ab-d0337fbc).
//!
//! Single entry-point decision-maker for the target stop hook. Reads external
//! state (manifest, transcript, git, gh, events, ledger) and returns a JSON
//! decision object. The manifest is NEVER mutated; the only write surface is
//! append-only event logs.
//!
//! Module name starts with "loop" to match the LOC-ratchet glob `crates/fno-agents/src/loop*`.

use crate::{completion_output::allow_output, delivery_completion::pr_passes};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// ── public types ──────────────────────────────────────────────────────────────

/// Why the loop terminated. Serialized as the exact string enum the spec names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminationReason {
    DonePRGreen,
    DoneAdvisory,
    DoneDelivery,
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
    /// A PR is green, mergeable, and nothing objected - but nothing reviewed it
    /// either (coverage 0 or Unknown). The old gate named this state
    /// `DonePRGreen` because its three conjuncts all ask "did anyone object"
    /// and none asks "did anyone review" (x-0eaf). Terminal on the first
    /// evaluation (no loop iteration spent waiting - that is what keeps the
    /// PR #214 wedge from returning), and deliberately NOT a ship reason (out
    /// of finalize.SHIP_REASONS -> no plan stamp/graduate), shaped exactly like
    /// `DoneAwaitingMerge`: never merges, never marks the node done. The
    /// autonomous merge is refused structurally because `should_arm_auto_merge`
    /// arms only on `DonePRGreen`; a human (or out-of-band) merge then the
    /// reconcile path closes it. The discriminator is coverage, NOT the
    /// `attended` manifest field (x-be78: that field lies for spawned workers).
    DoneUnreviewed,
    /// Work complete (PR open, green, HEAD shipped) but `done()` fails because a
    /// required review bot is rate-limited: it posted a usage-limit (quota)
    /// comment instead of a review, so the gate cannot be auto-satisfied. The
    /// agent cannot make a rate-limited bot recover, so holding would wedge to
    /// budget death (the PR #214 shape); instead the loop terminates cleanly.
    /// Terminal, but NOT a ship reason (like DoneAwaitingMerge): never merges,
    /// never graduates - a human merges after a real review (or quota recovery
    /// / a local review and a re-run), then the out-of-band-merge reconcile
    /// path closes it. This is the fail-closed flip of the old "drop the bot
    /// and proceed" behavior that let ~10 PRs (#890-#912) merge unreviewed
    /// (x-9ab2).
    DoneAwaitingReview,
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

// ── manifest parsing ──────────────────────────────────────────────────────────

/// Fields parsed from target-state.md YAML frontmatter.
#[derive(Debug)]
struct Manifest {
    session_id: Option<String>,
    /// The harness session that ran `fno target init` in this worktree
    /// (claude UUID / codex thread / etc). Distinct from `session_id`, which is
    /// the target run id: the two differ, and this is the value an attestation's
    /// attester_session_id is compared against to detect self-attestation.
    harness_session_id: Option<String>,
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
    /// Plan doc backing this session; source of the `done_probes` declaration.
    plan_path: Option<String>,
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
            harness_session_id: None,
            created_at: None,
            attended: true, // spec: attended defaults to true
            advisory: false,
            no_ship: false,
            no_external: false,
            batched: false,
            planned: false,
            plan_path: None,
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
                "harness_session_id" => {
                    // init writes `harness_session_id: ${_HARNESS_SESSION:-null}`,
                    // so an unresolvable session lands as the literal string "null"
                    // (and an empty value as ""). Treat both as absent - the shell
                    // side (target-stop-hook.sh) strips "null" the same way - or a
                    // real attester compared against Some("null") mislabels as
                    // other_session instead of unknown.
                    if v != "null" && !v.is_empty() {
                        m.harness_session_id = Some(v.to_string());
                    }
                }
                "created_at" => m.created_at = Some(v.to_string()),
                "attended" => m.attended = v == "true",
                "advisory" => m.advisory = v == "true",
                "no_ship" => m.no_ship = v == "true",
                "no_external" => m.no_external = v == "true",
                "batched" => m.batched = v == "true",
                "planned" => m.planned = v == "true",
                "plan_path" => {
                    if !v.is_empty() {
                        m.plan_path = Some(v.to_string());
                    }
                }
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
pub(crate) struct Settings {
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
    /// config.review.peers: local review harnesses. Identity-free entries form
    /// one composite, head-pinned local-attestation gate; entries with a shared
    /// or per-entry identity retain the legacy GitHub-login gate.
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
    /// config.review.self_review_required (default true): floor the
    /// harness-resolved self-review reviewer onto `reviewers` when a code
    /// payload would otherwise ship unreviewed on a stock install. None means
    /// absent, normalized to true (the obligation defaults ON); `false` is the
    /// documented escape hatch.
    self_review_required: Option<bool>,
    /// config.review.nudge (x-b167): per-login overrides for the bot-review
    /// nudge, resolved against BOT_PROFILES by `resolved_nudge_configs`. Empty =
    /// no overrides (the built-in profiles alone decide nudgeability). A
    /// malformed entry degrades that login to non-nudgeable, never panics (AC8).
    nudge_overrides: Vec<NudgeOverride>,
    /// Top-level `done_probes` (x-a534): the repo-wide probe list, evaluated
    /// alongside the plan's own. The file is FLAT, so this reads off the TOML
    /// root, not out of a `config` table.
    ///
    /// None = key absent (no project gate). Some(Ok(list)) = the declaration.
    /// Some(Err(why)) = present but not an array of strings, which maps to the
    /// plan side's `Unparseable` and BLOCKS - a config key that degrades to
    /// no-gate is a guardrail that disappears when you typo it.
    done_probes: Option<Result<Vec<String>, String>>,
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
/// evidence ever clears it (codex peer review P1).
const MALFORMED_REVIEWERS_SENTINEL: &str = "\u{0}malformed-reviewers";

/// A `config.review.peers` entry. `provider` is kept for messaging and the
/// same-model guard; `model` carries an optional `"route_provider,route_model"`
/// route (the claude CLI as transport for a genuinely different model); the gate
/// identity selects the legacy posting carrier; otherwise the entry contributes
/// to the composite local-attestation gate.
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

/// One `[review.nudge]` per-login override (x-b167). Every field is optional in
/// TOML; a value of the wrong type sets `malformed` so that login degrades to
/// non-nudgeable rather than panicking - the stop gate must never panic (AC8).
#[derive(Debug, Clone, Default)]
struct NudgeOverride {
    login: String,
    review_handle: Option<String>,
    wait_minutes: Option<i64>,
    ceiling: Option<usize>,
    /// Defaults to true; `enabled = false` opts a repo out (back to plain
    /// block-and-wait, NOT a faster give-up).
    enabled: bool,
    /// Any field of the wrong type: the whole login drops to non-nudgeable.
    malformed: bool,
}

/// Parse the `[review.nudge]` table (`login -> { review_handle, wait_minutes,
/// ceiling, enabled }`). Lenient by construction, matching `value_as_login_list`:
/// a non-table value, or any field of the wrong type / a non-positive integer,
/// marks that login `malformed`. Never panics (AC8).
fn value_as_nudge_overrides(v: &toml::Value) -> Vec<NudgeOverride> {
    let Some(table) = v.as_table() else {
        // The whole `nudge` value is not a table (scalar/list): no overrides.
        return Vec::new();
    };
    let mut out = Vec::new();
    for (login, entry) in table {
        let mut ov = NudgeOverride {
            login: login.clone(),
            enabled: true,
            ..Default::default()
        };
        let Some(map) = entry.as_table() else {
            // A scalar or list where an inline table was expected (AC8).
            ov.malformed = true;
            out.push(ov);
            continue;
        };
        if let Some(rh) = map.get("review_handle") {
            match rh.as_str() {
                Some(s) => ov.review_handle = Some(s.to_string()),
                None => ov.malformed = true,
            }
        }
        if let Some(wm) = map.get("wait_minutes") {
            match wm.as_integer() {
                // Upper-bounded so `chrono::Duration::minutes` (which panics
                // above i64::MAX/60) can never take the stop gate down on an
                // absurd config value; anything out of range is malformed.
                Some(n) if (1..=MAX_NUDGE_WAIT_MINUTES).contains(&n) => ov.wait_minutes = Some(n),
                _ => ov.malformed = true, // non-int, non-positive, or absurd (AC8)
            }
        }
        if let Some(c) = map.get("ceiling") {
            match c.as_integer() {
                Some(n) if (1..=MAX_NUDGE_CEILING).contains(&n) => ov.ceiling = Some(n as usize),
                _ => ov.malformed = true,
            }
        }
        if let Some(en) = map.get("enabled") {
            match en.as_bool() {
                Some(b) => ov.enabled = b,
                None => ov.malformed = true,
            }
        }
        out.push(ov);
    }
    out
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

/// Classify a top-level `done_probes` value as a probe list or a reason it is
/// unreadable. An empty array is a legitimate "no project probes"; a mapping,
/// a scalar, or an array holding a non-string is NOT - it is a mis-declared
/// gate, and the Err travels to the gate so it blocks with a reason instead of
/// silently reading as no declaration at all.
fn value_as_probe_list(v: &toml::Value) -> Result<Vec<String>, String> {
    let items = v
        .as_array()
        .ok_or_else(|| format!("it is a {}, not an array of strings", v.type_str()))?;
    items
        .iter()
        .map(|i| {
            i.as_str().map(str::to_string).ok_or_else(|| {
                format!(
                    "it holds a {} where a command string was expected",
                    i.type_str()
                )
            })
        })
        .collect()
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

    // Top-level flat `done_probes` (x-a534). Presence is recorded even when the
    // value is junk: the Err arm blocks downstream rather than degrading to
    // "no probes declared".
    if let Some(v) = root.get("done_probes") {
        s.done_probes = Some(value_as_probe_list(v));
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
        if let Some(v) = review.get("self_review_required") {
            // A malformed value stays None -> normalized to true (obligation on,
            // fail-closed); only an explicit bool reaches the field.
            s.self_review_required = v.as_bool();
        }
        if let Some(v) = review.get("nudge") {
            s.nudge_overrides = value_as_nudge_overrides(v);
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
    Aborted {
        reason: String,
    },
    /// Agent-declared async watch (x-e2c8): it has armed a harness-tracked
    /// watcher and wants the session to idle until that watcher fires rather
    /// than re-blocking every stop tick. All attributes are advisory (used for
    /// the event and the lease math), never load-bearing: external truth
    /// decides whether idling is actually allowed.
    Watching {
        reason: String,
        pr: Option<String>,
        timeout: Option<String>,
    },
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

/// Detect intent with proper attribute extraction. Precedence within one
/// message: aborted > watching > promise (x-e2c8). aborted is the hardest stop;
/// watching outranks promise so a session that both promises and asks to idle
/// idles (its promise is re-evaluated on the next wake).
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
    if let Some(w_start) = text.find("<watching") {
        if let Some(gt) = text[w_start..].find('>') {
            let tag_text = &text[w_start..w_start + gt + 1];
            return Intent::Watching {
                reason: parse_xml_attr(tag_text, "reason").unwrap_or_default(),
                pr: parse_xml_attr(tag_text, "pr"),
                timeout: parse_xml_attr(tag_text, "timeout"),
            };
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
    // `watching` is honored ONLY from the single newest assistant entry
    // (x-e2c8): a stale watch-request from earlier work must not idle a session
    // that has since moved on. `promise`/`aborted` keep their bounded lookback.
    let mut newest_entry = true;
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
            // A watching tag below the newest entry is stale: skip it (counts
            // as a scanned entry) and keep scanning for a promise/aborted.
            Intent::Watching { .. } if !newest_entry => {
                scanned += 1;
                if scanned >= INTENT_LOOKBACK_ENTRIES {
                    return Intent::None;
                }
            }
            tagged => return tagged,
        }
        newest_entry = false;
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
    /// Per-missing-bot nudge classification for this fire (x-b167), same order
    /// as `missing_bots`. Empty when the review reads were skipped or there is no
    /// PR. `missing_bots` stays the gate; this only changes idling and messaging.
    /// An EMPTY list with a non-empty `missing_bots` means "not classified" and
    /// is treated exactly like today (every missing bot idlable, today's string).
    bot_nudges: Vec<BotNudge>,
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
    /// Configured `config.review.reviewers` with no head-pinned attestation.
    /// The sole failing term whenever the login gate is vacuous, and the reason
    /// the block message can name real local work instead of an absent bot.
    unattested_reviewers: Vec<UnattestedReviewer>,
    /// Unparseable events.jsonl lines that carry the literal
    /// `review_attestation`. Named in the reason so a corrupt attestation is
    /// not silently dropped. Not exhaustive by construction: a write torn
    /// before that token cannot be recognized at all.
    malformed_attestations: usize,
    /// Review coverage: did anyone actually review, distinct from `reviewed`
    /// (did anyone object). Computed at read time from observed evidence
    /// across two producer axes (github_app review objects; local_attestation
    /// head-pinned passes). Terminal selection consumes this: a run that would
    /// report `DonePRGreen` at coverage 0/Unknown reports `DoneUnreviewed`
    /// instead (x-0eaf). Never cached, never inferred from `reviewed`.
    coverage: CoverageReport,
}

/// The non-interactive invocation that satisfies each local reviewer, mirroring
/// the `invocation` field of `_RESOLVABLE_REVIEWERS` in
/// `cli/src/fno/config/__init__.py`. A block message that names a reviewer
/// without naming how to run it is only half a remedy.
///
/// Two languages, one table: kept honest by
/// `scripts/ci/check-reviewer-descriptor-parity.sh`, not by a comment asking a
/// human to remember.
/// The fourth element encodes per-harness verb overrides as
/// `"harness=verb;harness=verb"`, empty when the scalar invocation is the only
/// rendering. The self-review verb is the one case: `/code-review` on claude,
/// `/review` bare on codex. The codex value must stay bare - prose after the
/// verb flips codex to a no-merge-base review target - so a no-whitespace
/// check on the codex value is a unit test, not a convention. Kept honest
/// against the Python descriptor's `invocations` map by
/// check-reviewer-descriptor-parity.sh.
const REVIEWER_INVOCATIONS: &[(&str, &str, bool, &str)] = &[
    ("sigma", "/fno:review sigma", false, ""),
    (
        "code-review",
        "/code-review",
        false,
        "claude=/code-review;codex=/review",
    ),
    ("declare", "/fno:review declare", true, ""),
];

/// `(invocation, is_self_cert, per_harness)`. The flag mirrors the Python
/// descriptor's `asserts` field: a surface that names `declare` without saying
/// it asserts nothing invites an operator to clear the gate with no review
/// behind it.
fn reviewer_entry(name: &str) -> Option<(&'static str, bool, &'static str)> {
    REVIEWER_INVOCATIONS
        .iter()
        .find(|(n, _, _, _)| *n == name)
        .map(|(_, inv, self_cert, per)| (*inv, *self_cert, *per))
}

/// The harness-correct verb. Falls back to the scalar default when the harness
/// is unknown or the reviewer declares no override. `harness` is the author
/// harness from `claims::resolve_harness`, threaded rather than re-read so a
/// unit test can pin a harness without touching the environment.
fn reviewer_invocation_for(name: &str, harness: Option<&str>) -> Option<(&'static str, bool)> {
    let (inv, sc, per) = reviewer_entry(name)?;
    if per.is_empty() {
        return Some((inv, sc));
    }
    if let Some(h) = harness {
        for pair in per.split(';') {
            if let Some((ph, pv)) = pair.split_once('=') {
                if ph == h {
                    return Some((pv, sc));
                }
            }
        }
    }
    Some((inv, sc))
}

/// Documentation is `*.md` anywhere and anything under `docs/`. Plan files are
/// markdown, so the `.md` rule covers them; the `internal/` vault is gitignored
/// and never appears in a diff. A config file, a lockfile, and a shell script
/// all count as code.
fn is_documentation_path(path: &str) -> bool {
    // A single leading "./" is stripped once; trim_start_matches would also strip
    // a char set and lstrip a literal-repeated run, diverging from the Python
    // mirror (and mangling ".github"). The two classifiers must agree exactly.
    let trimmed = path.trim();
    let p = trimmed.strip_prefix("./").unwrap_or(trimmed);
    if p.is_empty() {
        return false;
    }
    p.ends_with(".md") || p.starts_with("docs/")
}

/// Whether the author harness has a self-review verb (claude `/code-review`,
/// codex `/review`). The self-review floor only applies on these: gemini/agy/
/// opencode have no native review verb, so flooring code-review would demand an
/// attestation nothing produces and wedge the loop. Their path is route 3 (a
/// spawned reviewer), which is deferred. Pure so a unit test pins the set.
fn harness_can_self_review(harness: Option<&str>) -> bool {
    matches!(harness, Some("claude") | Some("codex"))
}

/// Pure payload classifier: CODE iff any changed path is not documentation.
/// An empty diff is NOT a code payload (no ship, so no gate). Pure over a path
/// slice so unit tests need no git; the git-caller wrapper is `classify_payload`.
fn payload_is_code(paths: &[String]) -> bool {
    paths.iter().any(|p| !is_documentation_path(p))
}

/// The self-review reviewer to floor onto the required set, or None. Pure so a
/// unit test can pin the floor without git: a code payload on a lane-less stock
/// install gets `code-review`; a configured lane, an opt-out, a docs payload,
/// and a lane that already names code-review all get None. Returning the name
/// (not a bool) keeps "should floor" and "what to floor" in one place - the
/// reviewer name is the gate input, and splitting them invites drift.
fn floor_self_review(
    required_reviewers: &[String],
    lane_configured: bool,
    is_code: bool,
    self_review_required: bool,
) -> Option<String> {
    if !self_review_required || !is_code || lane_configured {
        return None;
    }
    let already = required_reviewers
        .iter()
        .any(|r| r.trim_start_matches('/') == "code-review");
    if already {
        return None;
    }
    Some("code-review".to_string())
}

/// `(is_code, assumed)`: classifies the branch's payload, failing CLOSED. An
/// unreadable diff (no `origin/main`, git missing, any non-zero exit) classifies
/// as code with `assumed=true`, so a degraded probe can never silently disable
/// the obligation the way failing open would. `origin/main...HEAD` is the
/// three-dot diff, so it names the branch's own changes (the PR diff), not
/// changes that landed on main since the branch point.
fn classify_payload(git_bin: &str, cwd: &Path) -> (bool, bool) {
    let out = Command::new(git_bin)
        .args(["diff", "--name-only", "origin/main...HEAD"])
        .current_dir(cwd)
        .output();
    let (paths, readable) = match out {
        Ok(o) if o.status.success() => {
            let paths = String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>();
            (paths, true)
        }
        _ => (Vec::new(), false),
    };
    if !readable {
        return (true, true);
    }
    (payload_is_code(&paths), false)
}

// ── review freshness: one predicate, both producers (x-5b99 / x-62a1) ─────────
//
// Freshness used to be decided TWICE with two different rules: a `github_app`
// verdict got none at all (a bot opinion was inherited across commits it never
// read), while a `local_attestation` got a bare sha equality so strict that
// addressing a review destroyed the proof the review happened. One design,
// failing opposite ways on its two producers. `review_freshness` is the single
// rule both now go through.

/// Whether a review verdict still describes the code at HEAD.
///
/// The two `Carried` variants are the reason a carry was granted, recorded on
/// the event so a carry is auditable and can never be mistaken for a fresh
/// read. Only `Stale` stops a verdict counting toward coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    /// The reviewer read this exact commit.
    Fresh,
    /// The PR's own code delta is byte-identical; any tree difference came from
    /// the base moving under it. A rebase is this shape, which is what makes
    /// the mandatory pre-merge rebase stop destroying attestations.
    CarriedBaseSync,
    /// Only documentation paths changed between the reviewed commit and HEAD.
    CarriedDocsOnly,
    /// Everything else, including every failure path.
    Stale,
}

impl Freshness {
    /// Whether a verdict at this freshness counts toward coverage.
    pub fn counts(&self) -> bool {
        !matches!(self, Freshness::Stale)
    }
}

/// Pre-computed git facts for one `(reviewed_sha, head_sha)` pair, so
/// [`review_freshness`] is pure and unit-tests with no git and no repository.
#[derive(Debug, Clone, Default)]
pub struct FreshnessFacts {
    /// PR code-diff identity at the reviewed commit (see
    /// [`pr_code_diff_identity`]).
    pub reviewed_identity: Option<String>,
    /// The same identity at HEAD.
    pub head_identity: Option<String>,
    /// Paths differing between the two TREES (two-dot). `None` on git failure.
    pub tree_paths: Option<Vec<String>>,
}

/// The one freshness rule. Pure over pre-computed facts.
///
/// `Carried` requires a POSITIVE identity match between two successfully
/// computed identities. Two `None`s never match, and neither does an empty
/// result: matching an absence against an absence is what produced this plan's
/// first (wrong) 63% carry-forward measurement, where every merged PR's
/// three-dot diff against current `origin/main` was empty and `e3b0c442` - the
/// hash of the empty string - compared equal to itself twelve times. The real
/// figure was 2 of 22. Every failure path lands on `Stale`; there is no input
/// on which a failure produces a carry.
pub fn review_freshness(reviewed_sha: &str, head_sha: &str, facts: &FreshnessFacts) -> Freshness {
    // No pinned commit is not evidence of freshness. An absent `commit.oid`, an
    // attestation with no `head_sha`, and an unresolvable HEAD all land here.
    if reviewed_sha.is_empty() || head_sha.is_empty() {
        return Freshness::Stale;
    }
    if reviewed_sha == head_sha {
        return Freshness::Fresh;
    }
    let (Some(reviewed), Some(head)) = (
        facts.reviewed_identity.as_deref(),
        facts.head_identity.as_deref(),
    ) else {
        return Freshness::Stale;
    };
    if reviewed != head {
        return Freshness::Stale;
    }
    // The identities match, so the code under review is unchanged. The tree
    // diff only names WHY, and a carry that cannot name its reason is not
    // auditable - so an unreadable tree diff is Stale like any other failure.
    let Some(paths) = facts.tree_paths.as_deref() else {
        return Freshness::Stale;
    };
    if !paths.is_empty() && paths.iter().all(|p| is_documentation_path(p)) {
        Freshness::CarriedDocsOnly
    } else {
        Freshness::CarriedBaseSync
    }
}

/// The path from a `git diff --raw` line (`:<meta>\t<path>`), or `""`.
/// `--no-renames` guarantees one path per line, so there is no second field.
fn raw_diff_line_path(line: &str) -> &str {
    line.split('\t').nth(1).unwrap_or("").trim()
}

/// Content identity of the PR's own CODE changes at `sha`: the three-dot diff
/// from `merge-base(base, sha)`, documentation paths dropped, hashed.
///
/// `--raw --no-abbrev` emits one line per changed path carrying both blob
/// SHAs, so the identity is content-exact without materializing a patch.
/// `--no-renames` pins it against a per-user `diff.renames` config that would
/// otherwise make two runs of the same comparison disagree.
///
/// `None` on any git failure AND when nothing outside documentation changed.
/// An empty code diff is not positive evidence of anything, and letting two of
/// them compare equal is the absence-matched-against-absence trap above. The
/// cost is that a documentation-only PR never carries an attestation, which is
/// the fail-closed direction and matches today's behavior exactly.
fn pr_code_diff_identity(git_bin: &str, cwd: &Path, base: &str, sha: &str) -> Option<String> {
    let out = Command::new(git_bin)
        .args([
            "diff",
            "--raw",
            "--no-abbrev",
            "--no-renames",
            &format!("{base}...{sha}"),
        ])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty() && !is_documentation_path(raw_diff_line_path(l)))
        .collect();
    if lines.is_empty() {
        return None;
    }
    lines.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    for line in &lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    Some(hasher.finalize().to_hex().to_string())
}

/// Paths differing between two TREES (two-dot), or `None` on git failure.
fn git_tree_paths(git_bin: &str, cwd: &Path, a: &str, b: &str) -> Option<Vec<String>> {
    let out = Command::new(git_bin)
        .args(["diff", "--name-only", "--no-renames", a, b])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

/// Resolves `reviewed_sha -> Freshness` against one HEAD, memoized so N
/// verdicts at one commit cost one pair of git calls rather than N.
///
/// The HEAD identity is computed once, on first use: a session whose reviewers
/// are all fresh (the common case) pays no git at all.
pub struct FreshnessResolver<'a> {
    git_bin: &'a str,
    cwd: &'a Path,
    /// The ref the PR merges into, already qualified (`origin/main`). An
    /// unresolvable base yields no identity, hence `Stale` - fail closed.
    base_ref: String,
    head_sha: String,
    head_identity: std::cell::RefCell<Option<Option<String>>>,
    cache: std::cell::RefCell<std::collections::HashMap<String, Freshness>>,
}

impl<'a> FreshnessResolver<'a> {
    pub fn new(git_bin: &'a str, cwd: &'a Path, base_ref: &str, head_sha: &str) -> Self {
        let base = base_ref.trim();
        Self {
            git_bin,
            cwd,
            // `gh pr view` returns a BARE branch name, and a branch name may
            // itself contain a slash (`release/2.0`), so "has a slash" does not
            // mean "already remote-qualified" - it only means the caller may
            // have passed one of ours. Test the `origin/` prefix instead: a
            // bare `release/2.0` resolves to a local ref that a fresh worktree
            // usually does not have, and the identity then fails to compute for
            // every commit, silently taking the carry away on exactly the
            // long-lived release branches that rebase most.
            base_ref: if base.is_empty() {
                "origin/main".to_string()
            } else if base.starts_with("origin/") {
                base.to_string()
            } else {
                format!("origin/{base}")
            },
            head_sha: head_sha.to_string(),
            head_identity: std::cell::RefCell::new(None),
            cache: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    fn head_identity(&self) -> Option<String> {
        let mut slot = self.head_identity.borrow_mut();
        slot.get_or_insert_with(|| {
            pr_code_diff_identity(self.git_bin, self.cwd, &self.base_ref, &self.head_sha)
        })
        .clone()
    }

    /// Freshness of a verdict recorded at `reviewed_sha`. Never panics, never
    /// fails: every unreadable input resolves to `Stale`.
    pub fn freshness(&self, reviewed_sha: &str) -> Freshness {
        if reviewed_sha.is_empty() {
            return Freshness::Stale;
        }
        if reviewed_sha == self.head_sha {
            return Freshness::Fresh;
        }
        if let Some(hit) = self.cache.borrow().get(reviewed_sha) {
            return *hit;
        }
        let facts = FreshnessFacts {
            reviewed_identity: pr_code_diff_identity(
                self.git_bin,
                self.cwd,
                &self.base_ref,
                reviewed_sha,
            ),
            head_identity: self.head_identity(),
            tree_paths: git_tree_paths(self.git_bin, self.cwd, reviewed_sha, &self.head_sha),
        };
        let verdict = review_freshness(reviewed_sha, &self.head_sha, &facts);
        self.cache
            .borrow_mut()
            .insert(reviewed_sha.to_string(), verdict);
        verdict
    }
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

/// The GraphQL bucket's state, from `gh api rate_limit`.
///
/// That endpoint is REST and primary-exempt, so the probe is free even while
/// GraphQL sits at 0 - which is its whole job: it distinguishes "the call
/// cannot succeed for N minutes" from "gh blipped", the two outcomes a bare
/// read failure conflates. None on any failure: a failed probe must never
/// fabricate an exhaustion verdict (a false "resets in 40m" would stall a
/// healthy session for no reason).
struct GraphqlQuota {
    remaining: i64,
    reset_epoch: i64,
}

/// Below this GraphQL remaining count, a no-promise fire stands down entirely:
/// the last of the budget belongs to the operation that
/// ships. Code default, named in the PR body - never the operator's config.
const GRAPHQL_FLOOR: i64 = 200;

fn probe_graphql_quota(gh_bin: &str, cwd: &Path) -> Option<GraphqlQuota> {
    let out = Command::new(gh_bin)
        .args(["api", "rate_limit"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let g = v.get("resources")?.get("graphql")?;
    Some(GraphqlQuota {
        remaining: g.get("remaining").and_then(|x| x.as_i64())?,
        reset_epoch: g.get("reset").and_then(|x| x.as_i64())?,
    })
}

/// The self-teaching exhaustion message. A session that reads it must stop
/// retrying the GraphQL reads this window and know where the answer still
/// lives - anything less and it burns a fire every tick on a call that
/// cannot succeed until the reset.
fn graphql_exhausted_reason(q: &GraphqlQuota) -> String {
    let now = Utc::now().timestamp();
    let mins = ((q.reset_epoch - now) / 60).max(0);
    format!(
        "GraphQL quota exhausted ({} remaining, resets in ~{}m). `gh pr view` / \
         `gh pr checks` cannot succeed until the reset: stop retrying them this \
         window. `fno pr status <n>` still answers on the REST budget.",
        q.remaining, mins
    )
}

/// A configured local reviewer with no head-pinned `pass` attestation.
#[derive(Debug, Clone, PartialEq)]
struct UnattestedReviewer {
    name: String,
    /// A head this reviewer DID attest at, which is no longer HEAD. Always a
    /// PASS and never empty - normalized at construction so `Some` means
    /// "there is a real prior pass to name", not "check is_empty() first".
    /// Without it the block message reads as "you never ran sigma" to a session
    /// that ran sigma and then pushed a commit, losing turns twice.
    superseded_head: Option<String>,
    /// This reviewer DID attest at the current head, and the verdict was not
    /// `pass`. "No attestation exists" would be a lie to a session that ran the
    /// reviewer and was told no.
    failed_at_head: bool,
}

/// A question THIS session asked, that was closed WITH an answer, and for
/// which no `operator_decision` event exists on any reachable journal. The
/// stop gate holds the session until the decision is
/// recorded, because a ruling that dies with the transcript is the failure
/// the decision record exists to prevent.
pub(crate) struct UnrecordedDecision {
    pub(crate) question_id: String,
    pub(crate) question: String,
}

/// Fold the question/decision family across a UNION of journals.
///
/// A question can be asked and closed in one journal while the decision lands
/// in another (the operator verbs write to the canonical root's journal; a
/// worktree stop gate reads its own cwd's), so membership is only decidable
/// after every journal is folded - checking per-file would hold a session
/// whose record sits one path away.
///
/// An unreadable or absent journal contributes nothing (fail open): this gate
/// scans for an OBLIGATION contracted elsewhere, and a missing journal means
/// no obligation is visible, not that one was breached. The substring
/// prefilter mirrors the Python reader: the journals are shared, append-only,
/// and never rotated, so parsing every line costs more than the scan.
fn scan_unrecorded_decisions(
    journals: &[std::path::PathBuf],
    session_id: &str,
) -> Vec<UnrecordedDecision> {
    let mut asked: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut closed_with_answer: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut recorded: std::collections::HashSet<String> = std::collections::HashSet::new();

    for path in journals {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in content.lines() {
            if !(line.contains("operator_question") || line.contains("operator_decision")) {
                continue;
            }
            let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let kind = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let data = val
                .get("data")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            match kind {
                "operator_question" => {
                    if data.get("session_id").and_then(|v| v.as_str()) == Some(session_id) {
                        if let Some(qid) = data.get("question_id").and_then(|v| v.as_str()) {
                            asked.insert(
                                qid.to_string(),
                                data.get("question")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .chars()
                                    .take(80)
                                    .collect(),
                            );
                        }
                    }
                }
                "operator_question_closed" => {
                    let answered = data
                        .get("answer")
                        .and_then(|v| v.as_str())
                        .map(|a| !a.trim().is_empty())
                        .unwrap_or(false);
                    if answered {
                        if let Some(qid) = data.get("question_id").and_then(|v| v.as_str()) {
                            closed_with_answer.insert(qid.to_string());
                        }
                    }
                }
                "operator_decision" => {
                    if let Some(qid) = data.get("question_id").and_then(|v| v.as_str()) {
                        recorded.insert(qid.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    let mut out: Vec<UnrecordedDecision> = asked
        .into_iter()
        .filter(|(qid, _)| closed_with_answer.contains(qid) && !recorded.contains(qid))
        .map(|(question_id, question)| UnrecordedDecision {
            question_id,
            question,
        })
        .collect();
    out.sort_by(|a, b| a.question_id.cmp(&b.question_id));
    out
}

/// The `config.review.reviewers` entries NOT satisfied by a head-pinned
/// `review_attestation` event (x-e703 Phase 2; list form added by x-cdc7). A
/// reviewer is satisfied when events.jsonl carries a line with
/// `type == "review_attestation"`, `data.reviewer` matching (leading '/'
/// stripped on both sides), `data.head_sha == head_sha`, and
/// `data.verdict == "pass"`.
///
/// The gate reads `.is_empty()` and the block message reads the names, so the
/// decision and the explanation come from ONE scan. When they came from two,
/// the message told sessions to wait on a bot that was never required.
///
/// Fail closed everywhere: an empty/unreadable events file, a stale head_sha
/// (attestation for a prior commit), or a `fail` verdict leaves the reviewer
/// UNSATISFIED, mirroring how a missing bot review holds the login gate. An
/// empty reviewer list is vacuously satisfied (no reviewers gate).
/// `unattested_reviewers` plus the count of unparseable lines that LOOK like
/// attestations. A torn write leaves a corrupt `review_attestation` in the file
/// and the gate then reports "no head-pinned review_attestation", which is the
/// same class of lie this node exists to delete - so the count is surfaced in
/// the reason. Mirrors `open_review_findings`, which already does this for
/// `review_finding`.
fn unattested_reviewers_scan(
    events_path: &Path,
    reviewers: &[String],
    freshness: &dyn Fn(&str) -> Freshness,
) -> (Vec<UnattestedReviewer>, usize) {
    let unsatisfied_all = || -> Vec<UnattestedReviewer> {
        reviewers
            .iter()
            .map(|r| UnattestedReviewer {
                name: r.trim_start_matches('/').to_string(),
                superseded_head: None,
                failed_at_head: false,
            })
            .collect()
    };
    if reviewers.is_empty() {
        return (Vec::new(), 0);
    }
    let Ok(content) = std::fs::read_to_string(events_path) else {
        // no evidence file -> gate unmet (fail closed)
        return (unsatisfied_all(), 0);
    };
    let mut malformed = 0usize;
    // Single pass (gemini review): record the LATEST verdict per reviewer at the
    // current head. events.jsonl is append-ordered, so a later attestation
    // supersedes an earlier one for the same reviewer - a `fail` posted after a
    // `pass` must revoke it, and a re-run `pass` after a `fail` must restore it
    // (codex peer review P1: a later fail was previously ignored). A reviewer is
    // satisfied iff its latest head-pinned verdict is exactly `pass`. O(lines).
    let mut latest_pass: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    // reviewer -> every OLD head it attested at, in first-seen order, each
    // carrying that head's LATEST verdict. A single-entry "most recent pass"
    // map cannot survive a retraction: `pass A, pass B, fail B` overwrites A
    // with B and then drops B, reporting no prior pass while A is still a real
    // one (codex P2 on this PR). Multi-round review/fix cycles produce exactly
    // that sequence.
    let mut other_heads: std::collections::HashMap<String, Vec<(String, bool)>> =
        std::collections::HashMap::new();
    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            if line.contains("review_attestation") {
                malformed += 1;
            }
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("review_attestation") {
            continue;
        }
        let Some(r) = val.pointer("/data/reviewer").and_then(|v| v.as_str()) else {
            continue;
        };
        let r = r.trim_start_matches('/').to_string();
        // An event with no `head_sha` is not head-pinned evidence and is
        // skipped outright. Defaulting it to "" would make it MATCH a caller
        // whose own head_sha is "", turning unpinned data into a pass (codex
        // P1 on this PR).
        let Some(line_head) = val.pointer("/data/head_sha").and_then(|v| v.as_str()) else {
            continue;
        };
        let is_pass = val.pointer("/data/verdict").and_then(|v| v.as_str()) == Some("pass");
        // The SAME predicate the coverage axis uses, not a second head-equality
        // rule beside it. Leaving this one a bare equality would have made the
        // softening decorative: this is the scan that satisfies
        // `config.review.reviewers`, so a rebase that carried the coverage
        // count would still have killed the required `code-review` entry and
        // demanded the re-review the carry exists to prevent.
        if !freshness(line_head).counts() {
            // Empty is not a head; recording it would put a `Some` in the
            // message with nothing to print.
            if line_head.is_empty() {
                continue;
            }
            let seen = other_heads.entry(r).or_default();
            match seen.iter().position(|(h, _)| h == line_head) {
                Some(i) => seen[i].1 = is_pass, // latest verdict wins for that head
                None => seen.push((line_head.to_string(), is_pass)),
            }
            continue;
        }
        latest_pass.insert(r, is_pass);
    }
    let out = reviewers
        .iter()
        .map(|entry| entry.trim_start_matches('/'))
        .filter(|name| latest_pass.get(*name) != Some(&true))
        .map(|name| UnattestedReviewer {
            name: name.to_string(),
            // An old head whose LATEST verdict is still a pass. Heads keep
            // first-seen order, so a head re-attested later keeps its original
            // slot and this may name a slightly older one - both are real
            // passes, so the line stays true either way. Only a pass is worth
            // naming: an old-head `fail` rendered as "passed at X, superseded"
            // would imply a successful review that never happened.
            superseded_head: other_heads
                .get(name)
                .and_then(|heads| heads.iter().rev().find(|(_, ok)| *ok))
                .map(|(h, _)| h.clone()),
            failed_at_head: latest_pass.get(name) == Some(&false),
        })
        .collect();
    (out, malformed)
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
    git_bin: &str,
    cwd: &Path,
    ci_declared_none: bool,
    no_external: bool,
    required_bots: &[String],
    optional_bots: &[String],
    external_reviewers: &[String],
    reviewers: &[String],
    nudge_configs: &[NudgeConfig],
    head_sha: &str,
    events_path: &Path,
    global_events_path: &Path,
    repo_slug: &str,
    author_session: Option<&str>,
    pr_selector: Option<&str>,
) -> Result<PrInfo, (String, String)> {
    // An explicit PR selector for the branch-resolved gh calls (x-3a3f):
    // Some(n) inserts the number (`gh pr view <n>`, `gh pr checks <n>`) so the
    // standalone review-coverage verb can evaluate a PR from a checkout that is
    // NOT on its branch (`fno pr merge <n>` from canonical); None keeps the
    // argv byte-identical to the stop hook's branch-resolved form. The one
    // number-based call (`gh api .../pulls/<n>/comments`) already carries the
    // number the first read returned.
    let sel: Vec<&str> = match pr_selector {
        Some(n) => vec![n],
        None => vec![],
    };
    // Read 1: PR state + number + head OID + mergeability
    let pr_view_out = Command::new(gh_bin)
        .args(["pr", "view"])
        .args(&sel)
        .args([
            "--json",
            // baseRefName rides along for the freshness predicate's merge-base
            // (x-5b99). Same call, same round trip, no new API cost.
            "state,number,headRefName,headRefOid,mergeable,baseRefName",
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
                bot_nudges: Vec::new(),
                usage_limited: Vec::new(),
                unaddressed_findings: Vec::new(),
                review_skipped: false,
                unattested_reviewers: Vec::new(),
                malformed_attestations: 0,
                coverage: CoverageReport {
                    coverage: Coverage::Covered(0),
                    verdicts: Vec::new(),
                },
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

    // One freshness resolver for every reviewer on this PR (x-5b99 / x-62a1).
    // Both producers and both presence scans read it, so there is one rule
    // rather than the two divergent ones this replaces. Memoized per reviewed
    // sha, and the HEAD identity is computed lazily, so a PR whose reviewers
    // are all at HEAD (the common case) pays no git at all.
    let base_ref = pr_json
        .get("baseRefName")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let resolver = FreshnessResolver::new(git_bin, cwd, base_ref, head_sha);
    let freshness = |sha: &str| resolver.freshness(sha);

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
            bot_nudges: Vec::new(),
            usage_limited: Vec::new(),
            unaddressed_findings: Vec::new(),
            review_skipped: true,
            unattested_reviewers: Vec::new(),
            malformed_attestations: 0,
            coverage: CoverageReport {
                coverage: Coverage::Covered(0),
                verdicts: Vec::new(),
            },
        });
    }

    // Read 2: CI checks. Compute the conclusion, the full failing-check-name set,
    // AND whether any check is still pending from the same payload (the set feeds
    // the DoneAwaitingMerge subset rule; the pending flag gates that terminal so
    // it never fires on partial CI).
    let no_hosted_ci =
        crate::verify_evidence::hosted_ci_not_configured(ci_declared_none, cwd, head_sha);
    let (ci_conclusion, failing_checks, ci_has_pending) = if no_hosted_ci {
        (CiConclusion::Skipped, Vec::new(), false)
    } else {
        let checks_out = Command::new(gh_bin)
            .args(["pr", "checks"])
            .args(&sel)
            .args(["--json", "name,state,bucket"])
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
    // One scan feeds both the gate and its explanation, so the two cannot
    // disagree the way the decision and the message did on PR #618.
    let (unattested, malformed_attestations) =
        unattested_reviewers_scan(events_path, reviewers, &freshness);
    let reviewers_ok = unattested.is_empty();
    // Coverage reads the same events.jsonl as the attestation scan (its local
    // axis) plus the GitHub review arrays (its github_app axis). Read once;
    // a missing file is empty (the local axis then contributes nothing, which
    // is correct - no evidence of a local review).
    let events_text = std::fs::read_to_string(events_path).unwrap_or_default();
    let (
        latest_review_ts,
        reviewed,
        missing_bots,
        bot_nudges,
        usage_limited,
        unaddressed_findings,
        coverage,
    ) = if login_skipped {
        // No GitHub logins to poll (nothing configured, or no_external): skip
        // the gh review reads entirely (fewer calls + no spurious gh-error
        // block). The local attestation gate still applies - reviewers_ok is
        // true when unconfigured, so a login-only or no-gate config is
        // unaffected. Coverage's github axis is empty here (no logins read),
        // so coverage is the local axis alone - which is exactly how a
        // worker-run /code-review counts even on a no-required-bots config.
        let coverage = classify_coverage(
            &[],
            &[],
            &events_text,
            &[],
            false,
            author_session,
            &freshness,
        );
        (
            "none".to_string(),
            reviewers_ok,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            coverage,
        )
    } else {
        // Read 3: top-level reviews + issue comments
        let reviews_out = Command::new(gh_bin)
            .args(["pr", "view"])
            .args(&sel)
            .args(["--json", "reviews,comments"])
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
        let info = compute_review_info(&reviews_json, required_bots, &freshness);
        // Per-missing-bot nudge classification (x-b167), computed AFTER the
        // usage-limit retain (which happened inside compute_review_info) so
        // the two give-up paths never compose (AC6): a usage_limited bot is
        // already out of missing_bots and is never classified here. Derived
        // from the same issue-comment list, fresh every fire.
        let now = Utc::now();
        let review_comments = reviews_json
            .get("comments")
            .and_then(|v| v.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let bot_nudges: Vec<BotNudge> = info
            .missing_bots
            .iter()
            .map(|bot| {
                classify_bot_nudge(
                    bot,
                    review_comments,
                    nudge_config_for(nudge_configs, bot),
                    now,
                )
            })
            .collect();
        // The "empty bot_nudges = not classified = status quo" contract that
        // async_wait_class and build_block_reason rely on holds only because
        // this is an all-or-nothing map: bot_nudges is either empty or 1:1
        // with missing_bots. A future partial classification would silently
        // mis-idle, so pin the invariant here rather than let it drift.
        debug_assert_eq!(bot_nudges.len(), info.missing_bots.len());
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
                .args(["pr", "view"])
                .args(&sel)
                .args(["--json", "commits"])
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
        // Coverage's github_app axis: configured required + optional logins
        // (external_reviewers are local-attestation peers, not github
        // posters). Dedup so a login in both lists is one verdict.
        let reviews_arr: &[Value] = reviews_json
            .get("reviews")
            .and_then(|v| v.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let comments_arr: &[Value] = reviews_json
            .get("comments")
            .and_then(|v| v.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let mut gh_logins: Vec<String> = required_bots.to_vec();
        for b in optional_bots {
            if !gh_logins.iter().any(|x| x == b) {
                gh_logins.push(b.clone());
            }
        }
        // github_read_ok is true here: a failed gh read returned Err above.
        let coverage = classify_coverage(
            reviews_arr,
            comments_arr,
            &events_text,
            &gh_logins,
            true,
            author_session,
            &freshness,
        );
        (
            activity_ts,
            reviewed,
            info.missing_bots,
            bot_nudges,
            info.usage_limited,
            unaddressed,
            coverage,
        )
    };

    // Emit coverage every gate eval so the Python readers (the merge primitive
    // and the polling command) and audit see one coherent, fresh number rather
    // than recomputing it (the Ownership rule: loopcheck computes, Python
    // reads). Skipped for the no-PR early returns above.
    //
    // BOTH logs, like every other loop-check event. This one used to write only
    // the project log, and since the stop hook runs wherever the session runs,
    // that put the attestation in `<worktree>/.fno/events.jsonl` while a merge
    // run from canonical read `<canonical>/.fno/events.jsonl` - a satisfied
    // gate reading as an unsatisfiable one, silently, with a refusal that
    // named a count and not a location. The global log is the one file both
    // stand in; `repo` in the payload keeps it scoped (x-f43c).
    if number > 0 {
        emit_to_both(
            events_path,
            global_events_path,
            "review_coverage",
            coverage_event_data(number, &coverage, head_sha, repo_slug, author_session),
        );
    }

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
        bot_nudges,
        usage_limited,
        unaddressed_findings,
        // Telemetry only (no decision reads this): "no review gate of any kind
        // applied" = the login reads were skipped AND no local reviewers gate.
        // A reviewers-only config did gate, so it is NOT review_skipped.
        review_skipped: login_skipped && reviewers.is_empty(),
        unattested_reviewers: unattested,
        malformed_attestations,
        coverage,
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

/// True when a quota-bounced required bot is the ONLY unmet conjunct of
/// `reviewed`, which is what `DoneAwaitingReview` claims when it fires (x-9ab2).
///
/// `reviewed` is `all_required_passed() && unaddressed.is_empty() &&
/// reviewers_ok`, so reconstructing only `!usage_limited.is_empty()` fires the
/// terminal on a PR the agent still has work on: a bot that has not reviewed
/// YET is owed its nudge window (x-b167), a blocking finding is work to DO, and
/// `unattested_reviewers` is `reviewers_ok` in `PrInfo` terms (x-e703, a
/// configured local review that never ran). Any of them non-empty falls through
/// to the existing hold, which is the fail-closed direction.
fn awaiting_review_only(pr: &PrInfo) -> bool {
    !pr.usage_limited.is_empty()
        && pr.missing_bots.is_empty()
        && pr.unaddressed_findings.is_empty()
        && pr.unattested_reviewers.is_empty()
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

/// Post a bot's review trigger to the PR once, returning true on success (x-b167
/// section 5). `FNO_LOOPCHECK_NO_COMMENT=1` suppresses the post so the test suite
/// never comments on a real PR, mirroring `FNO_LOOPCHECK_NO_NOTIFY`.
///
/// Idempotency is the PR itself, not a counter: this fires only on a NeedsNudge
/// classification, which means zero qualifying mentions exist within the wait
/// window - the same read every participant makes. A sibling worktree, a
/// `/fno:pr check` cron, a human, and a restarted-after-compaction session all
/// see the same PR and reach the same decision, so there is nothing to double.
fn post_nudge_comment(gh_bin: &str, cwd: &Path, pr_number: i64, review_handle: &str) -> bool {
    if std::env::var("FNO_LOOPCHECK_NO_COMMENT").as_deref() == Ok("1") {
        return false;
    }
    Command::new(gh_bin)
        .args([
            "pr",
            "comment",
            &pr_number.to_string(),
            "--body",
            review_handle,
        ])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The first missing bot that has been nudged to its ceiling and gone silent, if
/// any. The NoProgress backstop names it instead of a bare fingerprint streak.
fn unresponsive_bot(pr: &PrInfo) -> Option<&BotNudge> {
    pr.bot_nudges
        .iter()
        .find(|n| n.class == NudgeClass::Unresponsive)
}

/// The give-up line for an unresponsive nudged bot (x-b167 AC13): the operator's
/// two questions ("will it finish, must I act") answered in one line.
fn nudge_giveup_message(n: &BotNudge) -> String {
    format!(
        "{} did not review after {} nudges over {}m; giving up (NoProgress). \
         Move it to config.review.optional_apps or review by hand.",
        n.login, n.nudges, n.span_min
    )
}

/// Per-bot knowledge, login-keyed: the ONE table the review-gate code reads for
/// "what is this bot and how do we reach it". Replaces the scattered `KNOWN_BOTS`
/// membership list and the `USAGE_LIMIT_MARKERS` body-string list.
///
/// One bot wears three names and they are NOT interchangeable at the three sites
/// that use them:
///   - `login`         the review author, what `login_matches_bot` compares against
///   - `review_handle` what a PR comment must CONTAIN to trigger a fresh review
///   - `reply_handle`  what an in-thread reply must ADDRESS to reach the bot
/// A `github-app` reviewer that reviews on mention (not on push) is `nudgeable`:
/// footnote may post its `review_handle` to un-stick a required gate that nobody
/// mentioned (x-b167). Nudge timing (`wait_minutes`, `ceiling`, `enabled`) is
/// config, not code - see `[review.nudge]` / `resolved_nudge_configs`.
struct BotProfile {
    login: &'static str,
    review_handle: &'static str,
    reply_handle: &'static str,
    /// ISSUE-comment body markers this bot posts when it is rate-limited and will
    /// never post a review object (PR #214). Empty for a bot never seen to do so.
    usage_markers: &'static [&'static str],
    nudgeable: bool,
}

/// The shipped bot table. `chatgpt-codex-connector` is characterized from PR #618
/// (mention-triggered, ~4-7m latency, 5/5 mentions answered); `gemini-code-assist`
/// stays `nudgeable: false` with an empty `review_handle` until its trigger is
/// characterized (Evidence Gaps), which is strictly more than the old lists knew.
const BOT_PROFILES: &[BotProfile] = &[
    BotProfile {
        login: "chatgpt-codex-connector",
        review_handle: "@codex review",
        reply_handle: "@chatgpt-codex-connector",
        usage_markers: &["usage limits for code reviews", "codex usage limits"],
        nudgeable: true,
    },
    BotProfile {
        login: "gemini-code-assist",
        review_handle: "",
        reply_handle: "@gemini-code-assist",
        usage_markers: &[],
        nudgeable: false,
    },
];

/// The profile for an actual review/comment AUTHOR login (may carry gh's `[bot]`
/// suffix or be the full login): the profile login is a substring of the author,
/// matching `login_matches_bot(author, profile.login)`. Used to reach a finding
/// author's `reply_handle` (x-b167 AC14).
fn profile_by_author(author: &str) -> Option<&'static BotProfile> {
    BOT_PROFILES
        .iter()
        .find(|p| login_matches_bot(author, p.login))
}

/// Two login strings name the same bot when either is a case-insensitive
/// substring of the other (so a config short name "codex", a full login, and a
/// "[bot]"-suffixed author all correspond). Symmetric superset of
/// `login_matches_bot`.
fn logins_correspond(a: &str, b: &str) -> bool {
    login_matches_bot(a, b) || login_matches_bot(b, a)
}

/// Default nudge cadence (x-b167). 15 minutes is the observed 6m55s worst-case
/// latency on PR #618 with headroom, not a guess; 3 nudges bounds the give-up at
/// ~45 minutes of *asked-for* waiting versus the unbounded budget burn today.
const DEFAULT_NUDGE_WAIT_MINUTES: i64 = 15;
const DEFAULT_NUDGE_CEILING: usize = 3;

/// Sanity ceilings for `[review.nudge]` override integers (x-b167). A value
/// beyond these is a typo, not a cadence: `wait_minutes` is bounded well under
/// `i64::MAX/60` so `chrono::Duration::minutes` can never overflow-panic in the
/// stop gate, and a nudge cadence past a week / 1000 asks is meaningless anyway.
const MAX_NUDGE_WAIT_MINUTES: i64 = 7 * 24 * 60; // one week
const MAX_NUDGE_CEILING: i64 = 1000;

/// A nudgeable bot login with its resolved cadence: BOT_PROFILES defaults
/// overlaid with `[review.nudge]` overrides. ONLY nudgeable logins appear here
/// (enabled, non-empty review_handle, not malformed); any other missing bot
/// classifies `NotNudgeable`.
#[derive(Debug, Clone)]
pub(crate) struct NudgeConfig {
    login: String,
    review_handle: String,
    wait_minutes: i64,
    ceiling: usize,
}

/// Resolve the nudgeable-bot set for this repo: the built-in profiles, then the
/// `[review.nudge]` overrides. A malformed or `enabled = false` override REMOVES
/// its login from the set (opting out is never opting into a faster give-up);
/// an override with no resolvable `review_handle` (neither its own nor a base
/// profile's) is likewise dropped, since there is nothing to post.
fn resolved_nudge_configs(settings: &Settings) -> Vec<NudgeConfig> {
    let mut out: Vec<NudgeConfig> = BOT_PROFILES
        .iter()
        .filter(|p| p.nudgeable && !p.review_handle.is_empty())
        .map(|p| NudgeConfig {
            login: p.login.to_string(),
            review_handle: p.review_handle.to_string(),
            wait_minutes: DEFAULT_NUDGE_WAIT_MINUTES,
            ceiling: DEFAULT_NUDGE_CEILING,
        })
        .collect();

    for ov in &settings.nudge_overrides {
        let base = out
            .iter()
            .find(|c| logins_correspond(&c.login, &ov.login))
            .cloned();
        // Drop first so an override always replaces (or removes) its login.
        out.retain(|c| !logins_correspond(&c.login, &ov.login));
        if ov.malformed || !ov.enabled {
            continue; // opt-out / bad entry -> non-nudgeable
        }
        let handle = ov
            .review_handle
            .clone()
            .or_else(|| base.as_ref().map(|b| b.review_handle.clone()))
            .filter(|h| !h.is_empty());
        let Some(review_handle) = handle else {
            continue; // no trigger to post -> not nudgeable
        };
        out.push(NudgeConfig {
            login: ov.login.clone(),
            review_handle,
            wait_minutes: ov
                .wait_minutes
                .or_else(|| base.as_ref().map(|b| b.wait_minutes))
                .unwrap_or(DEFAULT_NUDGE_WAIT_MINUTES),
            ceiling: ov
                .ceiling
                .or_else(|| base.as_ref().map(|b| b.ceiling))
                .unwrap_or(DEFAULT_NUDGE_CEILING),
        });
    }
    out
}

/// The nudge config for a configured missing-bot login, or None (non-nudgeable).
fn nudge_config_for<'a>(configs: &'a [NudgeConfig], bot: &str) -> Option<&'a NudgeConfig> {
    configs.iter().find(|c| logins_correspond(&c.login, bot))
}

/// A missing bot's nudge classification for this fire (x-b167). Derived fresh
/// from PR comments every fire - no durable counter - so a mention posted by a
/// human, `/fno:pr check`, or a sibling worktree counts identically and
/// self-heals across restart / compaction / handoff.
#[derive(Debug, Clone, PartialEq)]
enum NudgeClass {
    /// No mention within the wait window: work to DO (post the trigger). Never
    /// idlable.
    NeedsNudge,
    /// Newest mention still inside the wait window: a genuine async wait. The
    /// only idlable nudge state.
    Awaiting,
    /// Ceiling reached and the newest mention timed out: nobody will end this
    /// wait. Never idlable, so the NoProgress backstop reaps it.
    Unresponsive,
    /// Login footnote cannot nudge (no profile/override, disabled, or a peer
    /// sentinel): today's block-and-wait behavior, unchanged. Idlable (status
    /// quo).
    NotNudgeable,
}

/// One missing bot's classification plus the facts the block message renders.
#[derive(Debug, Clone)]
struct BotNudge {
    login: String,
    class: NudgeClass,
    /// The trigger to post; "" when NotNudgeable.
    review_handle: String,
    ceiling: usize,
    /// Mention count on the PR (every issue comment containing review_handle).
    nudges: usize,
    /// Minutes since the newest mention (0 when there is none).
    newest_age_min: i64,
    /// Minutes from the oldest mention to now (0 when there is none), for the
    /// "did not review after N nudges over Mm" give-up line.
    span_min: i64,
}

impl BotNudge {
    fn not_nudgeable(login: &str) -> Self {
        BotNudge {
            login: login.to_string(),
            class: NudgeClass::NotNudgeable,
            review_handle: String::new(),
            ceiling: 0,
            nudges: 0,
            newest_age_min: 0,
            span_min: 0,
        }
    }
}

/// Whether this state may idle on a `<watching>` tag: only a genuine async wait
/// (Awaiting) or a login we never nudge (NotNudgeable, status quo). NeedsNudge is
/// work to do; Unresponsive is a wait nobody ends.
fn nudge_class_idlable(class: &NudgeClass) -> bool {
    matches!(class, NudgeClass::Awaiting | NudgeClass::NotNudgeable)
}

/// Classify one missing bot against the PR's issue comments. A mention is every
/// issue comment whose body contains the trigger handle, author unrestricted (a
/// mention is a request from anyone; only a usage-limit *claim* is scoped to the
/// bot's own login). Reads NO review timestamp and NO `reviews[].commit`: the
/// bot gate is PR-lifetime, and touching either silently re-pins it to head.
fn classify_bot_nudge(
    login: &str,
    comments: &[Value],
    cfg: Option<&NudgeConfig>,
    now: DateTime<Utc>,
) -> BotNudge {
    let Some(cfg) = cfg else {
        return BotNudge::not_nudgeable(login);
    };
    if cfg.review_handle.is_empty() {
        return BotNudge::not_nudgeable(login);
    }
    let mut total = 0usize;
    let mut times: Vec<DateTime<Utc>> = Vec::new();
    for c in comments {
        let body = c.get("body").and_then(|v| v.as_str()).unwrap_or("");
        if !body.contains(&cfg.review_handle) {
            continue;
        }
        total += 1;
        // A malformed/missing createdAt must NOT push toward Unresponsive:
        // giving up on a parse error is not reversible, asking again is (AC-ERR).
        if let Some(dt) = c
            .get("createdAt")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        {
            times.push(dt);
        }
    }
    if total == 0 {
        return BotNudge {
            login: login.to_string(),
            class: NudgeClass::NeedsNudge,
            review_handle: cfg.review_handle.clone(),
            ceiling: cfg.ceiling,
            nudges: 0,
            newest_age_min: 0,
            span_min: 0,
        };
    }
    let (Some(newest), Some(oldest)) = (times.iter().max().copied(), times.iter().min().copied())
    else {
        // Mentions exist but none carried a usable timestamp: ask again (cheap).
        return BotNudge {
            login: login.to_string(),
            class: NudgeClass::NeedsNudge,
            review_handle: cfg.review_handle.clone(),
            ceiling: cfg.ceiling,
            nudges: total,
            newest_age_min: 0,
            span_min: 0,
        };
    };
    let newest_age_min = (now - newest).num_minutes().max(0);
    let span_min = (now - oldest).num_minutes().max(0);
    let class = if (now - newest) < chrono::Duration::minutes(cfg.wait_minutes) {
        NudgeClass::Awaiting
    } else if total >= cfg.ceiling {
        NudgeClass::Unresponsive
    } else {
        NudgeClass::NeedsNudge // previous mention timed out; ask again
    };
    BotNudge {
        login: login.to_string(),
        class,
        review_handle: cfg.review_handle.clone(),
        ceiling: cfg.ceiling,
        nudges: total,
        newest_age_min,
        span_min,
    }
}

/// Default must-have-reviewed list when config.review.github_apps is absent.
/// EMPTY for fresh installs: a clone with no review configuration completes on
/// PR + CI green without hanging on a review bot it has never set up (a fresh
/// `/target` otherwise runs to the budget cap waiting for a codex review that
/// never arrives). Maintainers who want an external-review gate pin it
/// explicitly via config.review.github_apps (e.g. ["chatgpt-codex-connector"]).
const DEFAULT_REQUIRED_BOTS: &[&str] = &[];

/// Stable reviewer key emitted by every identity-free peer. Multiple configured
/// peer harnesses are alternatives for one composite gate, not N required votes.
const LOCAL_PEER_REVIEWER: &str = "peer";

/// An unmatchable reviewer key used when every identity-free peer is the
/// author's own model family. It keeps the local gate fail-closed independently
/// of the producer and is rendered as an actionable same-model refusal.
const SAME_MODEL_LOCAL_PEER_SENTINEL: &str = "\u{0}fno-peer-same-model-local\u{0}";

/// A login no real GitHub account can equal, pushed when a required peer login is
/// backed ONLY by peers whose model is the author's own (same-model guard). It
/// REPLACES the clearable login so a same-model review can never satisfy the
/// cross-model gate.
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
/// the resolved posting identity of each identity-backed `peers` entry.
/// Identity-free peers are resolved separately into local reviewer evidence.
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

    // Only identity-backed peers contribute to the expected-login set. Shared
    // identity collapses to one login; per-peer identities each add their own.
    // Identity-free peers are not missing logins: they use local attestations.
    for peer in &settings.peers {
        let id = peer
            .identity
            .clone()
            .or_else(|| settings.peer_identity.clone());
        match id {
            Some(id) if !logins.iter().any(|l| l == &id) => logins.push(id),
            Some(_) => {} // already present (shared identity)
            None => {}    // local-attestation carrier
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

/// Resolve all identity-free peers into one local reviewer requirement.
///
/// Any cross-model option makes the composite gate satisfiable by a `peer`
/// attestation. When the author is known and every option is same-model, return
/// an unmatchable sentinel so even a forged `peer: pass` cannot self-review the
/// change. Unknown peer families remain eligible, matching the existing
/// identity-backed guard's conservative compatibility rule.
fn resolved_local_peer_reviewers_for_author(
    settings: &Settings,
    author_harness: Option<&str>,
) -> Vec<String> {
    if settings.peer_identity.is_some() {
        return Vec::new();
    }
    let local: Vec<&PeerEntry> = settings
        .peers
        .iter()
        .filter(|peer| peer.identity.is_none())
        .collect();
    if local.is_empty() {
        return Vec::new();
    }
    let Some(author_fam) = author_harness.and_then(harness_family) else {
        return vec![LOCAL_PEER_REVIEWER.to_string()];
    };
    if local
        .iter()
        .any(|peer| peer_family(peer) != Some(author_fam))
    {
        vec![LOCAL_PEER_REVIEWER.to_string()]
    } else {
        eprintln!(
            "loop-check: every identity-free peer is the author's own model - configure a cross-model peer or routed model"
        );
        vec![SAME_MODEL_LOCAL_PEER_SENTINEL.to_string()]
    }
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
pub(crate) fn login_matches_bot(login: &str, bot: &str) -> bool {
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
    // Default: endswith [bot] or a known profile login
    login.ends_with("[bot]") || BOT_PROFILES.iter().any(|p| login.contains(p.login))
}

/// Every usage-limit marker across all bot profiles, unioned. A rate-limited
/// review bot posts one of these as an ISSUE comment when it never posts a review
/// object (PR #214). Matched case-insensitively via `contains` against a
/// lowercased body, mirroring the pinned-string approach in `blocking_severity`.
/// Unioned rather than scoped per-login to stay byte-identical to the old flat
/// `USAGE_LIMIT_MARKERS` const it replaced.
///
/// The asymmetry here INVERTED with x-9ab2 and the marker list must be read in
/// the new direction: an under-match leaves the bot in `missing_bots`, which
/// blocks (safe, just slow), while an over-match now PARKS the PR at
/// `DoneAwaitingReview` with no automatic path back, rather than dropping the
/// bot and proceeding. Add a marker only for a string the bot posts when it
/// truly will not review; a phrase a real review could quote is not one.
pub(crate) fn body_is_usage_limit(body: &str) -> bool {
    BOT_PROFILES
        .iter()
        .flat_map(|p| p.usage_markers.iter())
        .any(|m| body.contains(m))
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
    /// Required bots that posted only a usage-limit (quota) comment, never a
    /// review. Such a bot has NOT reviewed, so `all_required_passed` is false
    /// while this is non-empty: the gate fails closed instead of merging
    /// unreviewed (x-9ab2). They are moved OUT of `missing_bots` (not scanned
    /// for nudges) because the agent cannot make a rate-limited bot recover, so
    /// a nudge/idle would wedge until budget death; the loop instead terminates
    /// cleanly via `DoneAwaitingReview`. Scoped to the bot's OWN author.login.
    usage_limited: Vec<String>,
}

impl ReviewInfo {
    /// Every required bot has at least one completed pass. A bot that posted
    /// only a usage-limit (quota) comment has NOT reviewed, so it counts as
    /// not-passed: a quota bounce must fail the gate closed instead of letting
    /// it proceed (x-9ab2). The caller still records the drop for telemetry.
    fn all_required_passed(&self) -> bool {
        self.missing_bots.is_empty() && self.usage_limited.is_empty()
    }
}

/// Newest review-or-comment timestamp on a PR, or `"none"`.
///
/// Split out of `compute_review_info` because one caller (the no-progress
/// activity probe) wants ONLY this. Freshness does not and must not affect an
/// activity timestamp - a stale review is still activity - and giving that
/// caller the full ReviewInfo meant handing it a fabricated freshness resolver
/// whose verdicts nobody reads. A function that cannot return the other fields
/// makes that structural instead of a comment somebody later disbelieves.
fn review_activity_ts(reviews_json: &Value) -> String {
    let mut latest = String::new();
    for (key, field) in [("reviews", "submittedAt"), ("comments", "createdAt")] {
        for item in reviews_json
            .get(key)
            .and_then(|v| v.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[])
        {
            let ts = item.get(field).and_then(|v| v.as_str()).unwrap_or("");
            if !ts.is_empty() && ts > latest.as_str() {
                latest = ts.to_string();
            }
        }
    }
    if latest.is_empty() {
        "none".to_string()
    } else {
        latest
    }
}

fn compute_review_info(
    reviews_json: &Value,
    required_bots: &[String],
    freshness: &dyn Fn(&str) -> Freshness,
) -> ReviewInfo {
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

    let final_ts = review_activity_ts(reviews_json);
    let mut passed: Vec<bool> = vec![false; required_bots.len()];

    for r in reviews {
        let login = r
            .pointer("/author/login")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let state = r.get("state").and_then(|v| v.as_str()).unwrap_or("");

        // A required bot's PRESENCE is the same question the coverage axis
        // asks, so it goes through the same predicate. Without this the
        // tightening is decorative on exactly the path that gates a merge: a
        // required bot whose only verdict sits on a commit it read twelve
        // hours and two commits ago would still satisfy `reviewed`, and a
        // fresh local attestation beside it would carry the whole gate. A bot
        // that goes stale returns to `missing_bots`, where the existing nudge
        // path asks it to re-read - which is the correct response to "reviewed
        // an older commit", and a different one from "has not reviewed".
        if !state.is_empty()
            && freshness(
                r.pointer("/commit/oid")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            )
            .counts()
        {
            for (i, bot) in required_bots.iter().enumerate() {
                if login_matches_bot(login, bot) {
                    passed[i] = true;
                }
            }
        }
    }

    let mut missing_bots: Vec<String> = required_bots
        .iter()
        .zip(passed.iter())
        .filter(|(_, ok)| !**ok)
        .map(|(bot, _)| bot.clone())
        .collect();

    // (a) Usage-limit detection. A still-missing required bot that authored a
    // comment carrying a pinned usage-limit marker is env-blocked, not
    // hasn't-reviewed-yet: it will never post a review. Move it OUT of
    // missing_bots into usage_limited so it is not nudged/idled-on (the agent
    // cannot make a rate-limited bot recover, so a nudge would wedge until
    // budget death - the PR #214 shape). Unlike PR #214, a usage-limited bot
    // now FAILS the gate closed (`all_required_passed` is false while
    // usage_limited is non-empty, x-9ab2): the PR does not merge on a quota
    // bounce. The loop terminates cleanly via DoneAwaitingReview instead of
    // spinning. Scoped to the bot's OWN author.login so a stranger's comment
    // never drops a required bot (AC1-ERR). Only still-missing bots are
    // scanned, so a bot that actually reviewed is never usage-limited (AC1-EDGE).
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
            body_is_usage_limit(&body)
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

// ── review coverage (x-0eaf) ──────────────────────────────────────────────────
//
// The old gate's `reviewed` boolean (loopcheck.rs `let reviewed =
// all_required_passed() && unaddressed.is_empty() && reviewers_ok`) was a claim
// about reviews computed entirely from what did NOT happen: nobody is still
// owed, no finding is outstanding, no reviewer is unattested. A quota refusal is
// dropped from `missing_bots` (PR #214) and reads as a pass; on a config with no
// required bots, nothing can object, so `reviewed` is true on zero reviews.
//
// Coverage is the missing predicate: did anyone actually review? It is a
// first-class value reported everywhere, never folded back into the objection
// boolean (collapsing it back undoes this node).
//
// Producer axis, not producer string. Two review producers share the display
// name "codex": the `chatgpt-codex-connector` GitHub App (posts review objects,
// can refuse on quota) and the local `codex` CLI (posts none, never rate-limited
// by the App's quota). They are told apart by `CoverageProducer`, never by the
// reviewer string (x-9ae8's one-word-two-entities disease). A third local lane,
// claude `/code-review`, shares the `LocalAttestation` axis.

/// The channel a review verdict came from. Two producers that share a name (the
/// `chatgpt-codex-connector` App vs the local `codex` CLI) are distinguished by
/// this axis, never by the reviewer string alone (x-9ae8, x-0eaf).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageProducer {
    /// A GitHub App bot that posts review objects via the reviews API. Can
    /// refuse on quota (the `usage_markers` / `body_is_usage_limit` path).
    GithubApp,
    /// A local reviewer that leaves NO GitHub object and instead emits a
    /// head-pinned `review_attestation` event (`emit-attestation.sh`). Never
    /// rate-limited by any App's quota: `/code-review`, the codex CLI, sigma.
    LocalAttestation,
}

/// One verdict for one reviewer over one producer axis (x-0eaf). `reviewed` here
/// is derived from observed evidence, unlike the old boolean of the same name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageVerdict {
    /// Posted a review object, or a `pass` attestation, against a commit whose
    /// code still matches HEAD (`Freshness::counts()`). The only verdict that
    /// counts toward coverage.
    Reviewed,
    /// Responded, but against a commit whose code no longer matches HEAD
    /// (x-5b99). Positive evidence that a reviewer READ AN OLDER COMMIT, which
    /// is a different fact from `Absent` (never responded) and needs a
    /// different response: nudge for a re-read, do not wait for a first read.
    /// Recorded rather than dropped so the trail shows what happened; excluded
    /// from the count, because inheriting a verdict across a commit its author
    /// never saw is the defect this variant exists to make visible.
    Stale,
    /// Responded and declined to review. Quota exhaustion is the first known
    /// shape (detected by `body_is_usage_limit`). Positive evidence a reviewer
    /// exists and will not help - exactly what a nudge or lane failover needs.
    Refused,
    /// Responded with a failure / unparseable payload.
    Errored,
    /// A configured reviewer that produced no response.
    Absent,
}

/// A first-class coverage value, separate from the objection predicate. Never
/// 0-on-error: a failed read is `Unknown`, which behaves as 0 for the autonomous
/// refusal (fail closed) and is reported as "unknown" in the receipt (fail
/// honest). Collapsing an API error into 0 produces false refusals; collapsing
/// it into a count reproduces the bug.
///
/// NOTE (x-0eaf finding 4): `Unknown` is not currently reachable in production.
/// When the GitHub reviews API call fails, `read_pr_info` returns `Err`, which
/// the caller handles by block-retry (fail-safe: the session retries, it does
/// not green or merge). The variant, its receipt, schema enum, and tests exist
/// so that softening the error path to terminate (rather than block) is a
/// one-line change, not a redesign. Do not delete it as dead code without
/// understanding this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coverage {
    /// `n` reviewers reviewed (excludes human approvals until the operator says
    /// otherwise). `Covered(0)` is a real, known zero - distinct from `Unknown`.
    Covered(usize),
    Unknown,
}

impl Coverage {
    /// `true` iff at least one non-human review was observed. `Unknown` is
    /// false: the autonomous path treats unknown as not-covered (fail closed).
    pub fn is_covered(&self) -> bool {
        matches!(self, Coverage::Covered(n) if *n > 0)
    }
}

/// Authorship of a local attestation: did the authoring session emit it, did a
/// different session, or is that unknowable. Computed from the attestation's
/// `attester_session_id` against the manifest's `harness_session_id`.
///
/// Recorded, never gating. `coverage_count` does not read this field: every
/// `Reviewed` verdict counts regardless of origin, `SelfAttested` included.
///
/// The middle state is deliberately not `Independent`. The manifest names the
/// session that ran `fno target init` in the worktree, so a self-handoff
/// successor or a second agent in a shared worktree is a different session and
/// is still not independent. A match is strong evidence of self-attestation; a
/// mismatch is weak evidence of anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttestationOrigin {
    SelfAttested,
    OtherSession,
    Unknown,
}

/// One reviewer's classification, for the `review_coverage` event and receipts.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewerVerdict {
    pub producer: CoverageProducer,
    pub name: String,
    pub verdict: CoverageVerdict,
    /// A human GitHub approval (state `APPROVED`, non-bot author). Computed but
    /// EXCLUDED from the coverage count: whether it should count is the
    /// operator's call (lean: exclude; a solo self-approval is self-cert).
    /// One predicate flip in `CoverageReport::coverage_count` includes it.
    #[serde(skip_serializing_if = "is_false")]
    pub human_approval: bool,
    /// Whether a local attestation was emitted by the authoring session
    /// (`SelfAttested`), a different one (`OtherSession`), or that is
    /// unknowable (`Unknown`). `coverage_count` never reads it: a
    /// `SelfAttested` verdict counts toward coverage exactly like any other,
    /// and a PR whose only attestation is self-attested is covered. Whether
    /// that should stay true is a later gate decision, not this field.
    /// Only meaningful on `local_attestation` verdicts; github_app and
    /// human approvals carry `Unknown` (omitted on serialize) since a GitHub
    /// login has no session to compare. Defaults to `Unknown` so every
    /// pre-existing attestation lands there unchanged.
    #[serde(skip_serializing_if = "is_attestation_origin_unknown")]
    pub attestation_origin: AttestationOrigin,
    /// The commit this reviewer actually read: a github_app review object's
    /// `.commit.oid`, or a local attestation's `data.head_sha`. Empty when
    /// unknowable (a review object with no commit, a verdict with no review),
    /// which [`review_freshness`] treats as `Stale` - fail closed.
    ///
    /// This is the field whose absence WAS the x-5b99 defect: the event pinned
    /// the head at EVAL time, so a bot verdict rendered twelve hours and two
    /// commits earlier serialized as coverage for a commit its author never
    /// saw.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reviewed_sha: String,
    /// Whether `reviewed_sha` still describes the code at HEAD. `None` on a
    /// verdict with no review behind it (`Absent`, `Refused`), where there is
    /// nothing to be fresh or stale about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<Freshness>,
}

/// The coverage over a PR plus the per-reviewer verdicts that produced it.
#[derive(Debug, Clone)]
pub struct CoverageReport {
    pub coverage: Coverage,
    pub verdicts: Vec<ReviewerVerdict>,
}

impl CoverageReport {
    /// Count of `reviewed` verdicts, excluding human approvals. This is the one
    /// place that decides whether a human GitHub approval counts; flip the
    /// `!v.human_approval` guard to include them (the operator's deferred call).
    /// How many of the counted verdicts are the author attesting its own diff.
    /// Recorded, never gating - see `coverage_event_data` for why.
    pub fn self_attested_count(&self) -> usize {
        self.verdicts
            .iter()
            .filter(|v| {
                v.verdict == CoverageVerdict::Reviewed
                    && !v.human_approval
                    && v.attestation_origin == AttestationOrigin::SelfAttested
            })
            .count()
    }

    pub fn coverage_count(&self) -> Option<usize> {
        match &self.coverage {
            Coverage::Unknown => None,
            Coverage::Covered(_) => Some(
                self.verdicts
                    .iter()
                    .filter(|v| v.verdict == CoverageVerdict::Reviewed && !v.human_approval)
                    .count(),
            ),
        }
    }
}

fn is_false(b: &bool) -> bool {
    !b
}

fn is_attestation_origin_unknown(o: &AttestationOrigin) -> bool {
    matches!(o, AttestationOrigin::Unknown)
}

/// Order for "which of this reviewer's reviews is the best evidence". Fresh
/// beats a carry beats stale; the two carry reasons are equally good, since
/// both mean the code under review is unchanged.
fn freshness_rank(f: Freshness) -> u8 {
    match f {
        Freshness::Fresh => 2,
        Freshness::CarriedBaseSync | Freshness::CarriedDocsOnly => 1,
        Freshness::Stale => 0,
    }
}

/// Label a local attestation's authorship from its emitting session vs the
/// worktree's authoring session. A match is `SelfAttested`; a non-empty
/// mismatch is `OtherSession` (NOT "independent" - a self-handoff successor or
/// a shared-worktree sibling is a different session and still not independent);
/// an empty/absent attester, or an unknown author, is `Unknown`. Failing open
/// on unknown authorship keeps the pre-change verdict set byte-identical.
fn classify_attestation_origin(attester: Option<&str>, author: Option<&str>) -> AttestationOrigin {
    match (attester, author) {
        (Some(a), Some(auth)) if a == auth => AttestationOrigin::SelfAttested,
        (Some(_), Some(_)) => AttestationOrigin::OtherSession,
        _ => AttestationOrigin::Unknown,
    }
}

/// One reviewer's latest `pass` attestation, and the commit it pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalPass {
    reviewer: String,
    attester: Option<String>,
    /// The head this attestation pinned. Whether it still counts is
    /// [`review_freshness`]'s call, not this scan's.
    head: String,
}

/// Distinct `(reviewer, attester_session_id)` pairs whose LATEST
/// attestation is `pass`, each with the head it pinned. Keying on the pair - not the reviewer name alone -
/// keeps a same-session re-run collapsed (one key, last-writer-wins, retraction
/// intact) while letting two sessions attesting under the same reviewer label
/// coexist: before this, a spawned peer emitting `code-review` replaced the
/// author's pass and a peer-reviewed PR read `1 reviewed` while deleting its
/// own control. `attester_session_id` is the harness session that emitted, or
/// None when the event predates the field. events.jsonl is append-ordered; a
/// later `fail` revokes, a later `pass` restores - mirrors
/// `unattested_reviewers_scan`'s retraction handling. Pure: scans text, no IO.
/// Presence-based: counts any reviewer regardless of the configured `reviewers`
/// list, so a worker-run `/code-review` counts even when `reviewers: []`.
fn local_latest_passes(events_text: &str) -> Vec<LocalPass> {
    // (reviewer, attester_session_id) -> (head it attested, was it a pass). The
    // attester lives in the key so cross-session attestations join instead of
    // replace. The HEAD is no longer a filter, it is a RESULT: which head an
    // attestation pinned is what the freshness predicate needs, and dropping
    // every non-matching line here is what made a rebase destroy a review.
    let mut latest: std::collections::HashMap<(String, Option<String>), (String, bool)> =
        std::collections::HashMap::new();
    for line in events_text.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("review_attestation") {
            continue;
        }
        let Some(r) = val.pointer("/data/reviewer").and_then(|v| v.as_str()) else {
            continue;
        };
        // An event with no head_sha is not head-pinned evidence and is skipped
        // outright (defaulting it to "" would match a caller whose own head is
        // "", turning unpinned data into coverage - codex P1 on the attestation
        // gate, same class of lie this node deletes).
        let Some(line_head) = val.pointer("/data/head_sha").and_then(|v| v.as_str()) else {
            continue;
        };
        if line_head.is_empty() {
            continue;
        }
        let is_pass = val.pointer("/data/verdict").and_then(|v| v.as_str()) == Some("pass");
        // attester_session_id is the live session that emitted; None on events
        // that predate the field (the whole backlog), which classifies as
        // Unknown downstream. Empty string is treated as None so the producer's
        // "unobservable" sentinel and the field's absence read identically.
        let attester = val
            .pointer("/data/attester_session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        // Under the pair key a peer's `fail` revokes only the peer's own pass;
        // the author's `pass` still counts toward coverage. Coverage counts
        // reviews performed, not approvals granted - the hold on a bad review
        // lives on `open_review_findings` and on `unattested_reviewers_scan`,
        // which keeps its name key (the config.review.reviewers gate).
        latest.insert(
            (r.trim_start_matches('/').to_string(), attester),
            (line_head.to_string(), is_pass),
        );
    }
    let mut out: Vec<LocalPass> = latest
        .into_iter()
        .filter(|(_, (_, pass))| *pass)
        .map(|((reviewer, attester), (head, _))| LocalPass {
            reviewer,
            attester,
            head,
        })
        .collect();
    out.sort_by(|a, b| {
        (&a.reviewer, &a.attester, &a.head).cmp(&(&b.reviewer, &b.attester, &b.head))
    });
    out
}

/// Whether an author login is a KNOWN review App (a BOT_PROFILES login or a
/// configured github_app). A present review from such an App counts toward
/// coverage; a random `[bot]` suffix alone does not (it may be a non-review
/// automation).
fn author_is_known_bot(author: &str, github_app_logins: &[String]) -> bool {
    BOT_PROFILES.iter().any(|p| author.contains(p.login))
        || github_app_logins
            .iter()
            .any(|l| login_matches_bot(author, l))
}

/// Whether an author login is a bot of any kind (known App, or any `[bot]`).
/// Used to separate a human GitHub approval from an app review on the same axis.
fn author_is_bot(author: &str, github_app_logins: &[String]) -> bool {
    author.ends_with("[bot]") || author_is_known_bot(author, github_app_logins)
}

/// Classify every reviewer response and compute coverage. Pure: takes the
/// already-fetched GitHub review/comment arrays, the events.jsonl text, the
/// current HEAD, and the configured GitHub-App logins; performs no IO, so it is
/// unit-testable in isolation. A failed GitHub read (`github_read_ok = false`)
/// makes coverage `Unknown` UNLESS the local axis carries a head-pinned pass:
/// positive local evidence survives a bot outage, because the local lane is
/// never rate-limited by the bot's quota (the PR #214 failure in a new hat,
/// which this node exists to escape). (x-0eaf)
///
/// `author_session` is the manifest's `harness_session_id` (the session that ran
/// `fno target init` in this worktree). Each local attestation's
/// `attester_session_id` is compared against it to label `attestation_origin`;
/// `None` (no manifest / unparseable) leaves every local verdict `Unknown`,
/// failing open on unknown authorship so the coverage verdict is byte-identical
/// to the pre-change behavior. `coverage_count` never reads the origin: every
/// `Reviewed` verdict counts regardless of it, `SelfAttested` included.
pub fn classify_coverage(
    reviews: &[Value],
    comments: &[Value],
    events_text: &str,
    github_app_logins: &[String],
    github_read_ok: bool,
    author_session: Option<&str>,
    freshness: &dyn Fn(&str) -> Freshness,
) -> CoverageReport {
    let local_passes = local_latest_passes(events_text);
    let mut verdicts: Vec<ReviewerVerdict> = Vec::new();

    if github_read_ok {
        // (1) Collect distinct KNOWN review-App authors that posted a review
        // object. A present review counts whether or not the App is in the
        // configured required/optional list - "did anyone review" must not
        // hinge on the operator having pre-listed the bot that happened to
        // review (chatgpt-codex-connector reviewing on a default
        // no-required-bots config still counts). A known App is a BOT_PROFILES
        // login or a configured github_app; a bare `[bot]` suffix is not.
        //
        // Each author keeps its FRESHEST review, not its latest: `.commit.oid`
        // says which commit that review read, and an author that reviewed
        // several commits is covered by whichever of them still describes HEAD.
        let mut reviewed_authors: Vec<(String, String, Freshness)> = Vec::new();
        for r in reviews {
            let author = r
                .pointer("/author/login")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if author.is_empty() || !author_is_known_bot(author, github_app_logins) {
                continue;
            }
            let state = r.get("state").and_then(|v| v.as_str()).unwrap_or("");
            if state.is_empty() {
                continue;
            }
            // The commit the reviewer actually read. Already in this payload
            // (`gh pr view --json reviews`) and discarded until now, so pinning
            // the github_app axis costs no new API call. Absent -> "" ->
            // Stale, which is the fail-closed direction.
            let oid = r
                .pointer("/commit/oid")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let fresh = freshness(oid);
            match reviewed_authors
                .iter_mut()
                .find(|(a, _, _)| logins_correspond(a, author))
            {
                Some(entry) => {
                    if freshness_rank(fresh) > freshness_rank(entry.2) {
                        entry.1 = oid.to_string();
                        entry.2 = fresh;
                    }
                }
                None => reviewed_authors.push((author.to_string(), oid.to_string(), fresh)),
            }
        }
        // (2) One verdict per unique configured login: reviewed if a
        // corresponding author posted, else refused on a usage-limit comment,
        // else absent. Dedup so a login in two lists is one verdict, not two
        // units of coverage (Failure Modes).
        let mut seen: Vec<String> = Vec::new();
        for login in github_app_logins {
            let login = login.trim();
            if login.is_empty() || seen.iter().any(|s| logins_correspond(s, login)) {
                continue;
            }
            seen.push(login.to_string());
            let hit = reviewed_authors
                .iter()
                .find(|(a, _, _)| logins_correspond(a, login));
            let (verdict, reviewed_sha, fresh) = match hit {
                // Reviewed at a commit that still describes HEAD, or reviewed
                // at one that does not. Both are a response; only the first is
                // coverage. Recording the second as `Stale` rather than
                // silently dropping it is what makes the tightening auditable.
                Some((_, sha, f)) => (
                    if f.counts() {
                        CoverageVerdict::Reviewed
                    } else {
                        CoverageVerdict::Stale
                    },
                    sha.clone(),
                    Some(*f),
                ),
                None => {
                    let refused = comments.iter().any(|c| {
                        let ca = c
                            .pointer("/author/login")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        logins_correspond(ca, login)
                            && body_is_usage_limit(
                                c.get("body").and_then(|v| v.as_str()).unwrap_or(""),
                            )
                    });
                    (
                        if refused {
                            CoverageVerdict::Refused
                        } else {
                            CoverageVerdict::Absent
                        },
                        String::new(),
                        None,
                    )
                }
            };
            verdicts.push(ReviewerVerdict {
                producer: CoverageProducer::GithubApp,
                name: login.to_string(),
                verdict,
                human_approval: false,
                attestation_origin: AttestationOrigin::Unknown,
                reviewed_sha,
                freshness: fresh,
            });
        }
        // (3) Known-App reviewers NOT in the configured list still count
        // (reviewed), so coverage reflects the review that actually happened.
        for (author, sha, fresh) in &reviewed_authors {
            if !seen.iter().any(|s| logins_correspond(author, s)) {
                verdicts.push(ReviewerVerdict {
                    producer: CoverageProducer::GithubApp,
                    name: author.clone(),
                    verdict: if fresh.counts() {
                        CoverageVerdict::Reviewed
                    } else {
                        CoverageVerdict::Stale
                    },
                    human_approval: false,
                    attestation_origin: AttestationOrigin::Unknown,
                    reviewed_sha: sha.clone(),
                    freshness: Some(*fresh),
                });
            }
        }
        // (4) Human GitHub approvals: non-bot authors with state APPROVED.
        // Recorded as `reviewed` with `human_approval: true` so they are
        // visible but excluded from the count until the operator decides (lean:
        // exclude). Computed here so the answer is a one-line flip, not a
        // redesign.
        let mut seen_human: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in reviews {
            let author = r
                .pointer("/author/login")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if author.is_empty()
                || author_is_bot(author, github_app_logins)
                || seen_human.contains(author)
            {
                continue;
            }
            if r.get("state").and_then(|v| v.as_str()) == Some("APPROVED") {
                seen_human.insert(author.to_string());
                // Freshness applies here too, though it changes no count: a
                // human approval is excluded either way. It changes what a
                // human READS in `fno pr status`, and an approval rendered
                // identically whether or not its author saw this code is the
                // same lie one axis down.
                let oid = r
                    .pointer("/commit/oid")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let fresh = freshness(oid);
                verdicts.push(ReviewerVerdict {
                    producer: CoverageProducer::GithubApp,
                    name: author.to_string(),
                    verdict: if fresh.counts() {
                        CoverageVerdict::Reviewed
                    } else {
                        CoverageVerdict::Stale
                    },
                    human_approval: true,
                    attestation_origin: AttestationOrigin::Unknown,
                    reviewed_sha: oid.to_string(),
                    freshness: Some(fresh),
                });
            }
        }
    }

    // local_attestation axis: one verdict per distinct latest `pass`, labeled
    // with whether the authoring session emitted it, and pinned to the head the
    // attestation itself recorded rather than to the head at eval time.
    for lp in &local_passes {
        let fresh = freshness(&lp.head);
        verdicts.push(ReviewerVerdict {
            producer: CoverageProducer::LocalAttestation,
            name: lp.reviewer.clone(),
            verdict: if fresh.counts() {
                CoverageVerdict::Reviewed
            } else {
                CoverageVerdict::Stale
            },
            human_approval: false,
            attestation_origin: classify_attestation_origin(lp.attester.as_deref(), author_session),
            reviewed_sha: lp.head.clone(),
            freshness: Some(fresh),
        });
    }

    // Unknown only when the GitHub read failed AND no local review still
    // describes HEAD. A COUNTING local pass is positive evidence that trumps a
    // bot outage, so coverage is Known(local) in that case, not Unknown. A
    // stale local pass is not evidence of anything current, so it must not
    // rescue the read the way a fresh one does.
    let local_counts = verdicts.iter().any(|v| {
        v.producer == CoverageProducer::LocalAttestation && v.verdict == CoverageVerdict::Reviewed
    });
    let coverage = if !github_read_ok && !local_counts {
        Coverage::Unknown
    } else {
        Coverage::Covered(
            verdicts
                .iter()
                .filter(|v| v.verdict == CoverageVerdict::Reviewed && !v.human_approval)
                .count(),
        )
    };

    CoverageReport { coverage, verdicts }
}

/// Build the `review_coverage` event payload. The per-reviewer verdicts
/// serialize via their serde derives (producer/verdict snake_cased);
/// `reviewed_count` is included only when coverage is `Covered` (omitted, not
/// null, when Unknown, matching the schema).
///
/// `repo` is the git-remote slug, and it is what makes this event safe to write
/// into the CROSS-PROJECT `~/.fno/events.jsonl`: `pr` alone is a bare integer,
/// so a reader scanning the global log for PR 781 would otherwise accept
/// another repo's PR 781 as coverage for this one. Omitted (not null) when the
/// slug is unresolvable, and a reader must then decline to match it globally.
fn coverage_event_data(
    pr: i64,
    rep: &CoverageReport,
    head_sha: &str,
    repo: &str,
    author_session: Option<&str>,
) -> serde_json::Value {
    // Three states, not two. `Covered(0)` is a real known zero and
    // `Coverage::is_covered()` has always returned false for it, but the
    // serializer rendered every `Covered(n)` as the string "covered" - so
    // `coverage: "covered"` and `reviewed_count: 0` co-occurred on three PRs
    // in flight, and the reassuring WORD sat beside the honest NUMBER. A
    // reader trusts the word. Emitting "uncovered" for a zero makes the two
    // agree, and it is additive for every current consumer: they all already
    // test `coverage == "covered" AND count > 0`, so a historical "covered"
    // event with a zero count keeps reading as not-covered.
    let coverage_str = match &rep.coverage {
        Coverage::Unknown => "unknown",
        Coverage::Covered(0) => "uncovered",
        Coverage::Covered(_) => "covered",
    };
    let mut data = serde_json::json!({
        "pr": pr,
        "coverage": coverage_str,
        "verdicts": &rep.verdicts,
        "head_sha": head_sha,
    });
    if let Coverage::Covered(n) = &rep.coverage {
        data["reviewed_count"] = serde_json::json!(n);
        // How much of that count is the author reviewing its own diff. Nothing
        // gates on it: self-review is the DEFAULT path (`self_review_required`
        // floors `/code-review` onto the author's own head), so refusing a
        // self-attested pass would wedge every single-session PR, and whether
        // it SHOULD is a merge-authority decision rather than a freshness one.
        // What was missing is that the answer lived only in prose. It is a
        // number on the verdict now, so a reader can see it and a future gate
        // is one predicate rather than a redesign. Deliberately not called
        // `independent_count`: the schema is explicit that `other_session` is
        // not independence, and this must not launder that.
        //
        // Emitted ONLY when authorship was measured. classify_attestation_origin
        // labels every verdict Unknown when `author_session` is None, so
        // `self_attested_count()` would read 0 while the truth is unmeasured -
        // a measured-zero shape (x-62a1: an aggregate reporting a state its
        // inputs do not support). The field is omitted instead, never 0, so
        // the day a gate enforces it, absence reads unmeasured rather than
        // "no self-attest" and cannot serve as the bypass.
        if author_session.is_some() {
            data["self_attested_count"] = serde_json::json!(rep.self_attested_count());
        }
    }
    if !repo.is_empty() {
        data["repo"] = serde_json::json!(repo);
    }
    data
}

/// Whether every `github_app` verdict went stale WITHOUT naming a commit.
///
/// One bot with an empty `commit.oid` is a payload quirk. EVERY bot with an
/// empty one, and none reviewed, is the signature of a `gh` too old to return
/// the field - which makes freshness unresolvable for the whole axis, forever,
/// so a required bot never clears and the loop has no reachable exit. Failing
/// closed is right; reporting it as "reviewed an older commit" is not, because
/// the fix is a gh upgrade rather than a re-read.
///
/// Requires at least one stale verdict, so a PR with no bot reviews at all
/// (every verdict `Absent`) never matches: an absence of reviewers is a
/// different fact from an absence of commits on the reviews that exist.
fn blind_to_reviewed_commits(rep: &CoverageReport) -> bool {
    let github: Vec<&ReviewerVerdict> = rep
        .verdicts
        .iter()
        .filter(|v| v.producer == CoverageProducer::GithubApp)
        .collect();
    let staleness: Vec<&&ReviewerVerdict> = github
        .iter()
        .filter(|v| v.verdict == CoverageVerdict::Stale)
        .collect();
    !staleness.is_empty() && staleness.iter().all(|v| v.reviewed_sha.is_empty())
}

/// One-line coverage summary for the terminal message and receipts (x-0eaf
/// task 3.1). Printed from the coverage value at print time, never from a
/// remembered gate verdict (receipts have lied before).
pub fn coverage_receipt_line(rep: &CoverageReport) -> String {
    match &rep.coverage {
        Coverage::Unknown => "review coverage: unknown (review read failed)".to_string(),
        Coverage::Covered(n) => {
            let reviewed_names: Vec<&str> = rep
                .verdicts
                .iter()
                .filter(|v| v.verdict == CoverageVerdict::Reviewed && !v.human_approval)
                .map(|v| v.name.as_str())
                .collect();
            if *n > 0 {
                // Origin breakdown over EVERY reviewed (non-human) verdict, folded
                // by its attestation_origin, so the three buckets sum to `n`. The
                // self-attestation hazard lives on the local lane; a GitHub App
                // review has no session to compare and reads `unknown` here (it is
                // named above, so a reader sees it reviewed - "unknown" is its
                // origin, not its verdict). All three buckets are always shown so
                // a reader learns the vocabulary even when two are zero; `other`
                // is a different session, NOT "independent".
                //
                // "all origins counted" is load-bearing: readers took the bare
                // tally for a subtraction and refused to merge green PRs over
                // it. A positive claim, not a disclaimer - a denial ("not a
                // gate") answers the question by raising it. Scoped to ORIGINS
                // because `n` does drop human approvals, so a bare "all
                // counted" would be false on a human-approved PR.
                let (self_n, other_n, unknown_n) = rep
                    .verdicts
                    .iter()
                    .filter(|v| v.verdict == CoverageVerdict::Reviewed && !v.human_approval)
                    .fold((0, 0, 0), |(s, o, u), v| match v.attestation_origin {
                        AttestationOrigin::SelfAttested => (s + 1, o, u),
                        AttestationOrigin::OtherSession => (s, o + 1, u),
                        AttestationOrigin::Unknown => (s, o, u + 1),
                    });
                return format!(
                    "review coverage: {} reviewed ({}) - all origins counted; self {}, other {}, unknown {}",
                    n,
                    reviewed_names.join(", "),
                    self_n,
                    other_n,
                    unknown_n
                );
            }
            let refused: Vec<&str> = rep
                .verdicts
                .iter()
                .filter(|v| v.verdict == CoverageVerdict::Refused)
                .map(|v| v.name.as_str())
                .collect();
            let errored = rep
                .verdicts
                .iter()
                .filter(|v| v.verdict == CoverageVerdict::Errored)
                .count();
            // Absent reviewers are NAMED: "the reviewers above" pointed at the
            // refused ones, the only names the line had.
            let absent: Vec<&str> = rep
                .verdicts
                .iter()
                .filter(|v| v.verdict == CoverageVerdict::Absent)
                .map(|v| v.name.as_str())
                .collect();
            // Stale reviewers are NAMED too. Without this the receipt for the
            // x-5b99 specimen reads "0 reviewed, 0 refused, 0 errored, 0
            // absent" - four zeros describing a PR a bot really did review, at
            // an older commit. That is the absence-shaped lie the Stale variant
            // exists to delete, and dropping it from the one line a human reads
            // puts it straight back.
            let stale: Vec<&str> = rep
                .verdicts
                .iter()
                .filter(|v| v.verdict == CoverageVerdict::Stale)
                .map(|v| v.name.as_str())
                .collect();
            // Never prescribe the local verb while anyone is absent, and never
            // suppress the next action entirely either. Both were tried here and
            // both were wrong: the offer walks a worker into self-attesting past
            // a reviewer that may be REQUIRED (the merge gate reads coverage
            // alone, so nothing downstream catches it), and bare suppression
            // strands an optional App that is never installed, which sits absent
            // forever with no reachable exit.
            //
            // The escape is that this line cannot know required-ness and should
            // not try. Name who is outstanding and point at the one move that is
            // safe whichever they are: check whether they are still configured.
            let next = if !absent.is_empty() {
                format!(
                    "waiting on {} - if a reviewer there is uninstalled or no longer configured, check config.review",
                    absent.join(", ")
                )
            } else if !stale.is_empty() && blind_to_reviewed_commits(rep) {
                // EVERY github_app verdict is stale AND none carries a commit at
                // all. That is not "the bots read an older commit", it is "we
                // cannot see which commit any bot read", and the two need
                // opposite responses. `gh pr view --json reviews` supplies
                // `commit.oid`; a gh too old to return it makes every bot review
                // stale forever, so a required bot never clears and the loop
                // blocks with no reachable exit. Failing closed is correct, but
                // a closed gate that reports the wrong cause is the same
                // absence-shaped lie this whole change deletes - so say which
                // absence it is.
                format!(
                    "no review carries a reviewed commit ({}) - `gh pr view --json reviews` must return `commit.oid`; upgrade gh, then ask for a re-read",
                    stale.join(", ")
                )
            } else if !stale.is_empty() {
                // A re-read by the reviewer that already responded, not a local
                // self-attest: "run the review verb" would walk a worker past a
                // reviewer that may be REQUIRED and has simply gone stale.
                format!(
                    "{} reviewed an older commit whose code no longer matches HEAD - ask for a re-read",
                    stale.join(", ")
                )
            } else {
                "run the review verb at HEAD".to_string()
            };
            // `stale` counts in the tally and is NAMED in the next action, like
            // `absent`. `refused` keeps its inline names, because a refusal is
            // terminal and never drives the next action, so the tally is the
            // only place a reader can learn who declined.
            //
            // Either way the parenthetical is dropped when the list is empty.
            // A trailing `()` is a shape a previous fix deleted from this exact
            // line, and the refused bucket had quietly kept printing it in
            // every case where nothing refused - which is most of them.
            let refused_names = if refused.is_empty() {
                String::new()
            } else {
                format!(" ({})", refused.join(", "))
            };
            format!(
                "review coverage: 0 reviewed, {} refused{}, {} errored, {} stale, {} absent. No head-pinned pass attestation for this head - {}.",
                refused.len(),
                refused_names,
                errored,
                stale.len(),
                absent.len(),
                next
            )
        }
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
    /// Whether this finding's thread had ANY non-bot reply. False is the
    /// silent-failure shape (PR #447, #787): the finding was answered with a
    /// top-level PR comment, which this gate cannot read, so it reads as
    /// unaddressed with no named cause. Carried per-finding so the block
    /// reason can name the count and the missing in_reply_to mechanism.
    had_reply: bool,
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
                            had_reply: false,
                        });
                    }
                }
            }
        }
    }

    let unaddressed: Vec<Finding> = candidates
        .into_iter()
        .filter_map(|mut f| {
            let non_bot_replies = replies.get(&f.id);
            let has_reply = non_bot_replies.map(|r| !r.is_empty()).unwrap_or(false);
            // Record whether this finding's thread had any non-bot reply so
            // the block reason can name the top-level-comment blind spot.
            f.had_reply = has_reply;
            if !has_reply {
                return Some(f); // no ack -> unaddressed
            }
            let commit_after = commit_dates.iter().any(|d| ts_after(d, &f.created_at));
            let wontfix = non_bot_replies
                .map(|rs| rs.iter().any(|b| b.to_lowercase().contains(WONTFIX_MARKER)))
                .unwrap_or(false);
            if !(commit_after || wontfix) {
                Some(f)
            } else {
                None
            }
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

/// Default debounce window: an unchanged fingerprint seen again inside this many
/// seconds is the SAME observation, not a new one. The streak counts independent
/// observations of an unchanged world, not stop-hook fires -- a session taking
/// short turns used to burn a 5-fire backstop in 109 seconds while its CI run
/// still had 7 minutes to go, which no external wait can outrun. The effective
/// floor becomes `(backstop_n - 1) * gap`: 10 minutes unattended, 20 attended.
/// Override with `FNO_LOOPCHECK_MIN_FIRE_GAP_SECS` (0 restores fire counting).
const MIN_FIRE_GAP_SECS: i64 = 300;

/// Resolve the debounce window from the env seam, falling back to the default.
/// Mirrors the `FNO_LOOPCHECK_GH_BIN` / `_NO_NOTIFY` / `_NO_COMMENT` seams.
fn min_fire_gap_secs() -> i64 {
    std::env::var("FNO_LOOPCHECK_MIN_FIRE_GAP_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(MIN_FIRE_GAP_SECS)
}

/// Count prior loop_check events for this session_id in the project events file.
/// Returns (total_fires, consecutive_unchanged_count, last_fingerprint_in_log,
/// streak_window_secs).
///
/// `current_fp` is the fingerprint computed this fire (used for streak matching).
/// `last_fp` is the most recent fingerprint recorded in the events log for this
/// session -- used for carry-forward when the gh pre-read fails this fire.
/// `streak_window_secs` is the span from the oldest COUNTED fire to `now`; it is
/// what makes a streak count falsifiable from the events log.
///
/// The streak is debounced by `min_gap_secs`: walking backwards from `now`, a
/// matching fire closer than the gap to the last counted one is skipped
/// TRANSPARENTLY and does not advance the cursor, so a burst collapses to a
/// single observation. The asymmetry is deliberate and load-bearing: a CHANGED
/// fingerprint breaks the streak at any spacing, because real progress is real
/// progress at any speed -- only the *absence* of change needs time to be
/// credible.
fn read_prior_fires(
    events_path: &Path,
    session_id: &str,
    current_fp: &str,
    now: DateTime<Utc>,
    min_gap_secs: i64,
) -> (u64, u64, Option<String>, i64) {
    let Ok(content) = std::fs::read_to_string(events_path) else {
        return (0, 0, None, 0);
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
    // and capture the most recent fp recorded. `next_ts` is the cursor: it starts
    // at `now` and only moves to a fire that was COUNTED, which is what collapses
    // a rapid burst into one observation.
    let mut consecutive: u64 = 0;
    let mut last_fp: Option<String> = None;
    let mut next_ts = now;
    let mut oldest_counted_ts: Option<DateTime<Utc>> = None;
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
        // A CHANGED fingerprint breaks the streak at ANY spacing - progress is
        // never debounced. This check precedes the gap check on purpose.
        if fp != current_fp {
            break;
        }
        // Debounce. A fire we cannot place in time is skipped transparently
        // rather than counted: giving up on a parse error must fail AWAY from
        // an irreversible NoProgress, matching classify_bot_nudge's precedent.
        let Some(ts) = val
            .get("ts")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        else {
            continue;
        };
        let gap = (next_ts - ts).num_seconds();
        // gap < 0 means clock skew (a fire stamped after `now`); count it rather
        // than invent a debounce from a bad clock - status quo, no crash.
        if gap < 0 || gap >= min_gap_secs {
            consecutive += 1;
            next_ts = ts;
            oldest_counted_ts = Some(ts);
        }
        // else: same observation seen twice; skip WITHOUT advancing next_ts.
    }

    let streak_window_secs = oldest_counted_ts
        .map(|t| (now - t).num_seconds().max(0))
        .unwrap_or(0);

    (total, consecutive, last_fp, streak_window_secs)
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
    /// Override for the ambient author harness (default: the env markers read
    /// by `claims::resolve_harness`). `--author-harness none` pins "no harness".
    /// Every other ambient input here already had an override, and this one did
    /// not, so a test inherited whatever harness ran it: the four review-gate
    /// cases passed in CI and failed under `cargo test` from inside Claude Code,
    /// where the marker floors a self-review reviewer they do not expect.
    author_harness_override: Option<String>,
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
    let mut author_harness_override: Option<String> = None;
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
        } else if let Some(val) = try_flag_value(arg, "--author-harness", args, &mut i) {
            author_harness_override = Some(val);
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
        author_harness_override,
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

/// The manifest-independent inputs every coverage/review evaluation needs:
/// event-log paths, repo identity, the merged settings, and the reviewer sets
/// derived from them. Extracted from `decide()` (x-3a3f) so the standalone
/// `review-coverage` verb resolves EXACTLY what the stop hook resolves - one
/// resolver, no second precedence implementation (the N-implementations trap).
pub(crate) struct ReviewInputs {
    pub(crate) project_events: PathBuf,
    pub(crate) global_events: PathBuf,
    /// Full `host/owner/repo` from the git remote; empty when unresolvable.
    pub(crate) repo_slug: String,
    pub(crate) settings: Settings,
    /// The ambient author harness (env markers, or the explicit override).
    pub(crate) author_harness: Option<String>,
    pub(crate) required_bots: Vec<String>,
    pub(crate) required_reviewers: Vec<String>,
    pub(crate) optional_bots: Vec<String>,
    pub(crate) nudge_configs: Vec<NudgeConfig>,
}

/// Resolve [`ReviewInputs`]: event paths, repo slug, the GLOBAL-then-local
/// settings overlay, and the bot/reviewer sets derived from it. This is the
/// block `decide()` ran inline; it moves here unchanged (including the
/// fail-closed unparseable-settings branch, which is why this cannot be a
/// naive copy) so `decide()` and `run_review_coverage` share one resolver.
pub(crate) fn resolve_review_inputs(
    cwd: &Path,
    events_path: Option<&Path>,
    global_events_path: Option<&Path>,
    settings_path: Option<&Path>,
    global_settings_path: Option<&Path>,
    author_harness_override: Option<&str>,
) -> ReviewInputs {
    let project_events = events_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.join(".fno/events.jsonl"));

    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let global_events = global_events_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(&home).join(".fno/events.jsonl"));

    // Scopes the review_coverage event written into the cross-project global
    // log. The git remote is the one identifier canonical and every one of its
    // worktrees agree on, which is exactly the agreement the coverage reader
    // needs (x-f43c). It is the FULL `host/owner/repo`, not the last path
    // segment: this key gates auto-merge, so `org-a/widget` aliasing
    // `org-b/widget` would let one repo's coverage clear the other's guard.
    // Empty when there is no remote; the payload then omits `repo` and no
    // reader will claim the event.
    let repo_slug = crate::finalize::repo_identity_from_git_remote(cwd).unwrap_or_default();

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
    let settings = if let Some(explicit) = settings_path {
        if let Ok(sc) = std::fs::read_to_string(explicit) {
            parse_or_emit(&sc, explicit)
        } else {
            Settings::default()
        }
    } else {
        let global_path = global_settings_path
            .map(Path::to_path_buf)
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
            if local.self_review_required.is_some() {
                // Presence, not value: `self_review_required = false` is the
                // documented repo opt-out, so a local Some(false) must override
                // a global Some(true). Same overlay rule as required_bots.
                merged.self_review_required = local.self_review_required;
            }
            if !local.nudge_overrides.is_empty() {
                // Without this line a project-local `[review.nudge]` (including
                // `enabled = false`) is read from the GLOBAL file only and the
                // repo's own overrides vanish - loop-check would post a nudge a
                // repo explicitly opted out of. Same per-field-overlay trap the
                // done_probes line below documents.
                merged.nudge_overrides = local.nudge_overrides;
            }
            if !local.peers.is_empty() {
                merged.peers = local.peers;
            }
            if local.peer_identity.is_some() {
                merged.peer_identity = local.peer_identity;
            }
            if local.done_probes.is_some() {
                // Presence, not non-emptiness: a project-local `done_probes = []`
                // is a deliberate "this repo declares none", same rule as
                // required_bots. Omitting this line entirely is the silent
                // guardrail bypass this list keeps re-inviting - the field would
                // be read from the GLOBAL file only and the project's own gate
                // would never run.
                merged.done_probes = local.done_probes;
            }
        }
        merged
    };

    // Resolve the must-have-reviewed list once (code default when unset). The
    // author harness (from the ambient env markers, shared with claims.rs) drives
    // the same-model peer guard (x-c2e7); None leaves the set unchanged.
    // `--author-harness none` pins the no-harness case, which an absent flag
    // cannot express, and an absent flag keeps reading the ambient markers.
    let author_harness = match author_harness_override {
        Some("none") | Some("") => None,
        Some(h) => Some(h.to_string()),
        None => crate::claims::resolve_harness(),
    };
    let required_bots = resolved_required_bots_for_author(&settings, author_harness.as_deref());
    let mut required_reviewers = settings.reviewers.clone();
    for reviewer in resolved_local_peer_reviewers_for_author(&settings, author_harness.as_deref()) {
        if !required_reviewers.contains(&reviewer) {
            required_reviewers.push(reviewer);
        }
    }
    let optional_bots = resolved_optional_bots(&settings);
    let nudge_configs = resolved_nudge_configs(&settings);

    ReviewInputs {
        project_events,
        global_events,
        repo_slug,
        settings,
        author_harness,
        required_bots,
        required_reviewers,
        optional_bots,
        nudge_configs,
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

    // Resolve paths + settings + reviewer sets through the ONE shared resolver
    // (x-3a3f): the standalone review-coverage verb resolves exactly these,
    // from the same overlay, so there is no second precedence implementation.
    let inputs = resolve_review_inputs(
        &cwd,
        parsed.events_path.as_deref(),
        parsed.global_events_path.as_deref(),
        parsed.settings_path.as_deref(),
        parsed.global_settings_path.as_deref(),
        parsed.author_harness_override.as_deref(),
    );
    let project_events = inputs.project_events;
    let global_events = inputs.global_events;
    let repo_slug = inputs.repo_slug;
    let settings = inputs.settings;
    let author_harness = inputs.author_harness;
    let required_bots = inputs.required_bots;
    let mut required_reviewers = inputs.required_reviewers;
    let optional_bots = inputs.optional_bots;
    let nudge_configs = inputs.nudge_configs;

    let ledger_path = parsed
        .ledger_path
        .clone()
        .unwrap_or_else(|| cwd.join(".fno/ledger.json"));
    // A code payload carries its own review obligation on a stock install:
    // when no lane is configured, the harness-resolved self-review reviewer is
    // floored onto `required_reviewers` so the existing unattested_reviewers_scan
    // holds the session for a head-pinned attestation instead of the run asking
    // an epic leader. Opt out with config.review.self_review_required = false.
    // classify_payload fails CLOSED, so an unreadable diff floors the reviewer
    // rather than waving the obligation away. The floor is additive: an
    // already-configured lane (reviewers, bots, peers) keeps meaning exactly
    // what it meant today, and a lane that already names code-review is a no-op.
    let lane_configured =
        !required_bots.is_empty() || !optional_bots.is_empty() || !required_reviewers.is_empty();
    let self_review_required = settings.self_review_required.unwrap_or(true);
    // The floor only applies where a session can satisfy it: a harness with a
    // self-review verb (claude /code-review, codex /review). Flooring
    // gemini/agy/opencode would demand an attestation no verb there produces,
    // wedging the loop; route 3 (a spawned reviewer) is those harnesses' path
    // and is deferred. classify_payload forks git, so it runs only when the
    // floor could apply - a configured lane makes it moot, and most fires have one.
    let harness_can_self_review = harness_can_self_review(author_harness.as_deref());
    let self_review_floor = if !lane_configured && self_review_required && harness_can_self_review {
        let payload = classify_payload(&parsed.git_bin, &cwd);
        floor_self_review(&required_reviewers, false, payload.0, true)
    } else {
        None
    };
    if let Some(floored) = self_review_floor.clone() {
        required_reviewers.push(floored);
    }
    // x-0eaf: DoneUnreviewed applies only when review is required. A stock
    // install that opts out (self_review_required=false AND no lane, or a
    // harness with no self-review verb) has zero coverage as its configured
    // state, not a defect - those green PRs still reach DonePRGreen.
    let review_required = lane_configured || self_review_floor.is_some();

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

    let generic = crate::delivery_completion::evaluate_manifest(
        &cwd,
        manifest.plan_path.as_deref(),
        &project_events,
    );
    // ── Step 3b: decided question left no decision record ────────────────────
    // The recording obligation is enforced here, never self-reported: a
    // session that closed one of ITS OWN operator questions WITH an answer but
    // emitted no matching operator_decision event is held, and the hold names
    // the question. Scopes to questions this session asked so a foreign
    // session's unfinished business cannot wedge an unrelated loop. The
    // journals are folded as a UNION because the operator verbs write to the
    // canonical root's journal while a worktree stop gate reads its own cwd's
    // - a record on any of the three paths clears the gate.
    let unrecorded = if session_id != "unknown" {
        let mut journals = vec![project_events.clone(), global_events.clone()];
        if let Some(canon) = crate::paths::canonical_repo_root(&cwd) {
            journals.push(canon.join(".fno/events.jsonl"));
        }
        scan_unrecorded_decisions(&journals, &session_id)
    } else {
        Vec::new()
    };
    if !unrecorded.is_empty() {
        let names = unrecorded
            .iter()
            .map(|u| format!("{} '{}'", u.question_id, u.question))
            .collect::<Vec<_>>()
            .join(", ");
        let reason = format!(
            "a decided question has no decision record ({names}); record it with \
             `fno decide --subject <node> --question-id <id> --decision \"...\"` \
             (the gate matches on the question id; a re-run of the clear is a \
             no-op once the question is closed) \
             so the decision survives this session"
        );
        emit(
            "loop_check",
            serde_json::json!({
                "session_id": session_id,
                "decision": "block",
                "gate": "unrecorded_decision",
                "unrecorded": unrecorded.iter().map(|u| u.question_id.clone()).collect::<Vec<_>>()
            }),
        );
        return (0, allow_output("block", None, &reason, 0, None));
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

    if !gh_available
        && matches!(
            generic,
            crate::delivery_completion::DeliveryCompletion::Inactive
        )
    {
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

    // ── reserve a GraphQL floor for the merge guard ────────────────────────
    // The GraphQL quota is per-USER and shared by every session on the machine;
    // an idle fire's fingerprint reads are the unbounded low-value consumer that
    // starves the bounded high-value one (the promise/merge evaluation). Below
    // the floor, a fire carrying no promise intent STANDS DOWN: it spends no
    // GraphQL at all rather than politely spending less. A promise-intent fire
    // always proceeds - the floor belongs to it. The probe itself is REST and
    // primary-exempt; a failed probe (None) changes nothing.
    let quota_probe = probe_graphql_quota(gh_bin, &cwd);
    if let Some(q) = &quota_probe {
        // Aborted is exempt too: honoring a cancel spends no GraphQL (the
        // Aborted terminal in done() reads nothing), so blocking it here
        // would trap a cancelled session behind the floor for a whole reset
        // window - the operator's cancel outranks the reserve.
        if q.remaining < GRAPHQL_FLOOR
            && !matches!(intent, Intent::Promise | Intent::Aborted { .. })
        {
            // Lease-only exemption for a WATCHING fire (review finding on the
            // floor): the watch-idle branch below is unreachable from here, so
            // without this a quota window converts every watching fire into a
            // "continue working" block - killing watch-idle exactly when quota
            // is low - and the claim lease never renews. The wait class itself
            // CANNOT be verified (that needs the reads we are refusing to
            // spend), so this idles on the tag + lease alone, only on a
            // harness that self-wakes, and the message says the state was not
            // verified. The watcher's exit re-evaluates with fresh quota.
            if let Intent::Watching {
                ref reason,
                ref timeout,
                ..
            } = intent
            {
                if harness_can_idle(
                    author_harness.as_deref(),
                    std::env::var("FNO_DRIVER_LIB").is_ok(),
                ) {
                    let window_ms = watch_window_ms(timeout.as_deref());
                    let renewed = match (
                        scan_manifest_field(&manifest_content, "target_claim_key"),
                        scan_manifest_field(&manifest_content, "target_claim_holder"),
                    ) {
                        (Some(key), Some(holder)) => matches!(
                            crate::claims::renew(&key, &holder, window_ms, None),
                            Ok(true)
                        ),
                        _ => false,
                    };
                    if renewed {
                        emit(
                            "loop_check_watch_idle",
                            serde_json::json!({
                                "session_id": session_id,
                                "blocker": "unknown",
                                "declared_timeout": timeout.clone().unwrap_or_default(),
                                "reason": reason,
                                "lease_ms": window_ms,
                                "stand_down": true,
                                "graphql_remaining": q.remaining
                            }),
                        );
                        return (
                            0,
                            allow_output(
                                "allow",
                                None,
                                &format!(
                                    "watching under GraphQL stand-down (remaining {} below \
                                     floor {}): idling until the watcher fires. This fire \
                                     verified NO PR state - the lease is renewed for the \
                                     window and the watcher's exit re-evaluates.",
                                    q.remaining, GRAPHQL_FLOOR
                                ),
                                0,
                                None,
                            ),
                        );
                    }
                    // renewal failed -> never idle without a lease: fall
                    // through to the stand-down block below.
                }
            }
            emit(
                "loop_check",
                serde_json::json!({
                    "session_id": session_id,
                    "decision": "block",
                    "intent": match &intent {
                        Intent::Promise => "promise",
                        Intent::Aborted { .. } => "aborted",
                        Intent::Watching { .. } => "watching",
                        Intent::None => "none",
                    },
                    "intent_source": intent_source,
                    "pr_state": "unknown",
                    "ci": "unknown",
                    "reviewed": false,
                    "standing_down": true,
                    "graphql_remaining": q.remaining,
                    "graphql_floor": GRAPHQL_FLOOR
                }),
            );
            return (
                0,
                allow_output(
                    "block",
                    None,
                    &format!(
                        "standing down: GraphQL remaining {} is below the floor of {} \
                         reserved for the merge guard, so this fire spends no GraphQL - \
                         `gh pr view` / `gh pr checks` are SKIPPED, not retried. \
                         `fno pr status <n>` still answers on the REST budget. The next \
                         fire re-probes; a promise intent always proceeds.",
                        q.remaining, GRAPHQL_FLOOR
                    ),
                    0,
                    None,
                ),
            );
        }
    }

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
                        review_activity_ts(&rv)
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
    let tentative_fp = generic.delivery_fingerprint(make_fingerprint(
        &head_sha,
        fp_pr_state.as_str(),
        &fp_ci.render(),
        &fp_review_ts,
    ));

    // Read prior fires. We pass the tentative_fp for streak counting; if the gh
    // read failed we'll override the fingerprint with the carried-forward value below.
    let min_fire_gap = min_fire_gap_secs();
    let (prior_fires, consecutive_unchanged, last_recorded_fp, streak_window) = read_prior_fires(
        &project_events,
        &session_id,
        &tentative_fp,
        now,
        min_fire_gap,
    );

    // If the pre-read gh call hard-failed, carry forward the prior fingerprint
    // (so the streak continues) rather than resetting to "none|none|none".
    let fingerprint = if fp_read_failed && !generic.is_active() {
        last_recorded_fp.unwrap_or(tentative_fp)
    } else {
        tentative_fp
    };

    // Recount consecutive streak with the (possibly carried-forward) fingerprint.
    // We already counted against the tentative_fp; if different, recount from the log.
    let (consecutive_unchanged, streak_window) = if fp_read_failed && !generic.is_active() {
        // Re-read the streak against the carried-forward fingerprint.
        let (_, streak, _, window) = read_prior_fires(
            &project_events,
            &session_id,
            &fingerprint,
            now,
            min_fire_gap,
        );
        (streak, window)
    } else {
        (consecutive_unchanged, streak_window)
    };

    let this_fire = prior_fires + 1;
    // The harness caps consecutive Stop-hook blocks at
    // CLAUDE_CODE_STOP_HOOK_BLOCK_CAP (Claude Code default 9) and force-ends the
    // turn once it binds (x-1680). Record the resolved cap on the first fire of
    // a session so a run ended by the harness override (last events are blocks
    // whose running consecutive count meets the cap, then silence) is
    // distinguishable from one ended by budget (a terminal budget decision).
    let (block_cap, block_cap_source) = match std::env::var("CLAUDE_CODE_STOP_HOOK_BLOCK_CAP") {
        Ok(v) => (v.trim().parse::<u64>().unwrap_or(9), "env"),
        Err(_) => (9, "default"),
    };
    if prior_fires == 0 {
        emit(
            "loop_check_config",
            serde_json::json!({
                "session_id": session_id,
                "block_cap": block_cap,
                "block_cap_source": block_cap_source,
            }),
        );
    }
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

    // Run done() on active generic delivery, intent, backstop, or mute-probe; malformed findings cannot block.
    if generic.is_active()
        || intent != Intent::None
        || backstop_tripped
        || consecutive_after >= MUTE_PROBE_N
    {
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
                    "streak_window_secs": streak_window,
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
        // (DonePlanned / DoneAdvisory / DoneDelivery / DoneBatched / DonePRGreen) until an
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
                    "streak_window_secs": streak_window,
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

        if let Some(output) = crate::delivery_completion::gate_output(
            &generic,
            intent == Intent::Promise,
            &project_events,
            &global_events,
            &session_id,
            manifest.session_id.as_deref(),
            node_id.as_deref(),
            intent_source,
            &fingerprint,
            this_fire,
            backstop_tripped,
            consecutive_after,
            streak_window,
            fp_pr_state.as_str(),
            &fp_ci.render(),
        ) {
            return (0, output);
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
                    "streak_window_secs": streak_window,
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
                    "streak_window_secs": streak_window,
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
                    "streak_window_secs": streak_window,
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
            git_bin,
            &cwd,
            settings.ci_declared_none,
            manifest.no_external,
            &required_bots,
            &optional_bots,
            &settings.external_reviewers,
            &required_reviewers,
            &nudge_configs,
            &head_sha,
            &project_events,
            &global_events,
            &repo_slug,
            manifest.harness_session_id.as_deref(),
        );

        match done_result {
            Ok(mut pr_info) => {
                // Read 4's newest activity timestamp folds into the
                // fingerprint's 4th component: a late inline finding advances
                // the fingerprint (re-block, not NoProgress - the codex
                // findings-minutes-after-summary shape). State/CI components
                // stay on the pre-read basis so quiet fires stay comparable.
                // Skipped entirely when the pre-read failed: its stale
                // none|none components would leak into done_fp and manufacture
                // a fingerprint change on a fire US4 declares transparent
                // (sigma-review finding on this branch).
                let (fingerprint, consecutive_after, streak_window) = if !fp_read_failed {
                    let done_fp = make_fingerprint(
                        &head_sha,
                        fp_pr_state.as_str(),
                        &fp_ci.render(),
                        &max_ts(&fp_review_ts, &pr_info.latest_review_ts),
                    );
                    if done_fp != fingerprint {
                        let (_, streak, _, window) = read_prior_fires(
                            &project_events,
                            &session_id,
                            &done_fp,
                            now,
                            min_fire_gap,
                        );
                        (done_fp, streak + 1, window)
                    } else {
                        (fingerprint, consecutive_after, streak_window)
                    }
                } else {
                    (fingerprint, consecutive_after, streak_window)
                };
                let backstop_tripped = consecutive_after >= backstop_n;

                // x-b167 section 5: post the trigger for any NeedsNudge bot ONCE,
                // then treat it as Awaiting for this fire's messaging + idle read.
                // A NeedsNudge state means !reviewed, so no terminal below can
                // fire (they require reviewed=true); posting here is safe. A
                // failed post keeps NeedsNudge so the block message tells the
                // agent to post by hand (AC11) and the count is unchanged - a
                // failed post is never counted as a nudge.
                let nudge_pr_number = pr_info.number;
                for n in pr_info.bot_nudges.iter_mut() {
                    if n.class != NudgeClass::NeedsNudge {
                        continue;
                    }
                    if post_nudge_comment(gh_bin, &cwd, nudge_pr_number, &n.review_handle) {
                        emit(
                            "loop_check_nudge_posted",
                            serde_json::json!({
                                "session_id": session_id,
                                "pr": nudge_pr_number,
                                "bot": n.login,
                                "handle": n.review_handle,
                                "nudge": n.nudges + 1,
                                "ceiling": n.ceiling
                            }),
                        );
                        n.nudges += 1;
                        n.newest_age_min = 0;
                        n.class = NudgeClass::Awaiting;
                    } else {
                        emit(
                            "loop_check_nudge_post_failed",
                            serde_json::json!({
                                "session_id": session_id,
                                "pr": nudge_pr_number,
                                "bot": n.login,
                                "handle": n.review_handle
                            }),
                        );
                    }
                }

                let ci_ok = pr_info.ci_conclusion.is_ok();
                let pr_open = pr_info.state.is_open_or_merged();
                // codex P1 on #447: a green PR must also contain the local
                // HEAD - otherwise unpushed work terminates as DonePRGreen
                // without ever shipping. MERGED PRs are exempt only when the
                // local HEAD matches too; an unpushed commit on top of a
                // merged PR is still unshipped work.
                let head_shipped = !pr_info.head_oid.is_empty() && pr_info.head_oid == head_sha;

                // done_probes: the FINAL DonePRGreen conjunct. Gated on
                // every other conjunct already holding, so a plan with no probes
                // spawns no subprocess and a red/unreviewed PR never pays for one.
                let (mut probe_block, mut probe_results) = (None, Value::Null);
                if pr_open && ci_ok && pr_info.reviewed && head_shipped {
                    match evaluate_done_probes(
                        manifest.plan_path.as_deref(),
                        settings.done_probes.as_ref(),
                        &cwd,
                        &project_events,
                        &session_id,
                        PROBE_TIMEOUT,
                    ) {
                        ProbeGate::Absent => {}
                        ProbeGate::Pass(results) => probe_results = results,
                        ProbeGate::Fail { reason, results } => {
                            probe_block = Some(reason);
                            probe_results = results;
                        }
                    }
                }

                // plan fidelity (x-cbab): the stop-gate half of AC5. A plan whose
                // declared deliverables did not all ship blocks DonePRGreen until
                // each shortfall carries a carveout - the agent files one and the
                // next eval passes. Gated on the same conjuncts as done_probes and
                // fail-open on a stale/missing fno (the merge gate is the backstop).
                let mut fidelity_block: Option<String> = None;
                if pr_open && ci_ok && pr_info.reviewed && head_shipped {
                    let fno_bin =
                        std::env::var_os("FNO_LOOPCHECK_FNO_BIN").unwrap_or_else(|| "fno".into());
                    match evaluate_plan_fidelity(manifest.plan_path.as_deref(), &fno_bin, &cwd) {
                        FidelityGate::Refused { reason } => fidelity_block = Some(reason),
                        _ => {}
                    }
                }

                let (reviewed, probes_passed) = (
                    pr_info.reviewed,
                    probe_block.is_none() && fidelity_block.is_none(),
                );
                if pr_passes(pr_open, ci_ok, reviewed, head_shipped, probes_passed) {
                    // Coverage gate (x-0eaf): the three pr_passes conjuncts all ask
                    // "did anyone object"; coverage asks "did anyone review". A
                    // passing PR nothing reviewed terminates DoneUnreviewed, not
                    // DonePRGreen - terminal on first eval (no PR #214 wedge),
                    // never a ship reason (never arms auto-merge). The
                    // discriminator is coverage, NOT the `attended` manifest field
                    // (x-be78: that field lies for spawned workers). A MERGED PR
                    // is exempt: the merge (human out-of-band, or an earlier
                    // autonomous arm) is the terminal authority (x-8b64), and
                    // loop-check must not re-litigate review on an already-merged
                    // PR - the coverage fix prevents the autonomous MERGE (arming),
                    // not the post-merge terminal.
                    if review_required
                        && pr_info.state != PrState::Merged
                        && !pr_info.coverage.coverage.is_covered()
                    {
                        let cov_line = coverage_receipt_line(&pr_info.coverage);
                        let done_msg = format!(
                            "PR #{} is green but UNREVIEWED - {}. Not mergeable by the autonomous path (DoneUnreviewed); merge by hand or after a review.",
                            pr_info.number, cov_line
                        );
                        emit(
                            "termination",
                            serde_json::json!({
                                "session_id": session_id,
                                "reason": "DoneUnreviewed",
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
                                "streak_window_secs": streak_window,
                                "decision": "allow",
                                "intent": if intent == Intent::Promise { "promise" } else { "backstop" },
                                "intent_source": intent_source,
                                "pr_state": pr_info.state.as_str(),
                                "ci": pr_info.ci_conclusion.render(),
                                "reviewed": pr_info.reviewed,
                                "review_skipped": pr_info.review_skipped,
                                "unaddressed_blocking": pr_info.unaddressed_findings.len(),
                                "coverage": coverage_event_data(pr_info.number, &pr_info.coverage, &head_sha, &repo_slug, manifest.harness_session_id.as_deref()),
                                "done_probes": probe_results,
                                "fp_read_failed": fp_read_failed,
                            }),
                        );
                        return (
                            0,
                            allow_output(
                                "allow",
                                Some(TerminationReason::DoneUnreviewed),
                                &done_msg,
                                this_fire,
                                Some(fingerprint),
                            ),
                        );
                    }
                    // A rate-limited bot now fails the gate closed (x-9ab2), so a
                    // green+reviewed DonePRGreen can never carry one: reaching
                    // here means every required bot has a real completed pass.
                    let done_msg = format!("PR #{} is green and reviewed", pr_info.number);
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
                            "streak_window_secs": streak_window,
                            "decision": "allow",
                            "intent": if intent == Intent::Promise { "promise" } else { "backstop" },
                            "intent_source": intent_source,
                            "pr_state": pr_info.state.as_str(),
                            "ci": pr_info.ci_conclusion.render(),
                            "reviewed": pr_info.reviewed,
                            "review_skipped": pr_info.review_skipped,
                            "unaddressed_blocking": pr_info.unaddressed_findings.len(),
                            "fp_read_failed": fp_read_failed,
                            "done_probes": probe_results
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
                                        "streak_window_secs": streak_window,
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

                // x-9ab2: a required bot posted only a usage-limit (quota)
                // comment, so `reviewed` is false and the gate must NOT merge on
                // it. The agent cannot make a rate-limited bot recover, so a hold
                // would wedge to budget death (the PR #214 shape this replaces);
                // terminate cleanly instead. Terminal but NOT a ship reason
                // (mirrors DoneAwaitingMerge): a human merges after a real review,
                // or the operator re-runs once quota recovers / a local review
                // posts, then out-of-band-merge reconcile closes the node.
                // This sits ABOVE every hold that handles a finding or a
                // still-pending bot, so `awaiting_review_only` is load-bearing:
                // anything looser parks work the agent should still be doing.
                if pr_open && ci_ok && head_shipped && awaiting_review_only(&pr_info) {
                    let bots = pr_info.usage_limited.join(", ");
                    let msg = format!(
                        "PR #{} is green and shipped, but required review bot(s) {} posted a usage-limit (quota) comment instead of a review; the review gate cannot be auto-satisfied. Wait for quota recovery or run a local review, then re-run; or merge manually after a real review.",
                        pr_info.number, bots
                    );
                    emit(
                        "termination",
                        serde_json::json!({
                            "session_id": session_id,
                            "reason": "DoneAwaitingReview",
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
                            "streak_window_secs": streak_window,
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
                            "PR #{} blocked - required review bot rate-limited",
                            pr_info.number
                        ),
                        &msg,
                    );
                    return (
                        0,
                        allow_output(
                            "allow",
                            Some(TerminationReason::DoneAwaitingReview),
                            &msg,
                            this_fire,
                            Some(fingerprint),
                        ),
                    );
                }

                // ── Watching idle-allow (x-e2c8) ─────────────────────────────
                // A verified async wait (CI pending or awaiting a bot review,
                // head pushed, zero unaddressed findings) plus an agent-armed
                // <watching> tag idles NON-terminally: the harness re-invokes the
                // model when the agent's watcher task exits, so re-blocking every
                // ~90s tick until then is pure no-op overhead. done() and every
                // terminal above already ran (a terminal always beats an idle),
                // and this sits BEFORE the NoProgress backstop so a long watched
                // wait degrades to budget/claim-expiry, never a spurious kill.
                if let Intent::Watching {
                    ref reason,
                    ref timeout,
                    ..
                } = intent
                {
                    // Harness + substrate gate: only a Claude session self-wakes
                    // on a background-task exit, and a `fno-agents loop run` child
                    // (FNO_DRIVER_LIB, the same discriminator terminal_stop.rs
                    // uses) exits on allow. codex/gemini keep today's block
                    // behavior until their daemon-consumer waker ships (AC1-EDGE).
                    let blocker = if harness_can_idle(
                        author_harness.as_deref(),
                        std::env::var("FNO_DRIVER_LIB").is_ok(),
                    ) {
                        async_wait_class(&pr_info, &head_sha, open_findings.is_empty())
                    } else {
                        None
                    };
                    if let Some(blocker) = blocker {
                        // Extend the node claim to cover the watch window BEFORE
                        // idling, or the idle opens a dispatcher-stampede gap.
                        // Renewal MUST pass an explicit --ttl (a default refresh
                        // shrinks the lease to 1min) and MUST return Ok(true)
                        // (holder match); anything else blocks (AC3-ERR).
                        let window_ms = watch_window_ms(timeout.as_deref());
                        let renewed = match (
                            scan_manifest_field(&manifest_content, "target_claim_key"),
                            scan_manifest_field(&manifest_content, "target_claim_holder"),
                        ) {
                            (Some(key), Some(holder)) => matches!(
                                crate::claims::renew(&key, &holder, window_ms, None),
                                Ok(true)
                            ),
                            _ => false,
                        };
                        if renewed {
                            emit(
                                "loop_check_watch_idle",
                                serde_json::json!({
                                    "session_id": session_id,
                                    "pr": pr_info.number,
                                    "blocker": blocker,
                                    "declared_timeout": timeout.clone().unwrap_or_default(),
                                    "reason": reason,
                                    "lease_ms": window_ms
                                }),
                            );
                            emit(
                                "loop_check",
                                serde_json::json!({
                                    "session_id": session_id,
                                    "fingerprint": fingerprint,
                                    "fires": this_fire,
                                    "consecutive_unchanged": consecutive_after,
                                    "streak_window_secs": streak_window,
                                    "decision": "allow",
                                    "intent": "watching",
                                    "intent_source": intent_source,
                                    "pr_state": pr_info.state.as_str(),
                                    "ci": pr_info.ci_conclusion.render(),
                                    "reviewed": pr_info.reviewed,
                                    "review_skipped": pr_info.review_skipped,
                                    "fp_read_failed": fp_read_failed
                                }),
                            );
                            let msg = format!(
                                "watching: idling until watcher fires (PR #{}, {blocker} pending)",
                                pr_info.number
                            );
                            return (
                                0,
                                allow_output("allow", None, &msg, this_fire, Some(fingerprint)),
                            );
                        }
                        // renewal failed / holder mismatch -> fall through to the
                        // block below (AC3-ERR): never idle without a lease.
                    }
                    // not async-wait class, or a loop-run child -> fall through:
                    // build_block_reason names the real blocker (AC1-ERR CI red,
                    // AC2-ERR head mismatch / finding).
                }

                // x-b167: a freshly-posted nudge sits in Awaiting until
                // wait_minutes elapses. On a harness that cannot idle on a
                // `<watching>` tag (a loop-run child, codex/gemini, or a failed
                // lease renewal) the fingerprint is stable, so without this guard
                // the generic backstop reaps the wait after backstop_n fires -
                // before the nudge cycle reaches its ceiling, terminating with a
                // generic NoProgress instead of the named give-up. Suppress the
                // backstop ONLY when the sole unmet condition is a live Awaiting
                // nudge: it is self-limiting (Awaiting -> Unresponsive after
                // wait_minutes, when this guard clears and the backstop reaps it
                // naming the bot), and the narrow scope keeps CI red, a finding,
                // an unattested reviewer, or a failed probe tripping it as before.
                let sole_blocker_is_awaiting = pr_open
                    && ci_ok
                    && probe_block.is_none()
                    && !pr_info.reviewed
                    && pr_info.unattested_reviewers.is_empty()
                    && pr_info.unaddressed_findings.is_empty()
                    && pr_info
                        .bot_nudges
                        .iter()
                        .any(|n| n.class == NudgeClass::Awaiting);
                // `probe_block.is_some()` keeps a probe that can never pass in
                // this environment on the NoProgress escape rather than looping
                // to the budget ceiling: PR+CI+review all hold, so without it
                // none of the other disjuncts can ever fire.
                if backstop_tripped
                    && (!pr_open || !ci_ok || !pr_info.reviewed || probe_block.is_some())
                    && !sole_blocker_is_awaiting
                {
                    // Backstop tripped + done() false -> NoProgress. x-b167 AC13:
                    // when a nudged bot never answered, the operator's question is
                    // "is this going to finish, and must I do something" - so name
                    // the bot + nudge count + elapsed instead of a bare fingerprint
                    // streak, and reach the operator (who is not watching the pane)
                    // with exactly one notification.
                    let nudge_giveup = unresponsive_bot(&pr_info);
                    let noprogress_msg = match nudge_giveup {
                        Some(n) => nudge_giveup_message(n),
                        None => format!(
                            "fingerprint unchanged for {} consecutive fires over {}m; PR not done",
                            consecutive_after,
                            streak_window / 60
                        ),
                    };
                    if let Some(n) = nudge_giveup {
                        best_effort_notify(
                            "target: bot review gave up",
                            &format!(
                                "PR #{}: {} did not review after {} nudges over {}m",
                                pr_info.number, n.login, n.nudges, n.span_min
                            ),
                        );
                    }
                    // Backstop tripped + done() false -> NoProgress
                    emit(
                        "termination",
                        serde_json::json!({
                            "session_id": session_id,
                            "reason": "NoProgress",
                            "message": noprogress_msg
                        }),
                    );
                    emit(
                        "loop_check",
                        serde_json::json!({
                            "session_id": session_id,
                            "fingerprint": fingerprint,
                            "fires": this_fire,
                            "consecutive_unchanged": consecutive_after,
                            "streak_window_secs": streak_window,
                            "decision": "allow",
                            "intent": "backstop",
                            "intent_source": intent_source,
                            "pr_state": pr_info.state.as_str(),
                            "ci": pr_info.ci_conclusion.render(),
                            "reviewed": pr_info.reviewed,
                            "review_skipped": pr_info.review_skipped,
                            "unaddressed_blocking": pr_info.unaddressed_findings.len(),
                            "fp_read_failed": fp_read_failed,
                            "done_probes": probe_results
                        }),
                    );
                    let return_msg = match nudge_giveup {
                        Some(_) => noprogress_msg.clone(),
                        None => format!(
                            "fingerprint unchanged for {} fires over {}m; HEAD={}, PR={}, CI={}, reviewed={}",
                            consecutive_after,
                            streak_window / 60,
                            short_sha(&head_sha),
                            pr_info.state.as_str(),
                            pr_info.ci_conclusion.render(),
                            pr_info.reviewed
                        ),
                    };
                    return (
                        0,
                        allow_output(
                            "allow",
                            Some(TerminationReason::NoProgress),
                            &return_msg,
                            this_fire,
                            Some(fingerprint),
                        ),
                    );
                }

                // done() false on promise -> block with named reason. P2
                // (ab-098967b4): enrich with a loop-boundary inbox nudge.
                // A failed probe OR a fidelity refusal IS the blocker when
                // everything else is green; build_block_reason would otherwise
                // report a healthy PR.
                let reason = crate::nudge::append_inbox_nudge(
                    &probe_block
                        .clone()
                        .or(fidelity_block.clone())
                        .unwrap_or_else(|| {
                            build_block_reason(&pr_info, &head_sha, open_findings.is_empty())
                        }),
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
                        "streak_window_secs": streak_window,
                        "decision": "block",
                        "intent": if intent == Intent::Promise { "promise" } else { "none" },
                        "intent_source": intent_source,
                        "pr_state": pr_info.state.as_str(),
                        "ci": pr_info.ci_conclusion.render(),
                        "reviewed": pr_info.reviewed,
                        "review_skipped": pr_info.review_skipped,
                        "unaddressed_blocking": pr_info.unaddressed_findings.len(),
                        "fp_read_failed": fp_read_failed,
                        "done_probes": probe_results
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
                // to the streak. Budget is NOT the sole ceiling during a
                // sustained outage: on Claude Code the harness itself caps
                // consecutive Stop-hook blocks (CLAUDE_CODE_STOP_HOOK_BLOCK_CAP,
                // default 9) and force-ends the turn once it binds - which on
                // the unraised harness happens long before budget, exactly the
                // x-1680 truncation. fno raises the cap for spawned workers
                // (see _mesh_env_wrapper / the bg spawn_env), so its own
                // NoProgress/budget terminals bind first in normal operation;
                // but during a pure gh-read outage the (raised, finite) cap is
                // still the binding ceiling, not budget. AC4-EDGE holds only in
                // the sense that budget is checked before any gh read, so a gh
                // outage alone never makes a session immortal from fno's side.
                // Name the real reason when the quota is the reason.
                // "retrying next fire" is the right advice for a blip and
                // the worst possible advice for an exhausted quota: it burns a
                // fire every tick for the whole reset window on a call that
                // cannot succeed. The probe is REST and primary-exempt, so it
                // still answers while GraphQL is at 0; a failed probe keeps
                // the transient wording rather than guessing.
                // Reuse the fire-start probe; re-probe only if it failed, so a
                // blip at the top still gets its one retry without a second
                // `gh api rate_limit` on every error fire (request-rate cost).
                let quota = quota_probe.or_else(|| probe_graphql_quota(gh_bin, &cwd));
                emit(
                    "loop_check_gh_error",
                    serde_json::json!({
                        "session_id": session_id,
                        "read": failed_read,
                        "stderr_tail": failed_stderr,
                        "graphql_remaining": quota.as_ref().map(|q| q.remaining),
                        "graphql_reset": quota.as_ref().map(|q| q.reset_epoch)
                    }),
                );
                emit(
                    "loop_check",
                    serde_json::json!({
                        "session_id": session_id,
                        "fingerprint": fingerprint,
                        "fires": this_fire,
                        "consecutive_unchanged": consecutive_after,
                        "streak_window_secs": streak_window,
                        "decision": "block",
                        "intent": if intent == Intent::Promise { "promise" } else { "none" },
                        "intent_source": intent_source,
                        "pr_state": "unknown",
                        "ci": "unknown",
                        "reviewed": false,
                        "fp_read_failed": true
                    }),
                );
                let reason = match &quota {
                    Some(q) if q.remaining == 0 => graphql_exhausted_reason(q),
                    _ => format!(
                        "gh read '{failed_read}' failed; retrying next fire. {failed_stderr}"
                    ),
                };
                return (
                    0,
                    allow_output("block", None, &reason, this_fire, Some(fingerprint)),
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
            "streak_window_secs": streak_window,
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
        "continue working; no completion signal. If you are only waiting on an async check (CI/review) with nothing to do, arm a harness-tracked watcher with a hard timeout (e.g. background Bash `i=0; while [ $i -lt 30 ]; do fno pr status <N> 2>/dev/null | grep -q '\"settled\": true' && break; sleep 60; i=$((i+1)); done` - REST, 60s interval, never `gh pr checks --watch`, which spends the shared GraphQL quota) and end your turn with `<watching reason=\"ci|review\" pr=\"<N>\" timeout=\"30m\">` - the session idles until the watcher exits instead of re-waking every tick.",
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
    git_bin: &str,
    cwd: &Path,
    ci_declared_none: bool,
    no_external: bool,
    required_bots: &[String],
    optional_bots: &[String],
    external_reviewers: &[String],
    reviewers: &[String],
    nudge_configs: &[NudgeConfig],
    head_sha: &str,
    events_path: &Path,
    global_events_path: &Path,
    repo_slug: &str,
    author_session: Option<&str>,
) -> Result<PrInfo, (String, String)> {
    read_pr_info(
        gh_bin,
        git_bin,
        cwd,
        ci_declared_none,
        no_external,
        required_bots,
        optional_bots,
        external_reviewers,
        reviewers,
        nudge_configs,
        head_sha,
        events_path,
        global_events_path,
        repo_slug,
        author_session,
        None,
    )
}

/// Slack added beyond the declared watch window so the claim lease outlives the
/// agent's watcher (x-e2c8): a watcher that fires right at its timeout must not
/// race claim expiry.
const WATCH_SLACK_MS: i64 = 12 * 60_000;

/// Lease window for an idle watch: the declared timeout clamped to [5m, 2h]
/// (never trust the tag for an unbounded hold) plus slack. Defaults to 30m when
/// the tag omits or mangles `timeout`, giving the ~40m default lease.
fn watch_window_ms(timeout: Option<&str>) -> i64 {
    let declared = timeout
        .and_then(crate::claims::parse_ttl_ms)
        .unwrap_or(30 * 60_000);
    declared.clamp(5 * 60_000, 2 * 3_600_000) + WATCH_SLACK_MS
}

/// Whether a session's harness + substrate can park-and-wake on a `<watching>`
/// idle (x-e2c8). Only a Claude session's harness-tracked background/Monitor
/// tasks re-invoke the model when they exit, so only Claude may idle. A
/// `fno-agents loop run` child exits on allow (FNO_DRIVER_LIB set), and
/// codex/gemini have no self-wake on background-task exit - their waker is the
/// fno-agents daemon consuming the watch event, shipped as a separate
/// live-verified follow-up - so all of those keep today's block behavior rather
/// than idling with nothing to wake them (a dead watch). This is the design's
/// "unroutable harness -> status quo, never a dead watch" degradation.
fn harness_can_idle(author_harness: Option<&str>, is_loop_run_child: bool) -> bool {
    author_harness == Some("claude") && !is_loop_run_child
}

/// Whether the PR is in the async-wait class a `<watching>` tag may idle on
/// (x-e2c8): PR open, local HEAD pushed, no unaddressed findings (inline OR
/// operator), and the sole remaining blocker is CI still pending or an
/// outstanding bot review. Returns the blocker label, or None if anything else
/// blocks. External truth only - the tag is a request, this is the authority.
fn async_wait_class(
    pr: &PrInfo,
    local_head: &str,
    open_findings_empty: bool,
) -> Option<&'static str> {
    let head_shipped = !pr.head_oid.is_empty() && pr.head_oid == local_head;
    if pr.state != PrState::Open
        || !head_shipped
        || !pr.unaddressed_findings.is_empty()
        || !open_findings_empty
    {
        return None;
    }
    // CI still pending AND nothing has concluded red yet: idle on CI. If a
    // check has ALREADY failed while others run, do NOT idle - the agent should
    // start debugging the failure now rather than wait out the rest (gemini).
    if pr.ci_has_pending && !matches!(pr.ci_conclusion, CiConclusion::Failure(_)) {
        return Some("ci");
    }
    // Awaiting an EXTERNAL bot review: a real GitHub login WILL post it, so
    // idling until it does is correct. `reviewed == false` with an EMPTY
    // missing_bots is instead a LOCAL-attestation gate (config.review.reviewers,
    // e.g. sigma) or an unaddressed finding - work the agent must DO, and no
    // GitHub reviewer will ever appear to wake it, so idling would park the
    // session forever. Require an outstanding bot (codex P1).
    //
    // An outstanding LOCAL reviewer disqualifies the wait even when a bot is
    // also outstanding (codex review of x-cdc7): the session has work it can do
    // right now, and if the bot never posts, idling means that work never
    // happens and the run dies on budget with the gate still unmet.
    //
    // x-b167: idle ONLY when every missing bot is in an idlable nudge state
    // (Awaiting, a genuine async wait; or NotNudgeable, today's status quo). A
    // NeedsNudge bot is work to DO (post its trigger) and an Unresponsive bot is
    // a wait nobody ends - idling on either parks the session. This is the same
    // rule x-cdc7 gave unattested_reviewers. An empty bot_nudges (not classified)
    // means every-bot-idlable vacuously, preserving pre-x-b167 behavior.
    if pr.ci_conclusion.is_ok()
        && !pr.reviewed
        && !pr.review_skipped
        && !pr.missing_bots.is_empty()
        && pr.unattested_reviewers.is_empty()
        && pr.bot_nudges.iter().all(|n| nudge_class_idlable(&n.class))
    {
        return Some("review");
    }
    None
}

/// First 8 chars of a sha, never bytes. `&s[..8]` panics when byte offset 8
/// lands inside a multibyte character, and one of these strings comes from a
/// user-writable events.jsonl - a panic there takes the whole stop gate down.
fn short_sha(s: &str) -> String {
    s.chars().take(8).collect()
}

/// The arm-and-tag ritual (x-e2c8, US3) that converts an unwatched async wait
/// into a single idle turn. Supersedes the old "wait silently" prose: waiting
/// silently still costs a full model invocation every ~90s tick, whereas arming
/// a harness-tracked watcher and emitting `<watching>` idles the session to ZERO
/// invocations until the watcher fires. The `gh pr checks` shape is a template
/// (gh's `--watch` exit varies by version); the design depends only on the task
/// EXITING, never on its exit code.
///
/// The bound uses shell builtins, never `timeout(1)`: stock macOS has neither
/// it nor `gtimeout`, so naming it makes the watcher no-op and the session idle
/// forever on a wait that never started. The watchdog is reaped once the wait
/// returns - left alive, it wakes 30m later and kills whatever now holds that
/// recycled pid (codex P1).
fn arm_watch_hint(pr_number: i64, blocker: &str) -> String {
    // The watcher must WAIT on the actual blocker (codex P2): a review wait
    // has CI already green, so a checks watcher returns instantly and the
    // session just re-blocks. Both recipes poll on REST at a 60s interval:
    // `gh pr checks --watch` / `gh pr view` are GraphQL, and
    // a fleet of 60s GraphQL watchers is exactly what exhausts the per-USER
    // quota the merge guard needs. The CI recipe greps for the POSITIVE
    // settled marker, never for an absence: a rate-limited read answers
    // `settled: false`, so an exhausted window keeps the watcher waiting
    // instead of reading as "nothing pending".
    let watcher = if blocker == "review" {
        format!(
            "background Bash `r=$(git config --get remote.origin.url | sed -E 's#.*github.com[:/]##; s#\\.git$##'); n=$(gh api \"repos/$r/pulls/{pr_number}/reviews?per_page=100\" --jq length); i=0; while [ $i -lt 30 ]; do sleep 60; [ \"$(gh api \"repos/$r/pulls/{pr_number}/reviews?per_page=100\" --jq length)\" -gt \"$n\" ] && break; i=$((i+1)); done` (wakes when a new review posts, or after ~30m; per_page=100 because gh api fetches ONE page - at the default 30 the count saturates and a 31st review never wakes it)"
        )
    } else {
        format!(
            "background Bash `i=0; while [ $i -lt 30 ]; do fno pr status {pr_number} 2>/dev/null | grep -q '\"settled\": true' && break; sleep 60; i=$((i+1)); done` (wakes when CI settles - green or red - or after ~30m)"
        )
    };
    format!(
        " Arm a harness-tracked watcher with a hard timeout (e.g. {watcher}), then end your turn with `<watching reason=\"{blocker}\" pr=\"{pr_number}\" timeout=\"30m\">` and nothing else - the session then idles until the watcher exits."
    )
}

// ── done_probes ──────────────────────────────────────────────────────
//
// A plan may declare `done_probes` in its frontmatter: runnable commands whose
// success is the operational evidence that the shipped thing actually RUNS.
// DonePRGreen measures artifacts (PR + CI + review), which operational silence
// cannot falsify - grooming shipped three times without ever running. Probes are
// the enforcement arm: the gate refuses done until the declared observation
// holds, forcing the session to perform the last mile before claiming done.

/// Wall-clock ceiling per probe. The host has no `timeout` binary (and no
/// gtimeout), so the bound is native: spawn, poll `try_wait`, kill.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A probe list is a gate, not a test suite.
const PROBE_CAP: usize = 3;

/// Probe stderr is quoted back in the block reason so the agent knows which
/// last-mile action to perform; cap it so the reason stays readable.
const PROBE_STDERR_CAP: usize = 500;

enum ProbeOutcome {
    Pass,
    Fail { code: Option<i32>, stderr: String },
    Timeout,
}

impl ProbeOutcome {
    /// Event rendering: `pass` | `fail:<code>` | `timeout`.
    fn render(&self) -> String {
        match self {
            ProbeOutcome::Pass => "pass".to_string(),
            ProbeOutcome::Fail { code: Some(c), .. } => format!("fail:{c}"),
            ProbeOutcome::Fail { code: None, .. } => "fail:signal".to_string(),
            ProbeOutcome::Timeout => "timeout".to_string(),
        }
    }
}

enum ProbeGate {
    /// No declaration: zero subprocesses, gate behavior byte-identical to before.
    Absent,
    Pass(Value),
    Fail {
        reason: String,
        results: Value,
    },
}

/// Plan-fidelity stop gate (x-cbab). The stop-gate half of AC5; the merge gate
/// (`_merge.py`, which imports the core in-process) is the other. Shells
/// `fno plan fidelity --json <plan_path>` and blocks DonePRGreen when a planned
/// deliverable is unjoined and uncovered by a carveout - the agent must file a
/// carveout before it may stop. Mirrors `ProbeGate`'s shape deliberately.
#[derive(Debug)]
enum FidelityGate {
    /// No plan, or the probe degraded. A missing/stale `fno` (one without the
    /// `plan fidelity` verb) must NOT wedge the stop gate - the merge gate is the
    /// backstop, and `fno doctor` flags the staleness. Fail open here.
    Absent,
    Pass,
    Refused {
        reason: String,
    },
}

/// Run `fno plan fidelity --json` for the bound plan and classify the decision.
///
/// Fail-open on every error path (no fno, non-zero exit, unparseable JSON): the
/// stop gate must not block on a broken probe. The inversion lives in the Python
/// core (`fno.plan.fidelity`); Rust only reads the `refused` bool, so there is
/// one implementation of the join and the gate and the loop cannot drift.
/// `fno_bin` is resolved by the caller (from `FNO_LOOPCHECK_FNO_BIN`, default
/// `fno`) so this function is hermetically testable with a stub script.
fn evaluate_plan_fidelity(plan_path: Option<&str>, fno_bin: &OsStr, cwd: &Path) -> FidelityGate {
    let plan = match plan_path {
        Some(p) if !p.is_empty() => p,
        _ => return FidelityGate::Absent,
    };
    let out = match Command::new(fno_bin)
        .args(["plan", "fidelity", plan, "--json"])
        .current_dir(cwd)
        .output()
    {
        Ok(o) => o,
        Err(_) => return FidelityGate::Absent,
    };
    classify_plan_fidelity(&out.stdout)
}

fn classify_plan_fidelity(stdout: &[u8]) -> FidelityGate {
    let v: Value = match serde_json::from_slice(stdout) {
        Ok(v) => v,
        Err(_) => return FidelityGate::Absent,
    };
    match v.get("refused").and_then(|r| r.as_bool()) {
        Some(true) => FidelityGate::Refused {
            reason: v
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("plan has unjoined deliverables with no covering carveout")
                .to_string(),
        },
        _ => FidelityGate::Pass,
    }
}

/// Unwrap a YAML scalar to the string a YAML parser would produce.
///
/// Decoding escapes is not cosmetic: the recommended block form routinely
/// carries an inner quote (`- "test -n \"$(cmd)\""`). Leaving the backslashes in
/// would hand `sh -c` literal `\"` characters - a DIFFERENT command than the
/// plan declared, whose result the gate would then trust - and would also key
/// the event by a string the PyYAML-side grader never matches.
fn unquote_scalar(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let inner = &s[1..s.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                // `\"`, `\\`, `\/` and anything else: keep the escaped char.
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        }
        return out;
    }
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        // YAML single-quoted scalars escape only the quote, by doubling it.
        return s[1..s.len() - 1].replace("''", "'");
    }
    s.to_string()
}

/// Split a YAML inline list body. Quoted segments win over comma-splitting
/// because probe commands routinely contain commas (`--jq '.a,.b'`); only an
/// unquoted body falls back to a naive split.
fn split_inline_list(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' || c == '\'' {
            let mut item = String::new();
            let mut escaped = false;
            for c2 in chars.by_ref() {
                if escaped {
                    item.push(c2);
                    escaped = false;
                } else if c2 == '\\' {
                    escaped = true;
                } else if c2 == c {
                    break;
                } else {
                    item.push(c2);
                }
            }
            out.push(item);
        }
    }
    if out.is_empty() {
        out = body
            .split(',')
            .map(unquote_scalar)
            .filter(|s| !s.is_empty())
            .collect();
    }
    out
}

/// What a plan doc's frontmatter says about `done_probes`.
#[derive(Debug, PartialEq)]
enum ProbeDecl {
    /// No `done_probes` key, or explicitly `[]` - both mean "no gate".
    None,
    Probes(Vec<String>),
    /// The key is present but no probes could be recovered from it. This is
    /// NEVER treated as "no probes": a declaration this parser cannot read is
    /// the vacuous-pass shape the whole feature exists to prevent, so it fails
    /// closed and asks a human to look.
    Unparseable,
}

/// Read `done_probes` from a plan doc's frontmatter. Accepts the block form
/// (`done_probes:\n  - "cmd"`) and the single-line inline form
/// (`done_probes: ["cmd"]`); anything else declared is `Unparseable`.
fn parse_done_probes(content: &str) -> ProbeDecl {
    parse_probes_for(content, "done_probes")
}

/// Key-parameterized probe-list parser. `done_probes` (loop-check's
/// session-termination gate) and `close_probes` (the close verbs' node-closure
/// gate) share one parser so the two gates cannot drift on what counts as a
/// declared probe list. Anything else declared under `key` is `Unparseable`.
fn parse_probes_for(content: &str, key: &str) -> ProbeDecl {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return ProbeDecl::None;
    }
    let after_first = &content[3..];
    let Some(end) = after_first.find("\n---") else {
        return ProbeDecl::None;
    };

    let key_prefix = format!("{key}:");
    let mut out = Vec::new();
    let mut declared = false;
    let mut in_block = false;
    for line in after_first[..end].lines() {
        let trimmed = line.trim();
        if !in_block {
            let Some(rest) = trimmed.strip_prefix(&key_prefix) else {
                continue;
            };
            declared = true;
            let rest = rest.trim();
            if rest == "[]" {
                return ProbeDecl::None;
            }
            if let Some(inner) = rest.strip_prefix('[') {
                // strip_suffix, not trim_end_matches: the latter eats EVERY
                // trailing ']' (mangling a command that ends in one) and would
                // silently accept an unterminated list.
                let Some(inner) = inner.strip_suffix(']') else {
                    return ProbeDecl::Unparseable;
                };
                let items = split_inline_list(inner);
                // An empty result means a multi-line inline list (items live on
                // following lines) - unrecoverable here, so refuse rather than
                // report the declaration as absent.
                return if items.is_empty() {
                    ProbeDecl::Unparseable
                } else {
                    ProbeDecl::Probes(items)
                };
            }
            // A plain scalar (`close_probes: cmd`) is ONE probe. The plan
            // schema advertises `str | list`, so refusing the scalar here made
            // a documented-legal declaration an unevaluable gate (a hard
            // refusal at the close verbs). A YAML block scalar (`|` / `>`) puts
            // the value on the following lines and is still unreadable here.
            if !rest.is_empty() {
                if rest.starts_with('|') || rest.starts_with('>') {
                    return ProbeDecl::Unparseable;
                }
                let item = unquote_scalar(rest);
                return if item.is_empty() {
                    ProbeDecl::Unparseable
                } else {
                    ProbeDecl::Probes(vec![item])
                };
            }
            in_block = true;
            continue;
        }
        // Inside the block: a comment is not the end of it (treating one as a
        // terminator would silently drop every probe below it).
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(item) = trimmed.strip_prefix("- ") else {
            break; // the next frontmatter key ends the block
        };
        let item = unquote_scalar(item);
        if !item.is_empty() {
            out.push(item);
        }
    }

    match (declared, out.is_empty()) {
        (false, _) => ProbeDecl::None,
        (true, true) => ProbeDecl::Unparseable,
        (true, false) => ProbeDecl::Probes(out),
    }
}

/// Keep at most the LAST `cap` bytes, without splitting a UTF-8 character.
///
/// The tail, not the head: a failing command's real error is almost always its
/// last line, so keeping the prefix would routinely drop the one diagnostic the
/// block reason exists to surface. Char-boundary aware because `String::drain`
/// and `truncate` panic mid-character, and probe stderr regularly carries
/// arrows, box-drawing, and accented words.
fn keep_last_on_char_boundary(s: &mut String, cap: usize) {
    if s.len() <= cap {
        return;
    }
    let start = s.len() - cap;
    let cut = (start..=s.len())
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(s.len());
    s.drain(..cut);
}

/// SIGKILL a process group, ignoring "already gone".
fn killpg(pgid: i32) {
    if pgid <= 0 {
        return;
    }
    // SAFETY: pgid is our own spawned group leader's pid; ESRCH is expected
    // once every member has exited and is deliberately ignored.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

/// Run one probe under a native timeout.
///
/// Two things here are load-bearing rather than defensive. stderr is drained by
/// a reader thread because reading a piped stderr only after exit deadlocks any
/// probe that writes past the pipe buffer. And the child leads its own process
/// group, which is killed on EVERY exit path - not just the timeout.
///
/// The group kill has to cover normal exit too, because `sh` is not the only
/// process holding the stderr write end. A pipeline (`... | grep -q x`) forks,
/// and a probe that backgrounds anything (`sleep 3600 &`, or any command that
/// daemonizes) lets `sh` exit IMMEDIATELY while the descendant keeps the pipe
/// open. `try_wait` then reports success and leaves the timeout loop, so the
/// timer is never consulted again and the drain join blocks for the
/// descendant's whole lifetime - wedging the stop hook well past the 60s the
/// gate promises. Killing the group closes the pipe and bounds the join.
fn run_probe(cmd: &str, cwd: &Path, timeout: std::time::Duration) -> ProbeOutcome {
    use std::os::unix::process::CommandExt;

    let spawned = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn();

    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            return ProbeOutcome::Fail {
                code: Some(127),
                stderr: format!("probe spawn failed: {e}"),
            }
        }
    };

    // Capture the pgid before any wait() can reap the leader.
    let pgid = child.id() as i32;

    let mut pipe = child.stderr.take();
    let drain = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(ref mut p) = pipe {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });

    let start = std::time::Instant::now();
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break if status.success() {
                    ProbeOutcome::Pass
                } else {
                    ProbeOutcome::Fail {
                        code: status.code(),
                        stderr: String::new(),
                    }
                };
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    kill_process_group(&mut child);
                    break ProbeOutcome::Timeout;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                kill_process_group(&mut child);
                break ProbeOutcome::Fail {
                    code: None,
                    stderr: format!("probe wait failed: {e}"),
                };
            }
        }
    };

    // Reap any descendant still holding the stderr write end, so the drain sees
    // EOF. Without this a backgrounding probe blocks the join indefinitely even
    // though the shell itself exited cleanly.
    killpg(pgid);

    // On timeout the stderr tail is worthless (the reason names the timeout) and
    // joining risks the very hang we just escaped if anything outlived the group
    // kill. Drop the handle instead: the thread ends when the pipe closes.
    if matches!(outcome, ProbeOutcome::Timeout) {
        return outcome;
    }

    let mut stderr = drain.join().unwrap_or_default();
    keep_last_on_char_boundary(&mut stderr, PROBE_STDERR_CAP);
    match outcome {
        ProbeOutcome::Fail { code, stderr: s } if s.is_empty() => {
            ProbeOutcome::Fail { code, stderr }
        }
        other => other,
    }
}

/// SIGKILL the child's whole process group, then reap it. A probe pipeline's
/// grandchildren hold the stderr pipe open; killing only the direct child would
/// leave the drain thread blocked on a pipe that never reaches EOF.
fn kill_process_group(child: &mut std::process::Child) {
    killpg(child.id() as i32);
    let _ = child.kill();
    let _ = child.wait();
}

/// Event payload for a refusal where probes were DECLARED but none ran (plan
/// unreadable, unparseable, over cap). It must be a non-empty object: recording
/// a bare null would make the refusal invisible to `prior_fires_declared_probes`,
/// so a plan that tripped the cap and then went missing would silently degrade
/// to "no gate" - the exact fail-open this records history to prevent. The key
/// is underscore-prefixed so it cannot collide with a probe command string.
fn undeterminable_marker(cause: &str) -> Value {
    serde_json::json!({ "_undeterminable": cause })
}

/// True when any prior loop_check fire for this session recorded probe results.
/// Used to fail closed on an unreadable plan only when probes are known to have
/// existed - a probe-less session with a stale plan_path keeps today's behavior.
fn prior_fires_declared_probes(events_path: &Path, session_id: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(events_path) else {
        return false;
    };
    content.lines().any(|line| {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        val.get("type").and_then(|v| v.as_str()) == Some("loop_check")
            && val.pointer("/data/session_id").and_then(|v| v.as_str()) == Some(session_id)
            && val
                .pointer("/data/done_probes")
                .and_then(|v| v.as_object())
                .is_some_and(|m| !m.is_empty())
    })
}

/// Resolve the PLAN source to its probe list, or the gate that must block.
///
/// Split out from `evaluate_done_probes` so the project source can be resolved
/// independently: a plan that declares nothing (or whose doc is missing on a
/// probe-less session) must still let the project's own probes run, which a
/// single early-return-Absent path cannot express.
fn plan_declared_probes(
    plan_path: Option<&str>,
    cwd: &Path,
    events_path: &Path,
    session_id: &str,
) -> Result<Vec<String>, ProbeGate> {
    // Resolve a relative plan_path against the session's cwd, not the process
    // cwd: plan_path is repo-relative in practice, and reading nothing here
    // would degrade to Absent - a silent gate bypass.
    let plan = plan_path.and_then(|p| {
        // A plan_path may carry a `#wave-1`-style fragment; the Python plan
        // readers strip it, and reading the literal name would fail, which on
        // the first fire (no probe history) degrades to Absent - a silent
        // bypass of a gate the plan actually declared.
        let p = Path::new(p.split('#').next().unwrap_or(p));
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        std::fs::read_to_string(abs).ok()
    });
    let Some(plan) = plan else {
        // Fail closed only when probes were observed before; otherwise a stale
        // plan_path on a probe-less session must not start refusing done.
        if prior_fires_declared_probes(events_path, session_id) {
            return Err(ProbeGate::Fail {
                reason: format!(
                    "done_probes undeterminable: plan {} is unreadable but a prior fire declared probes; restore the plan doc",
                    plan_path.unwrap_or("(unset)")
                ),
                results: undeterminable_marker("plan-unreadable"),
            });
        }
        return Ok(Vec::new());
    };

    let probes = match parse_done_probes(&plan) {
        ProbeDecl::None => return Ok(Vec::new()),
        ProbeDecl::Unparseable => {
            return Err(ProbeGate::Fail {
                reason: format!(
                    "done_probes undeterminable: plan {} declares the field but no probe could be read from it (use a block list, or a single-line inline list)",
                    plan_path.unwrap_or("(unset)")
                ),
                results: undeterminable_marker("unparseable-declaration"),
            })
        }
        ProbeDecl::Probes(p) => p,
    };
    if probes.len() > PROBE_CAP {
        return Err(ProbeGate::Fail {
            reason: format!(
                "plan declares {} done_probes; the cap is {PROBE_CAP} per source (a probe list is a gate, not a test suite)",
                probes.len()
            ),
            results: undeterminable_marker("over-cap"),
        });
    }
    Ok(probes)
}

/// Evaluate the probe conjunct across BOTH sources. Called ONLY once every
/// other DonePRGreen conjunct already holds, so probes run at most once per
/// would-be-done fire.
///
/// `config_probes` is the repo-wide `done_probes` off config.toml. Both lists
/// run and both must pass: a plan can ADD guardrails and can never silence the
/// project's, including via an explicit `done_probes: []`. A repo-wide guard a
/// plan doc can switch off is a guard on one of two paths, which is decorative.
fn evaluate_done_probes(
    plan_path: Option<&str>,
    config_probes: Option<&Result<Vec<String>, String>>,
    cwd: &Path,
    events_path: &Path,
    session_id: &str,
    timeout: std::time::Duration,
) -> ProbeGate {
    // The project source resolves first: a declaration this parser cannot read
    // must block before anything runs, in the same vocabulary the plan side
    // uses. A config key that degrades to no-gate is a guardrail that
    // disappears when you typo it.
    let project = match config_probes {
        None => Vec::new(),
        Some(Err(why)) => {
            return ProbeGate::Fail {
                reason: format!(
                    "done_probes undeterminable: config.toml declares `done_probes` but {why}"
                ),
                results: undeterminable_marker("unparseable-config-declaration"),
            }
        }
        Some(Ok(p)) => p.clone(),
    };
    // PROBE_CAP applies PER SOURCE, not to the union. The cap encodes
    // per-declaration discipline ("a gate, not a test suite"); one shared
    // budget would instead make two independent authors compete for one number
    // and let a project's policy eat a plan's operational probes.
    if project.len() > PROBE_CAP {
        return ProbeGate::Fail {
            reason: format!(
                "config.toml declares {} done_probes; the cap is {PROBE_CAP} per source (a probe list is a gate, not a test suite)",
                project.len()
            ),
            results: undeterminable_marker("over-cap"),
        };
    }

    let plan_probes = match plan_declared_probes(plan_path, cwd, events_path, session_id) {
        Ok(p) => p,
        Err(gate) => return gate,
    };

    if project.is_empty() && plan_probes.is_empty() {
        return ProbeGate::Absent;
    }

    let mut results = serde_json::Map::new();
    let mut failures = Vec::new();
    for (source, cmd) in project
        .iter()
        .map(|c| ("project", c))
        .chain(plan_probes.iter().map(|c| ("plan", c)))
    {
        let outcome = run_probe(cmd, cwd, timeout);
        // Keyed by the BARE command: cli/src/fno/scoreboard/fold.py joins this
        // map back to the plan's frontmatter by exact command string, so the
        // source label belongs in the failure reason - which is what the
        // operator reads to know which file to edit - and never in the key.
        results.insert(cmd.clone(), Value::String(outcome.render()));
        match &outcome {
            ProbeOutcome::Pass => {}
            ProbeOutcome::Timeout => failures.push(format!(
                "{source} probe `{cmd}` timed out after {}s (killed)",
                timeout.as_secs()
            )),
            ProbeOutcome::Fail { code, stderr } => {
                let code = code.map(|c| c.to_string()).unwrap_or("signal".to_string());
                let tail = if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", stderr.trim())
                };
                failures.push(format!("{source} probe `{cmd}` exited {code}{tail}"));
            }
        }
    }

    let results = Value::Object(results);
    if failures.is_empty() {
        ProbeGate::Pass(results)
    } else {
        ProbeGate::Fail {
            reason: format!(
                "done_probes failed - the shipped thing has no evidence of running: {}",
                failures.join("; ")
            ),
            results,
        }
    }
}

fn build_block_reason(pr: &PrInfo, local_head: &str, open_findings_empty: bool) -> String {
    // The ONE predicate. A message that prescribes the arm-and-tag ritual for a
    // blocker `async_wait_class` refuses to idle is the code contradicting
    // itself, and a session complied with exactly that roughly ten times on
    // #618. Deriving the hint from the classifier makes the two agree by
    // construction rather than by two hand-kept branch orders.
    let idlable = async_wait_class(pr, local_head, open_findings_empty);
    let hint = |blocker: &str| -> String {
        if idlable == Some(blocker) {
            arm_watch_hint(pr.number, blocker)
        } else {
            String::new()
        }
    };
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
            short_sha(&pr.head_oid),
            short_sha(local_head)
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
            return format!("CI still running on PR #{}.{}", pr.number, hint("ci"));
        }
        let check_name = match &pr.ci_conclusion {
            CiConclusion::Failure(Some(name)) => name.as_str(),
            _ => "CI",
        };
        return format!("CI red on PR #{}: {} failed", pr.number, check_name);
    }

    if !pr.reviewed {
        // Order: work you can do now, cheapest-to-invalidate first, then the
        // async wait. An unaddressed finding leads because addressing it MOVES
        // HEAD, which supersedes any attestation produced before it - naming
        // the reviewer first would make a session run sigma twice. The bot
        // wait comes last: naming only the bot hides the half the session can
        // act on now, and if the bot never posts, the local work never happens
        // and the run dies on budget with the gate still unmet.
        if !pr.unaddressed_findings.is_empty() {
            // AC2-UI: name the specific finding (path:line) and the remedy.
            let f = &pr.unaddressed_findings[0];
            let more = if pr.unaddressed_findings.len() > 1 {
                format!(" [+{} more]", pr.unaddressed_findings.len() - 1)
            } else {
                String::new()
            };
            // x-b167 AC14: "reply in-thread" alone is a half-remedy - a reply
            // that does not address the bot by its full login never reaches it.
            // Name the handle when the finding author is a known bot.
            let reply_to = profile_by_author(&f.author)
                .map(|p| format!(" addressed to {}", p.reply_handle))
                .unwrap_or_default();
            // The silent failure (PR #447, #787): a finding answered with a
            // top-level PR comment reads as unaddressed because this gate only
            // walks in_reply_to_id chains on /pulls/N/comments. The worker
            // sees green CI, its own fix commits, and a loop that will not
            // terminate, with nothing connecting the two. When any unaddressed
            // finding has no reply at all, lead with the mechanism and the
            // exact command - "reply in-thread" alone was misread twice as
            // "I did reply" by workers who had posted top-level comments.
            let no_reply = pr
                .unaddressed_findings
                .iter()
                .filter(|fnd| !fnd.had_reply)
                .count();
            if no_reply > 0 {
                return format!(
                    "PR #{}: {} blocking finding(s) unaddressed, {} with no in-thread reply. \
                     A top-level PR comment is NOT detected - this gate reads in_reply_to_id chains \
                     on /pulls/{}/comments only. Reply in-thread: gh api repos/$OWNER/$REPO/pulls/{}/comments \
                     -F in_reply_to=<id> -f body='Fixed in <sha>: ...' (or wontfix: <reason>). \
                     First: {} {} at {}:{}{}",
                    pr.number,
                    pr.unaddressed_findings.len(),
                    no_reply,
                    pr.number,
                    pr.number,
                    f.author,
                    f.severity,
                    f.path,
                    f.line,
                    more
                );
            }
            return format!(
                "PR #{}: {} {} at {}:{} unaddressed (reply in-thread{} or wontfix:){}",
                pr.number, f.author, f.severity, f.path, f.line, reply_to, more
            );
        }
        if !pr.unattested_reviewers.is_empty() {
            // The branch that was missing (x-cdc7). Without it a local-only
            // reviewers gate fell through to the generic string below and told
            // the session to wait on a bot that was never required.
            //
            // No arm_watch_hint here, deliberately: `async_wait_class` has
            // ALREADY excluded this blocker from idling, because no GitHub
            // reviewer will ever post the attestation and the session would park
            // forever. Emitting the arm-and-tag ritual on a blocker the same
            // file refuses to idle is the code contradicting itself, and a
            // session did comply with it roughly ten times.
            let head = short_sha(local_head);
            // The verb a wedged session is told to run is harness-correct:
            // codex gets `/review`, claude `/code-review`. Resolved here from the
            // ambient author markers (the same `resolve_harness` the gate uses)
            // rather than threaded through every caller, so the 20+ build_block_reason
            // call sites stay single-arg.
            let author_harness = crate::claims::resolve_harness();
            let items: Vec<String> = pr
                .unattested_reviewers
                .iter()
                .map(|r| {
                    // "no attestation" is a lie to a session that ran the
                    // reviewer and got told no; name that case separately.
                    let state = if r.failed_at_head {
                        " (attested at this head, verdict NOT pass)".to_string()
                    } else {
                        match &r.superseded_head {
                            Some(h) => {
                                format!(" (passed at {}, superseded by this head)", short_sha(h))
                            }
                            None => String::new(),
                        }
                    };
                    if r.name == SAME_MODEL_LOCAL_PEER_SENTINEL {
                        return format!(
                            "peer{} -> configure a cross-model peer or routed model",
                            state
                        );
                    }
                    if r.name == LOCAL_PEER_REVIEWER {
                        return format!("peer{} -> run `/fno:review peer --attest`", state);
                    }
                    match reviewer_invocation_for(&r.name, author_harness.as_deref()) {
                        Some((inv, self_cert)) => {
                            let mark = if self_cert {
                                " [self-cert: asserts no review evidence]"
                            } else {
                                ""
                            };
                            // code-review is a native harness verb that emits
                            // no attestation on its own; the session must also
                            // run the emit helper or the gate never clears.
                            // The fno-skill reviewers (sigma, declare) attest
                            // inside their own invocation.
                            let emit_step = if r.name == "code-review" {
                                format!(
                                    ", then `bash skills/review/scripts/emit-attestation.sh {}`",
                                    r.name
                                )
                            } else {
                                String::new()
                            };
                            format!("{}{} -> run `{}`{}{}", r.name, state, inv, emit_step, mark)
                        }
                        None => format!("{}{}", r.name, state),
                    }
                })
                .collect();
            let corrupt = match pr.malformed_attestations {
                0 => String::new(),
                n => format!(" ({n} unparseable attestation line(s) ignored)"),
            };
            return format!(
                "PR #{}: reviewers gate unmet - no head-pinned review_attestation at {} for {}{}. \
                 This is local work to DO, not a wait: no GitHub reviewer posts these, \
                 so do not arm a watcher.",
                pr.number,
                head,
                items.join("; "),
                corrupt
            );
        }
        if !pr.missing_bots.is_empty() {
            // x-b167: render per nudge state. `hint("review")` is derived from
            // async_wait_class, so it is EMPTY for NeedsNudge/Unresponsive (both
            // non-idlable) and PRESENT for Awaiting/NotNudgeable by construction -
            // the arm-and-tag ritual can never appear on a blocker the same file
            // refuses to idle (the contradiction x-cdc7 removed). NeedsNudge and
            // Unresponsive lead because they are work/decisions, not waits.
            if let Some(n) = pr
                .bot_nudges
                .iter()
                .find(|n| n.class == NudgeClass::NeedsNudge)
            {
                return format!(
                    "PR #{}: {} reviews on mention, not on push, and has not been asked. Run:\n  \
                     gh pr comment {} --body \"{}\"\nthen arm a watcher (nudge {} of {}).{}",
                    pr.number,
                    n.login,
                    pr.number,
                    n.review_handle,
                    n.nudges + 1,
                    n.ceiling,
                    hint("review")
                );
            }
            if let Some(n) = pr
                .bot_nudges
                .iter()
                .find(|n| n.class == NudgeClass::Unresponsive)
            {
                return format!(
                    "PR #{}: {} did not review after {} nudges over {}m. Nothing further \
                     will arrive on its own. Either post the review by hand, or move this \
                     login to config.review.optional_apps (honored-if-present, never waited \
                     on). Not a wait: do not arm a watcher.{}",
                    pr.number,
                    n.login,
                    n.nudges,
                    n.span_min,
                    hint("review")
                );
            }
            if let Some(n) = pr
                .bot_nudges
                .iter()
                .find(|n| n.class == NudgeClass::Awaiting)
            {
                return format!(
                    "PR #{}: {} nudged {}m ago ({} of {}), awaiting review.{}",
                    pr.number,
                    n.login,
                    n.newest_age_min,
                    n.nudges,
                    n.ceiling,
                    hint("review")
                );
            }
            // All NotNudgeable (or not classified): today's exact string + hint
            // (AC5 - a non-nudgeable required bot keeps the pre-x-b167 behavior).
            return format!(
                "PR #{}: {} has not reviewed.{}",
                pr.number,
                pr.missing_bots.join(", "),
                hint("review")
            );
        }
        // Reaching here means missing_bots is empty, which `async_wait_class`
        // treats as non-idlable, so this must not teach the arm-and-tag ritual
        // either (the two must never disagree about whether a wait is valid).
        return format!(
            "PR #{} not yet reviewed and no reviewer is outstanding; \
             re-check config.review (required_bots / reviewers) - nothing here will \
             arrive on its own.",
            pr.number
        );
    }

    format!("PR #{} done() returned false (unknown reason)", pr.number)
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

/// `fno-agents review-coverage --cwd <dir> [--pr <n>] [--head <sha>] ...`
/// (x-3a3f). The standalone review_coverage producer.
///
/// The only thing that could WRITE a `review_coverage` event used to be
/// `read_pr_info` under `run_done`, which `decide()` reaches only past a streak
/// counter - so a session with no target manifest (no stop hook at all) could
/// never produce the row the merge gate demands, making that gate unsatisfiable
/// for a shape that can still open a PR. This verb exposes the SAME computation
/// with the SAME resolver (`resolve_review_inputs`) and the SAME emitter
/// (`read_pr_info` itself, untouched) to every path that can reach the gate.
///
/// Read-only against GitHub (nudge posting lives outside `read_pr_info`),
/// append-only against the two event logs. It has NO way to assert coverage
/// without performing the reads: there is no --force, no --assume-covered, and
/// no config key that skips the coverage guard. A caller wanting a green gate
/// must cause a review to exist.
///
/// Exit contract: 0 = a `review_coverage` row was emitted (covered or
/// uncovered, the number says which); 3 = no PR for the selector (nothing to
/// cover); 4 = the gh read failed and the emitted row is `unknown`; 2 = bad
/// arguments. stdout is always one JSON object.
pub fn run_review_coverage(args: &[String]) -> i32 {
    let (code, json) = decide_review_coverage(args);
    println!("{json}");
    code
}

/// Test-friendly variant: (exit_code, json_string) without printing.
pub fn run_review_coverage_capture(args: &[String]) -> (i32, String) {
    decide_review_coverage(args)
}

const REVIEW_COVERAGE_USAGE: &str = "\
usage: fno-agents review-coverage --cwd <dir> [--pr <n>] [--head <sha>] [--session-id <id>]
       [--events <p>] [--global-events <p>] [--settings <p>] [--global-settings <p>]
       [--gh-bin <p>] [--git-bin <p>] [--author-harness <h>]

Computes and emits the review_coverage event for a PR using the exact
resolver and emitter the stop hook uses (resolve_review_inputs +
read_pr_info), so any session that can open a PR can also satisfy the
gate that guards it. Read-only against GitHub, append-only against the
event logs.

There is no way to assert coverage without performing the reads: no
--force, no --assume-covered, no skip key. A caller wanting a green gate
must cause a review to exist.

Manifest-less defaults, both strict: external review reads are ON
(no_external=false - the manifest field can only relax them, so its
absence must not), and the author session is --session-id, else the
harness_session_id scanned from <cwd>/.fno/target-state.md, else none
(the payload then omits self_attested_count rather than report an
unmeasured 0).

Exits: 0 emitted a row; 3 no PR for the selector; 4 gh read failed
(emitted row is unknown); 2 bad arguments.";

fn decide_review_coverage(args: &[String]) -> (i32, String) {
    let args = if args.first().map(|s| s.as_str()) == Some("review-coverage") {
        &args[1..]
    } else {
        args
    };
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return (
            0,
            serde_json::json!({"usage": REVIEW_COVERAGE_USAGE}).to_string(),
        );
    }
    let mut cwd: Option<PathBuf> = None;
    let mut pr: Option<String> = None;
    let mut head: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut events_path: Option<PathBuf> = None;
    let mut global_events_path: Option<PathBuf> = None;
    let mut settings_path: Option<PathBuf> = None;
    let mut global_settings_path: Option<PathBuf> = None;
    let mut gh_bin = std::env::var("FNO_LOOPCHECK_GH_BIN").unwrap_or_else(|_| "gh".to_string());
    let mut git_bin = std::env::var("FNO_LOOPCHECK_GIT_BIN").unwrap_or_else(|_| "git".to_string());
    let mut author_harness_override: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        if let Some(val) = try_flag_value(&args[i], "--cwd", args, &mut i) {
            cwd = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(&args[i], "--pr", args, &mut i) {
            pr = Some(val);
        } else if let Some(val) = try_flag_value(&args[i], "--head", args, &mut i) {
            head = Some(val);
        } else if let Some(val) = try_flag_value(&args[i], "--session-id", args, &mut i) {
            session_id = Some(val);
        } else if let Some(val) = try_flag_value(&args[i], "--events", args, &mut i) {
            events_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(&args[i], "--global-events", args, &mut i) {
            global_events_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(&args[i], "--settings", args, &mut i) {
            settings_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(&args[i], "--global-settings", args, &mut i) {
            global_settings_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(&args[i], "--gh-bin", args, &mut i) {
            gh_bin = val;
        } else if let Some(val) = try_flag_value(&args[i], "--git-bin", args, &mut i) {
            git_bin = val;
        } else if let Some(val) = try_flag_value(&args[i], "--author-harness", args, &mut i) {
            author_harness_override = Some(val);
        } else if args[i].starts_with('-') {
            // Unknown flag (or one missing its value): silently ignoring it is
            // not leniency - a typo'd `--events` would leave events_path=None
            // and append to the REAL logs while the caller believed their
            // scratch path was used. Exit 2, as the usage text promises.
            return (
                2,
                serde_json::json!({"error": format!("unknown or valueless argument: {}", args[i])})
                    .to_string(),
            );
        }
        i += 1;
    }
    let cwd = match cwd {
        Some(c) => c,
        None => {
            return (
                2,
                serde_json::json!({"error": "--cwd is required"}).to_string(),
            )
        }
    };

    let inputs = resolve_review_inputs(
        &cwd,
        events_path.as_deref(),
        global_events_path.as_deref(),
        settings_path.as_deref(),
        global_settings_path.as_deref(),
        author_harness_override.as_deref(),
    );

    // head_sha pins the emitted event to what would actually merge. Default
    // local HEAD; --head overrides with a caller that knows the PR head. A
    // --head sha the local repository does not contain leaves freshness
    // unresolvable, which resolves stale and refuses - the safe direction.
    let head_sha = head.unwrap_or_else(|| git_head_sha(&git_bin, &cwd));

    // Authorship: --session-id, else the manifest's harness_session_id when one
    // exists, else None. None leaves every local verdict's attestation_origin
    // Unknown (the documented fail-open-on-authorship behavior) and the payload
    // OMITS self_attested_count rather than reporting an unmeasured 0. The
    // "null" filter is parse_manifest's own (init writes ${_HARNESS_SESSION:-null}):
    // Some("null") would mislabel every origin other_session and emit the
    // unmeasured-0 self_attested_count the omission exists to prevent.
    let author_session = session_id.or_else(|| {
        std::fs::read_to_string(cwd.join(".fno/target-state.md"))
            .ok()
            .and_then(|content| scan_manifest_field(&content, "harness_session_id"))
            .filter(|s| s != "null")
    });

    match read_pr_info(
        &gh_bin,
        &git_bin,
        &cwd,
        inputs.settings.ci_declared_none,
        // no_external=false: the manifest field can only RELAX external review,
        // so its absence here must not.
        false,
        &inputs.required_bots,
        &inputs.optional_bots,
        &inputs.settings.external_reviewers,
        &inputs.required_reviewers,
        &inputs.nudge_configs,
        &head_sha,
        &inputs.project_events,
        &inputs.global_events,
        &inputs.repo_slug,
        author_session.as_deref(),
        pr.as_deref(),
    ) {
        Ok(pr_info) => {
            if pr_info.number == 0 {
                // PrState::None: no PR for the selector (or the branch). There
                // is nothing to cover and nothing was emitted.
                return (
                    3,
                    serde_json::json!({
                        "coverage": "none",
                        "emitted": false,
                        "reason": "no PR for the selector",
                    })
                    .to_string(),
                );
            }
            // read_pr_info already emitted this exact payload to both logs;
            // print the same object so stdout and the logs agree.
            (
                0,
                coverage_event_data(
                    pr_info.number,
                    &pr_info.coverage,
                    &head_sha,
                    &inputs.repo_slug,
                    author_session.as_deref(),
                )
                .to_string(),
            )
        }
        Err((read, tail)) => {
            // The gh read failed. Emit an unknown row when the PR number is
            // known (--pr was passed - always true for the merge recompute) so
            // downstream readers see the failed read rather than nothing; with
            // no number the row cannot be attributed, so emit nothing.
            let pr_num: i64 = pr.as_deref().and_then(|p| p.parse().ok()).unwrap_or(0);
            if pr_num > 0 {
                let data = coverage_event_data(
                    pr_num,
                    &CoverageReport {
                        coverage: Coverage::Unknown,
                        verdicts: Vec::new(),
                    },
                    &head_sha,
                    &inputs.repo_slug,
                    author_session.as_deref(),
                );
                emit_to_both(
                    &inputs.project_events,
                    &inputs.global_events,
                    "review_coverage",
                    data.clone(),
                );
                return (4, data.to_string());
            }
            // The emitted unknown row above is schema-gated, so the exhaustion
            // diagnosis rides this stdout-only branch (and the stop hook's own
            // block reason); it must not fork the event contract.
            let quota = probe_graphql_quota(&gh_bin, &cwd);
            (
                4,
                serde_json::json!({
                    "error": format!("gh read failed: {read}"),
                    "detail": tail,
                    "emitted": false,
                    "graphql_remaining": quota.as_ref().map(|q| q.remaining),
                    "graphql_exhausted": quota.as_ref().map(|q| q.remaining == 0),
                })
                .to_string(),
            )
        }
    }
}

/// `fno-agents probe-run --plan <path> --key <name> --cwd <root> --json`.
///
/// Evaluates one named probe list (`done_probes` or `close_probes`) from a plan
/// doc and exits 0 when every probe passes. The close verbs shell out to this
/// for `close_probes`, mirroring how `active_backlog` shells out to
/// `fno backlog done`: the node-closure gate and the session-termination gate
/// share ONE runner. The process-group kill, the pipe-buffer drain, and
/// 127-as-failure all live in `run_probe` and are NOT reimplemented here.
///
/// Exit contract: 0 = every probe passed (or none declared under `key`);
/// 1 = at least one probe failed; 2 = undeterminable (plan unreadable,
/// declaration unparseable, over cap). stdout is always one JSON object.
pub fn run_probe_run(args: &[String]) -> i32 {
    let (code, json) = decide_probe_run(args);
    println!("{json}");
    code
}

fn decide_probe_run(args: &[String]) -> (i32, String) {
    let mut plan: Option<String> = None;
    let mut key = String::from("done_probes");
    let mut cwd: Option<String> = None;
    let mut want_json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--plan" => {
                i += 1;
                if i < args.len() {
                    plan = Some(args[i].clone());
                }
            }
            "--key" => {
                i += 1;
                if i < args.len() {
                    key = args[i].clone();
                }
            }
            "--cwd" => {
                i += 1;
                if i < args.len() {
                    cwd = Some(args[i].clone());
                }
            }
            "--json" => want_json = true,
            _ => {}
        }
        i += 1;
    }
    // JSON is always emitted; the flag is accepted so callers stay explicit.
    let _ = want_json;

    let Some(plan_raw) = plan else {
        return probe_run_payload(2, &key, false, vec![], "--plan is required");
    };
    // A plan_path may carry a `#wave-1` fragment; reading the literal name
    // would fail, degrading an asserted gate to absent.
    let plan_clean = plan_raw.split('#').next().unwrap_or(&plan_raw);
    let plan_path = std::path::Path::new(plan_clean);
    let abs = if plan_path.is_absolute() {
        plan_path.to_path_buf()
    } else {
        std::path::Path::new(cwd.as_deref().unwrap_or(".")).join(plan_path)
    };
    let content = match std::fs::read_to_string(&abs) {
        Ok(c) => c,
        Err(e) => {
            return probe_run_payload(
                2,
                &key,
                false,
                vec![],
                &format!("plan {plan_clean} unreadable: {e}"),
            )
        }
    };

    let probes = match parse_probes_for(&content, &key) {
        ProbeDecl::None => return probe_run_payload(0, &key, false, vec![], "no probes declared"),
        ProbeDecl::Unparseable => {
            return probe_run_payload(
                2,
                &key,
                true,
                vec![],
                &format!("`{key}` declared but no probe could be read from it"),
            )
        }
        ProbeDecl::Probes(p) => p,
    };
    if probes.len() > PROBE_CAP {
        return probe_run_payload(
            2,
            &key,
            true,
            vec![],
            &format!("{} probes declared; cap is {PROBE_CAP}", probes.len()),
        );
    }

    let work_dir = std::path::Path::new(cwd.as_deref().unwrap_or("."));
    let mut results: Vec<Value> = Vec::new();
    let mut failed_reason: Option<String> = None;
    for cmd in &probes {
        let outcome = run_probe(cmd, work_dir, PROBE_TIMEOUT);
        let entry = match outcome {
            ProbeOutcome::Pass => serde_json::json!({"cmd": cmd, "outcome": "pass"}),
            ProbeOutcome::Timeout => {
                if failed_reason.is_none() {
                    failed_reason = Some(format!("`{cmd}` timed out"));
                }
                serde_json::json!({"cmd": cmd, "outcome": "timeout"})
            }
            ProbeOutcome::Fail { code, stderr } => {
                let c = code
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "signal".to_string());
                if failed_reason.is_none() {
                    failed_reason = Some(format!("`{cmd}` exited {c}"));
                }
                serde_json::json!({
                    "cmd": cmd,
                    "outcome": format!("fail:{c}"),
                    "code": code,
                    "stderr": stderr,
                })
            }
        };
        results.push(entry);
    }

    let (code, reason) = match failed_reason {
        Some(r) => (1, r),
        None => (0, "every probe passed".to_string()),
    };
    probe_run_payload(code, &key, true, results, &reason)
}

fn probe_run_payload(
    code: i32,
    key: &str,
    declared: bool,
    results: Vec<Value>,
    reason: &str,
) -> (i32, String) {
    let payload = serde_json::json!({
        "key": key,
        "declared": declared,
        "passed": code == 0,
        "results": results,
        "reason": reason,
    });
    (
        code,
        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string()),
    )
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── plan fidelity stop gate (x-cbab) ──────────────────────────────────────
    //
    // Hermetic: classify canned JSON without spawning, and exercise missing
    // process handling separately. Mirrors the merge-gate half (tested in
    // Python); the two readers are independent by design.

    #[test]
    fn plan_fidelity_gate_blocks_an_uncovered_shortfall() {
        match classify_plan_fidelity(br#"{"refused": true, "reason": "1 unjoined, 0 carveouts"}"#) {
            FidelityGate::Refused { reason } => assert!(reason.contains("unjoined")),
            other => panic!("expected Refused, got {:?}", other),
        }
    }

    #[test]
    fn plan_fidelity_gate_passes_when_not_refused() {
        assert!(matches!(
            classify_plan_fidelity(br#"{"refused": false}"#),
            FidelityGate::Pass
        ));
    }

    #[test]
    fn plan_fidelity_gate_absent_without_a_plan() {
        let cwd = std::env::temp_dir();
        let missing = Path::new("/definitely/missing/fno");
        assert!(matches!(
            evaluate_plan_fidelity(None, missing.as_os_str(), &cwd),
            FidelityGate::Absent
        ));
        assert!(matches!(
            evaluate_plan_fidelity(Some(""), missing.as_os_str(), &cwd),
            FidelityGate::Absent
        ));
    }

    #[test]
    fn plan_fidelity_gate_fails_open_on_an_unparseable_or_missing_fno() {
        // A stale fno without the verb prints an error, not JSON. The stop gate
        // must not block on that - the merge gate is the backstop, and fail-open
        // here is what keeps a stale install from wedging every run.
        let cwd = std::env::temp_dir();
        assert!(matches!(
            evaluate_plan_fidelity(
                Some("/x/plan.md"),
                Path::new("/definitely/missing/fno").as_os_str(),
                &cwd
            ),
            FidelityGate::Absent
        ));
        assert!(matches!(
            classify_plan_fidelity(b"No such command: fidelity"),
            FidelityGate::Absent
        ));
    }

    // ── review freshness: the one predicate (x-5b99 / x-62a1) ───────────────

    fn facts(reviewed: Option<&str>, head: Option<&str>, tree: Option<&[&str]>) -> FreshnessFacts {
        FreshnessFacts {
            reviewed_identity: reviewed.map(str::to_string),
            head_identity: head.map(str::to_string),
            tree_paths: tree.map(|p| p.iter().map(|s| s.to_string()).collect()),
        }
    }

    #[test]
    fn freshness_same_sha_is_fresh() {
        // No git facts needed at all: the reviewer read this exact commit.
        assert_eq!(
            review_freshness("abc123", "abc123", &FreshnessFacts::default()),
            Freshness::Fresh
        );
    }

    #[test]
    fn freshness_base_sync_carries() {
        // PR 829's specimen: a 153-file rebase whose PR code diff is identical.
        assert_eq!(
            review_freshness(
                "3f64bc31",
                "83d2b4ce",
                &facts(
                    Some("ident-a"),
                    Some("ident-a"),
                    Some(&["crates/fno/src/lib.rs"])
                )
            ),
            Freshness::CarriedBaseSync
        );
    }

    #[test]
    fn freshness_identical_trees_carry_as_base_sync() {
        // An empty tree diff must not fall through the "all paths are docs"
        // branch, which is vacuously true over an empty list.
        assert_eq!(
            review_freshness("aaa", "bbb", &facts(Some("i"), Some("i"), Some(&[]))),
            Freshness::CarriedBaseSync
        );
    }

    #[test]
    fn freshness_docs_only_carries_with_its_reason() {
        // PR 830's specimen: one documentation file moved the head.
        assert_eq!(
            review_freshness(
                "e2976abc",
                "1ef60959",
                &facts(
                    Some("i"),
                    Some("i"),
                    Some(&["docs/architecture/x.md", "README.md"])
                )
            ),
            Freshness::CarriedDocsOnly
        );
    }

    #[test]
    fn freshness_code_change_dies() {
        // 20 of the 22 measured transitions are this: genuine code change, and
        // no rule that refuses to guess can absorb them.
        assert_eq!(
            review_freshness(
                "aaa",
                "bbb",
                &facts(Some("i-old"), Some("i-new"), Some(&["a.rs"]))
            ),
            Freshness::Stale
        );
    }

    #[test]
    fn freshness_missing_identity_dies() {
        // Git failure on either side: fail closed, re-review.
        assert_eq!(
            review_freshness("aaa", "bbb", &facts(None, Some("i"), Some(&[]))),
            Freshness::Stale
        );
        assert_eq!(
            review_freshness("aaa", "bbb", &facts(Some("i"), None, Some(&[]))),
            Freshness::Stale
        );
    }

    #[test]
    fn freshness_two_absent_identities_never_match() {
        // THE regression guard. A first measurement pass reported 63%
        // carry-forward and was wrong: merged PRs' three-dot diff against
        // current origin/main is empty, e3b0c442 is the SHA-256 of the empty
        // string, and twelve transitions matched absence against absence. The
        // true figure was 2 of 22. `Carried` requires two Some values that are
        // equal - never two empties, however they arose.
        assert_eq!(
            review_freshness("aaa", "bbb", &facts(None, None, Some(&[]))),
            Freshness::Stale
        );
    }

    #[test]
    fn freshness_absent_reviewed_sha_dies() {
        // A github_app review object with no `commit.oid`, or an attestation
        // with no head_sha. An empty sha must never match an empty head.
        assert_eq!(
            review_freshness("", "", &facts(Some("i"), Some("i"), Some(&[]))),
            Freshness::Stale
        );
        assert_eq!(
            review_freshness("", "bbb", &facts(Some("i"), Some("i"), Some(&[]))),
            Freshness::Stale
        );
    }

    #[test]
    fn freshness_unreadable_tree_diff_dies() {
        // Matching identities but no way to name the carry reason: a carry that
        // cannot say why it carried is not auditable.
        assert_eq!(
            review_freshness("aaa", "bbb", &facts(Some("i"), Some("i"), None)),
            Freshness::Stale
        );
    }

    #[test]
    fn freshness_only_stale_stops_counting() {
        assert!(Freshness::Fresh.counts());
        assert!(Freshness::CarriedBaseSync.counts());
        assert!(Freshness::CarriedDocsOnly.counts());
        assert!(!Freshness::Stale.counts());
    }

    #[test]
    fn code_diff_identity_drops_docs_and_is_none_when_only_docs_changed() {
        // The identity is computed from `git diff --raw` lines, so exercise the
        // path classifier and the empty-result rule on that exact shape.
        let code = ":100644 100644 aaa bbb M\tcrates/fno/src/lib.rs";
        let docs = ":100644 100644 ccc ddd M\tdocs/architecture/review-lanes.md";
        assert_eq!(raw_diff_line_path(code), "crates/fno/src/lib.rs");
        assert!(!is_documentation_path(raw_diff_line_path(code)));
        assert!(is_documentation_path(raw_diff_line_path(docs)));
    }

    #[test]
    fn freshness_resolver_qualifies_a_bare_base_ref() {
        // `gh pr view` returns `main`, not `origin/main`; a bare branch name
        // resolves to the local ref, which in a stale worktree is not the base.
        let cwd = std::env::temp_dir();
        assert_eq!(
            FreshnessResolver::new("git", &cwd, "main", "abc").base_ref,
            "origin/main"
        );
        assert_eq!(
            FreshnessResolver::new("git", &cwd, "origin/release", "abc").base_ref,
            "origin/release"
        );
        // A slash in the name is not remote-qualification: `release/2.0` is a
        // bare branch and must still be qualified, or the identity resolves
        // against a local ref the worktree may not have.
        assert_eq!(
            FreshnessResolver::new("git", &cwd, "release/2.0", "abc").base_ref,
            "origin/release/2.0"
        );
        assert_eq!(
            FreshnessResolver::new("git", &cwd, "", "abc").base_ref,
            "origin/main"
        );
    }

    // ── both producers go through the one predicate (x-5b99 / x-62a1) ───────

    /// PR #826's real payload shape: codex submitted at `8e557ccd` while the
    /// gate evaluated against head `89bc0b91`, two commits later.
    fn pr826_reviews() -> Vec<Value> {
        vec![serde_json::json!({
            "author": {"login": "chatgpt-codex-connector"},
            "state": "COMMENTED",
            "submittedAt": "2026-08-12T17:51:48Z",
            "commit": {"oid": "8e557ccdecec07abc7e409ad8d888318016612c1"}
        })]
    }

    #[test]
    fn github_app_verdict_at_an_older_commit_is_stale_and_uncovers_the_pr() {
        // THE x-5b99 specimen. Before this, the github_app axis read `state !=
        // ""` and never asked which commit the review was submitted against,
        // so this exact payload produced `coverage: covered, reviewed_count:
        // 1` for a commit codex never saw. The state is non-empty here on
        // purpose: that is the whole of what the old rule looked at.
        let rep = classify_coverage(
            &pr826_reviews(),
            &[],
            "",
            &["chatgpt-codex-connector".to_string()],
            true,
            None,
            &|_| Freshness::Stale,
        );
        let v = &rep.verdicts[0];
        assert_eq!(v.verdict, CoverageVerdict::Stale);
        assert_eq!(v.freshness, Some(Freshness::Stale));
        assert_eq!(v.reviewed_sha, "8e557ccdecec07abc7e409ad8d888318016612c1");
        assert_eq!(rep.coverage, Coverage::Covered(0));
        // And the word a human reads now agrees with the number beside it.
        let data = coverage_event_data(826, &rep, "89bc0b91", "", None);
        assert_eq!(data["coverage"], serde_json::json!("uncovered"));
        assert_eq!(data["reviewed_count"], serde_json::json!(0));
    }

    #[test]
    fn github_app_verdict_carried_across_a_rebase_still_counts() {
        // The same payload where the head moved by a rebase rather than a code
        // change: the reviewer's read still describes the code, so it counts.
        let rep = classify_coverage(
            &pr826_reviews(),
            &[],
            "",
            &["chatgpt-codex-connector".to_string()],
            true,
            None,
            &|_| Freshness::CarriedBaseSync,
        );
        assert_eq!(rep.verdicts[0].verdict, CoverageVerdict::Reviewed);
        assert_eq!(rep.coverage, Coverage::Covered(1));
    }

    #[test]
    fn github_app_review_without_a_commit_oid_fails_closed() {
        // An older review object, or a payload shape change, leaves no commit
        // to pin. That is an absence, and an absence is never freshness.
        let reviews = vec![serde_json::json!({
            "author": {"login": "chatgpt-codex-connector"},
            "state": "APPROVED"
        })];
        let rep = classify_coverage(
            &reviews,
            &[],
            "",
            &["chatgpt-codex-connector".to_string()],
            true,
            None,
            // The real predicate, not a stub: an empty sha must reach Stale on
            // its own rather than because a fake said so.
            &|sha| review_freshness(sha, "89bc0b91", &FreshnessFacts::default()),
        );
        assert_eq!(rep.verdicts[0].verdict, CoverageVerdict::Stale);
        assert_eq!(rep.coverage, Coverage::Covered(0));
    }

    #[test]
    fn coverage_receipt_names_a_stale_reviewer_instead_of_four_zeros() {
        // The receipt for the x-5b99 specimen used to read "0 reviewed, 0
        // refused, 0 errored, 0 absent" - four zeros over a PR codex really did
        // review, at an older commit - and then prescribed the local verb,
        // which is the one move that does NOT get the bot to re-read.
        let rep = classify_coverage(
            &pr826_reviews(),
            &[],
            "",
            &["chatgpt-codex-connector".to_string()],
            true,
            None,
            &|_| Freshness::Stale,
        );
        let line = coverage_receipt_line(&rep);
        // Counted in the tally, NAMED in the next action - the same split the
        // absent bucket uses, and the reason the line carries no empty `()`.
        assert!(line.contains("1 stale,"), "{line}");
        assert!(line.contains("chatgpt-codex-connector"), "{line}");
        assert!(!line.contains("()"), "{line}");
        assert!(line.contains("ask for a re-read"), "{line}");
        assert!(!line.contains("run the review verb"), "{line}");
    }

    #[test]
    fn coverage_receipt_separates_an_old_commit_from_no_commit_at_all() {
        // Both shapes are "stale", and they need OPPOSITE responses. A bot that
        // read an older commit needs a re-read. A whole axis with no commit on
        // any review needs a gh upgrade, because `commit.oid` is where
        // freshness comes from and without it every bot review is stale
        // forever - a required bot never clears and the loop has no exit.
        let no_commit = vec![serde_json::json!({
            "author": {"login": "chatgpt-codex-connector"}, "state": "COMMENTED"
        })];
        let rep = classify_coverage(
            &no_commit,
            &[],
            "",
            &["chatgpt-codex-connector".to_string()],
            true,
            None,
            &|sha| review_freshness(sha, "89bc0b91", &FreshnessFacts::default()),
        );
        let line = coverage_receipt_line(&rep);
        assert!(
            line.contains("no review carries a reviewed commit"),
            "{line}"
        );
        assert!(line.contains("upgrade gh"), "{line}");

        // The ordinary stale case keeps the re-read instruction and must NOT
        // mention gh: the payload named a commit, it is simply an older one.
        let old_commit = classify_coverage(
            &pr826_reviews(),
            &[],
            "",
            &["chatgpt-codex-connector".to_string()],
            true,
            None,
            &|_| Freshness::Stale,
        );
        let line = coverage_receipt_line(&old_commit);
        assert!(line.contains("ask for a re-read"), "{line}");
        assert!(!line.contains("upgrade gh"), "{line}");
    }

    fn attestation_line(reviewer: &str, head: &str, verdict: &str) -> String {
        serde_json::json!({
            "type": "review_attestation",
            "data": {"reviewer": reviewer, "head_sha": head, "verdict": verdict,
                     "attester_session_id": "sess-author"}
        })
        .to_string()
    }

    #[test]
    fn local_attestation_survives_a_carrying_head_move() {
        // THE x-62a1 relief, and the only relief the measurement supports: an
        // attestation at an older commit whose code identity still matches
        // keeps counting. Before this, the scan dropped every line whose head
        // was not byte-equal to the current one, so the mandatory pre-merge
        // rebase destroyed a review that was still entirely valid.
        let events = attestation_line("code-review", "oldhead", "pass");
        let rep = classify_coverage(&[], &[], &events, &[], true, Some("sess-author"), &|_| {
            Freshness::CarriedBaseSync
        });
        assert_eq!(rep.verdicts[0].verdict, CoverageVerdict::Reviewed);
        assert_eq!(rep.verdicts[0].reviewed_sha, "oldhead");
        assert_eq!(rep.coverage, Coverage::Covered(1));
    }

    #[test]
    fn local_attestation_dies_on_a_real_code_change() {
        // The other 91%. No rule that refuses to guess can absorb these.
        let events = attestation_line("code-review", "oldhead", "pass");
        let rep = classify_coverage(&[], &[], &events, &[], true, None, &|_| Freshness::Stale);
        assert_eq!(rep.verdicts[0].verdict, CoverageVerdict::Stale);
        assert_eq!(rep.coverage, Coverage::Covered(0));
    }

    #[test]
    fn a_later_fail_still_revokes_an_earlier_pass_across_heads() {
        // Retraction ordering must survive the scan no longer filtering by
        // head: a `fail` posted after a `pass` revokes it, even when the two
        // sit on different commits that both carry.
        let events = format!(
            "{}\n{}",
            attestation_line("code-review", "headA", "pass"),
            attestation_line("code-review", "headB", "fail")
        );
        let rep = classify_coverage(&[], &[], &events, &[], true, None, &|_| Freshness::Fresh);
        assert!(rep.verdicts.is_empty());
        assert_eq!(rep.coverage, Coverage::Covered(0));
    }

    #[test]
    fn a_stale_local_pass_does_not_rescue_a_failed_github_read() {
        // Positive local evidence trumps a bot outage (x-0eaf). A STALE local
        // pass is not positive evidence of anything current, so it must not
        // buy `covered` the way a fresh one does.
        let events = attestation_line("code-review", "oldhead", "pass");
        let rep = classify_coverage(&[], &[], &events, &[], false, None, &|_| Freshness::Stale);
        assert_eq!(rep.coverage, Coverage::Unknown);

        let fresh = classify_coverage(&[], &[], &events, &[], false, None, &|_| Freshness::Fresh);
        assert_eq!(fresh.coverage, Coverage::Covered(1));
    }

    #[test]
    fn coverage_event_reports_how_much_of_the_count_is_self_attested() {
        // The self-review question answered as a number on the verdict rather
        // than in prose. It gates nothing; it is now READABLE.
        let events = format!(
            "{}\n{}",
            attestation_line("code-review", "h", "pass"),
            serde_json::json!({
                "type": "review_attestation",
                "data": {"reviewer": "sigma", "head_sha": "h", "verdict": "pass",
                         "attester_session_id": "sess-peer"}
            })
        );
        let rep = classify_coverage(&[], &[], &events, &[], true, Some("sess-author"), &|_| {
            Freshness::Fresh
        });
        assert_eq!(rep.coverage, Coverage::Covered(2));
        assert_eq!(rep.self_attested_count(), 1);
        let data = coverage_event_data(826, &rep, "h", "", Some("sess-author"));
        assert_eq!(data["coverage"], serde_json::json!("covered"));
        assert_eq!(data["reviewed_count"], serde_json::json!(2));
        assert_eq!(data["self_attested_count"], serde_json::json!(1));
    }

    #[test]
    fn coverage_event_omits_self_attested_count_when_authorship_unmeasured() {
        // The manifest-less recompute shape: no author session, so every
        // attestation classifies Unknown and self_attested_count() would read
        // 0 while the truth is UNMEASURED. A measured zero and an unmeasured
        // one must not serialize identically - the field is omitted, never 0,
        // so a future gate on it cannot read absence-of-measurement as
        // absence-of-self-attestation (the x-62a1 aggregate shape).
        let events = attestation_line("code-review", "h", "pass");
        let rep = classify_coverage(&[], &[], &events, &[], true, None, &|_| Freshness::Fresh);
        assert_eq!(rep.coverage, Coverage::Covered(1));
        // Every origin is Unknown - the direct statement of "unmeasured".
        assert!(rep
            .verdicts
            .iter()
            .all(|v| v.attestation_origin == AttestationOrigin::Unknown));
        let data = coverage_event_data(826, &rep, "h", "", None);
        assert_eq!(data["reviewed_count"], serde_json::json!(1));
        assert!(
            data.get("self_attested_count").is_none(),
            "an unmeasured authorship must omit the field, not report 0: {data}"
        );
        // The control: the same events with a measured author emit the field
        // (attestation_line stamps attester "sess-author"), so the omission
        // above is the unmeasured marker, not a dropped key. Classification
        // happens at classify_coverage time, so the measured report is built
        // with the author - exactly how read_pr_info threads one
        // author_session into both.
        let measured_rep =
            classify_coverage(&[], &[], &events, &[], true, Some("sess-author"), &|_| {
                Freshness::Fresh
            });
        let measured = coverage_event_data(826, &measured_rep, "h", "", Some("sess-author"));
        assert_eq!(measured["self_attested_count"], serde_json::json!(1));
    }

    #[test]
    fn required_reviewer_gate_honors_the_same_carry() {
        // The N-reachable-paths check. `config.review.reviewers` is satisfied
        // by a DIFFERENT scan than the coverage count, so a carry granted to
        // one and refused by the other leaves the gate exactly as tight as
        // before and the softening purely decorative.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("events.jsonl");
        std::fs::write(&p, attestation_line("code-review", "oldhead", "pass")).unwrap();
        let reviewers = vec!["code-review".to_string()];

        let carried = unattested_reviewers_scan(&p, &reviewers, &|_| Freshness::CarriedBaseSync).0;
        assert!(
            carried.is_empty(),
            "a carried attestation must satisfy the gate"
        );

        let stale = unattested_reviewers_scan(&p, &reviewers, &|_| Freshness::Stale).0;
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].superseded_head.as_deref(), Some("oldhead"));
    }

    #[test]
    fn a_required_bot_that_went_stale_returns_to_missing_bots() {
        // The other reachable path: `missing_bots` drives the presence gate and
        // the nudge. A required bot whose only verdict sits on a commit it read
        // two commits ago has not reviewed THIS code, and the correct response
        // is to ask it to re-read.
        let json = serde_json::json!({"reviews": pr826_reviews(), "comments": []});
        let required = vec!["chatgpt-codex-connector".to_string()];
        let stale = compute_review_info(&json, &required, &|_| Freshness::Stale);
        assert_eq!(stale.missing_bots, required);
        let carried = compute_review_info(&json, &required, &|_| Freshness::CarriedDocsOnly);
        assert!(carried.missing_bots.is_empty());
        // Activity timestamp is not a freshness question: a stale review is
        // still activity, and the no-progress probe must keep seeing it.
        assert_eq!(stale.latest_ts, "2026-08-12T17:51:48Z");
        assert_eq!(review_activity_ts(&json), "2026-08-12T17:51:48Z");
    }

    // ── review_coverage reaches BOTH logs, scoped by repo (x-f43c) ───────────

    #[test]
    fn coverage_event_carries_the_repo_slug() {
        let rep = CoverageReport {
            coverage: Coverage::Covered(1),
            verdicts: vec![],
        };
        let data = coverage_event_data(
            781,
            &rep,
            "a3f4b413b",
            "github.com/bllshttng/footnote",
            None,
        );
        assert_eq!(
            data["repo"],
            serde_json::json!("github.com/bllshttng/footnote")
        );
        assert_eq!(data["pr"], serde_json::json!(781));
        assert_eq!(data["reviewed_count"], serde_json::json!(1));
    }

    #[test]
    fn coverage_event_omits_repo_when_unresolvable() {
        // Omitted, not null: a reader scanning the shared cross-project log
        // must be able to tell "not attributed" from "attributed to nothing",
        // and decline to match it either way.
        let rep = CoverageReport {
            coverage: Coverage::Unknown,
            verdicts: vec![],
        };
        let data = coverage_event_data(781, &rep, "a3f4b413b", "", None);
        assert!(data.get("repo").is_none());
    }

    #[test]
    fn coverage_emit_reaches_the_global_log() {
        // The specimen: the stop hook writes the events file of whatever
        // directory the session ran in, so an attestation made inside a
        // worktree never reached the log a merge from canonical reads. Every
        // other loop-check event already went to both.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("worktree-events.jsonl");
        let global = dir.path().join("global-events.jsonl");
        let rep = CoverageReport {
            coverage: Coverage::Covered(1),
            verdicts: vec![],
        };
        emit_to_both(
            &project,
            &global,
            "review_coverage",
            coverage_event_data(
                781,
                &rep,
                "a3f4b413b",
                "github.com/bllshttng/footnote",
                None,
            ),
        );
        for path in [&project, &global] {
            let text = std::fs::read_to_string(path).unwrap();
            assert!(text.contains("review_coverage"), "missing in {path:?}");
            assert!(
                text.contains("\"repo\":\"github.com/bllshttng/footnote\""),
                "unscoped in {path:?}"
            );
        }
    }

    /// The bare sha equality the predicate replaced, as a freshness resolver.
    /// The pre-x-5b99 tests below run against it unchanged: with no carry ever
    /// granted, the new code must reproduce the old behavior exactly, and any
    /// test that moves is a regression rather than the intended softening.
    fn sha_equality_freshness(head: &str) -> impl Fn(&str) -> Freshness + '_ {
        move |sha: &str| {
            if !sha.is_empty() && sha == head {
                Freshness::Fresh
            } else {
                Freshness::Stale
            }
        }
    }

    /// The list half of the scan. Production reads the count too, so this
    /// wrapper lives here rather than as an unused function in the binary.
    fn unattested_reviewers(
        events_path: &Path,
        reviewers: &[String],
        head_sha: &str,
    ) -> Vec<UnattestedReviewer> {
        unattested_reviewers_scan(events_path, reviewers, &sha_equality_freshness(head_sha)).0
    }

    /// The gate's boolean view of `unattested_reviewers`, exactly as
    /// `read_pr_info` derives it. The pre-x-cdc7 predicate tests below are
    /// unchanged on purpose: promoting the return value to a list must not
    /// move the gate.
    fn reviewers_all_attested(events_path: &Path, reviewers: &[String], head_sha: &str) -> bool {
        unattested_reviewers(events_path, reviewers, head_sha).is_empty()
    }

    // ── streak debounce (x-6231) ─────────────────────────────────────────────
    //
    // These drive `read_prior_fires` with an explicit `now` and gap, so they need
    // no env var and are parallel-safe -- unlike the integration suite, which
    // pins FNO_LOOPCHECK_MIN_FIRE_GAP_SECS=0 process-wide.

    const FP: &str = "FP";
    const NOW: &str = "2026-06-05T12:00:00Z";

    fn at(ts: &str) -> DateTime<Utc> {
        ts.parse().unwrap()
    }

    /// Write a loop_check events log from (ts, fingerprint) pairs, oldest first.
    fn write_fire_log(path: &Path, fires: &[(String, &str)]) {
        let mut out = String::new();
        for (ts, fp) in fires {
            out.push_str(
                &serde_json::json!({
                    "ts": ts, "type": "loop_check", "source": "hook",
                    "data": { "session_id": "sess", "fingerprint": fp },
                })
                .to_string(),
            );
            out.push('\n');
        }
        std::fs::write(path, out).unwrap();
    }

    /// Count the streak over prior fires given as SECONDS BEFORE `now`, oldest
    /// first, all sharing FP. Returns (streak, streak_window_secs).
    fn streak_ago(secs_before_now: &[i64], gap: i64) -> (u64, i64) {
        let now = at(NOW);
        let fires: Vec<(String, &str)> = secs_before_now
            .iter()
            .map(|s| {
                (
                    (now - chrono::Duration::seconds(*s))
                        .format("%Y-%m-%dT%H:%M:%SZ")
                        .to_string(),
                    FP,
                )
            })
            .collect();
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("events.jsonl");
        write_fire_log(&p, &fires);
        let (_, streak, _, window) = read_prior_fires(&p, "sess", FP, now, gap);
        (streak, window)
    }

    /// The streak rules. `consecutive_after` is streak + 1, so a streak of 4 is
    /// what trips the attended backstop of 5.
    #[test]
    fn debounce_streak_counting_rules() {
        // (case, prior fires as seconds before now (oldest first), gap, streak, window)
        #[rustfmt::skip]
        let cases: &[(&str, &[i64], i64, u64, i64)] = &[
            // AC1-HP: the triggering shape - four fires inside 60s are ONE
            // observation (the current fire), nowhere near backstop_n.
            ("rapid burst collapses to one observation", &[49, 33, 16, 0], 300, 0, 0),
            // AC2-HP: a genuinely stalled session is still reaped.
            ("fires 6 minutes apart still trip the backstop", &[1440, 1080, 720, 360], 300, 4, 1440),
            // AC3-FR: a skip must NOT advance the cursor. This fire is 330s
            // before `now` but only 270s before the burst's oldest member, so it
            // counts ONLY because the burst left the cursor parked at `now`.
            ("a skip does not advance the cursor", &[330, 60, 30, 10], 300, 1, 330),
            // AC6-FR: gap 0 is byte-identical to the old fire counting, which is
            // what lets the integration suite pin the seam and keep every
            // backstop assertion it already had.
            ("gap 0 restores fire counting exactly", &[49, 33, 16], 0, 3, 49),
            // Clock skew must not invent a debounce from a bad clock.
            ("a fire stamped after `now` counts, not crashes", &[1200, -600], 300, 2, 1200),
            // AC8-REG: the recorded sequence behind the false terminal - session
            // 20260727T203203Z, five fires in 109 seconds with CI still PENDING.
            ("the false-NoProgress incident now blocks", &[109, 93, 76, 17], 300, 0, 0),
        ];
        for (case, fires, gap, want_streak, want_window) in cases {
            let (streak, window) = streak_ago(fires, *gap);
            assert_eq!(streak, *want_streak, "streak: {case}");
            assert_eq!(window, *want_window, "window: {case}");
        }
    }

    /// AC4-CON: progress is never debounced - a CHANGED fingerprint breaks the
    /// streak however fast it arrived.
    #[test]
    fn debounce_changed_fingerprint_breaks_streak_at_any_speed() {
        let now = at(NOW);
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("events.jsonl");
        write_fire_log(
            &p,
            &[
                ("2026-06-05T11:40:00Z".to_string(), FP),
                ("2026-06-05T11:50:00Z".to_string(), FP),
                ("2026-06-05T11:59:58Z".to_string(), "DIFFERENT"),
            ],
        );
        let (_, streak, _, _) = read_prior_fires(&p, "sess", FP, now, 300);
        assert_eq!(streak, 0, "a 2-second-old change still resets the streak");
    }

    /// AC5-ERR: a fire we cannot place in time is transparent - it neither counts
    /// toward nor breaks the streak, and never panics. Failing this way biases
    /// away from an irreversible NoProgress.
    #[test]
    fn debounce_untimestamped_fire_is_transparent() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("events.jsonl");
        let lines = [
            r#"{"ts":"2026-06-05T11:40:00Z","type":"loop_check","source":"hook","data":{"session_id":"sess","fingerprint":"FP"}}"#,
            r#"{"ts":"not-a-timestamp","type":"loop_check","source":"hook","data":{"session_id":"sess","fingerprint":"FP"}}"#,
            r#"{"type":"loop_check","source":"hook","data":{"session_id":"sess","fingerprint":"FP"}}"#,
        ];
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();

        let (_, streak, last_fp, _) = read_prior_fires(&p, "sess", FP, at(NOW), 300);
        assert_eq!(
            streak, 1,
            "unplaceable fires skip; the good one still counts"
        );
        assert_eq!(
            last_fp.as_deref(),
            Some(FP),
            "carry-forward still reads the newest recorded fp"
        );
    }

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
    fn parse_manifest_harness_session_id_null_sentinel_is_none() {
        // init writes `harness_session_id: ${_HARNESS_SESSION:-null}`, so an
        // unresolvable session lands as the literal "null" and an empty value as
        // "". Both must read as None or a real attester compared against
        // Some("null") mislabels a self-attestation as other_session.
        for raw in ["null", ""] {
            let content = format!("---\nsession_id: abc\nharness_session_id: {raw}\n---\n");
            let m = parse_manifest(&content).unwrap();
            assert_eq!(
                m.harness_session_id, None,
                "harness_session_id: {raw:?} must parse as None"
            );
        }
        // A real id parses through unchanged.
        let m = parse_manifest(
            "---\nsession_id: abc\nharness_session_id: 3abddea3-ad19-481f-b0c1-af19043c95fe\n---\n",
        )
        .unwrap();
        assert_eq!(
            m.harness_session_id.as_deref(),
            Some("3abddea3-ad19-481f-b0c1-af19043c95fe")
        );
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
    fn watching_intent_parses_all_attrs() {
        let (intent, source) = detect_intent(
            Some("waiting <watching reason=\"ci\" pr=\"404\" timeout=\"30m\">"),
            Path::new("/nonexistent"),
        );
        assert_eq!(source, "payload");
        assert_eq!(
            intent,
            Intent::Watching {
                reason: "ci".into(),
                pr: Some("404".into()),
                timeout: Some("30m".into()),
            }
        );
    }

    #[test]
    fn watching_intent_malformed_attrs_default_to_absent() {
        // A bare tag: attributes absent, not an error; lease math applies its
        // own default window downstream.
        let (intent, _) = detect_intent(Some("<watching>"), Path::new("/nonexistent"));
        assert_eq!(
            intent,
            Intent::Watching {
                reason: String::new(),
                pr: None,
                timeout: None,
            }
        );
    }

    #[test]
    fn watching_intent_aborted_beats_watching() {
        let (intent, _) = detect_intent(
            Some("<watching reason=\"ci\" pr=\"1\"> <aborted reason=\"kill\">"),
            Path::new("/nonexistent"),
        );
        assert!(matches!(intent, Intent::Aborted { ref reason } if reason == "kill"));
    }

    #[test]
    fn watching_intent_beats_promise() {
        let (intent, _) = detect_intent(
            Some("<promise>done</promise> <watching reason=\"review\" pr=\"9\">"),
            Path::new("/nonexistent"),
        );
        assert!(matches!(intent, Intent::Watching { .. }));
    }

    #[test]
    fn watching_intent_newest_transcript_entry_honored() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let line = serde_json::json!({
            "message": {"role": "assistant", "content": "<watching reason=\"ci\" pr=\"7\">"}
        });
        std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();
        assert!(matches!(detect_intent_full(&path), Intent::Watching { .. }));
    }

    #[test]
    fn watching_intent_stale_transcript_not_honored() {
        // AC3-EDGE: a watching tag 2 entries back with a tag-less newest entry
        // must NOT resurrect as Watching (payload-or-newest-entry rule).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let mut content = String::new();
        for text in [
            "<watching reason=\"ci\" pr=\"3\">", // oldest
            "still going",
            "moving on to unrelated work", // newest
        ] {
            let line = serde_json::json!({"message": {"role": "assistant", "content": text}});
            content.push_str(&serde_json::to_string(&line).unwrap());
            content.push('\n');
        }
        std::fs::write(&path, content).unwrap();
        assert_eq!(detect_intent_full(&path), Intent::None);
    }

    #[test]
    fn watching_intent_stale_watch_does_not_shadow_deeper_promise() {
        // A stale watching in the newest-but-one entry is skipped, and a real
        // promise deeper in the lookback window still wins.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let mut content = String::new();
        for text in [
            "<promise>MISSION COMPLETE: shipped</promise>", // oldest, real
            "<watching reason=\"ci\" pr=\"3\">",            // stale (not newest)
            "tag-less newest",                              // newest
        ] {
            let line = serde_json::json!({"message": {"role": "assistant", "content": text}});
            content.push_str(&serde_json::to_string(&line).unwrap());
            content.push('\n');
        }
        std::fs::write(&path, content).unwrap();
        assert_eq!(detect_intent_full(&path), Intent::Promise);
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
            bot_nudges: vec![],
            usage_limited: vec![],
            unaddressed_findings: vec![],
            review_skipped: false,
            unattested_reviewers: vec![],
            malformed_attestations: 0,
            coverage: CoverageReport {
                coverage: Coverage::Covered(0),
                verdicts: vec![],
            },
        };
        let reason = build_block_reason(&pr, "abc", true);
        assert!(
            reason.contains("still running"),
            "pending CI must not read as red; got: {reason}"
        );
        assert!(!reason.contains("failed"), "got: {reason}");
    }

    #[test]
    fn unwatched_async_nudge_ci_pending_teaches_arm_and_tag() {
        // AC3-HP: the CI-pending block message must instruct arming a
        // harness-tracked watcher with a timeout and emitting <watching>,
        // replacing the old "wait silently" prose.
        let pr = PrInfo {
            ci_conclusion: CiConclusion::Pending,
            ci_has_pending: true,
            ..watch_pr()
        };
        let reason = build_block_reason(&pr, "abc", true);
        assert!(reason.contains("<watching"), "got: {reason}");
        assert!(reason.contains("timeout"), "got: {reason}");
        // The taught watcher is the REST status poll, never the
        // GraphQL `gh pr checks --watch` this assertion used to pin.
        assert!(reason.contains("fno pr status"), "got: {reason}");
        assert!(!reason.contains("gh pr checks"), "got: {reason}");
        assert!(!reason.contains("wait silently"), "got: {reason}");
    }

    #[test]
    fn no_hint_prescribes_the_timeout_binary() {
        // File-wide, so a future hint cannot reintroduce `timeout(1)` at a site
        // this test does not name. The needle is built at runtime so the test
        // does not match its own source.
        let needle = ["timeout", " "].concat();
        for tail in include_str!("loopcheck.rs").split(&needle).skip(1) {
            assert!(
                !tail.trim_start().starts_with(|c: char| c.is_ascii_digit()),
                "bare timeout invocation: ...{}",
                tail.chars().take(60).collect::<String>()
            );
        }
    }

    #[test]
    fn unwatched_async_nudge_missing_review_teaches_arm_and_tag() {
        let pr = PrInfo {
            ci_conclusion: CiConclusion::Success,
            ci_has_pending: false,
            reviewed: false,
            missing_bots: vec!["chatgpt-codex-connector".into()],
            bot_nudges: vec![],
            ..watch_pr()
        };
        let reason = build_block_reason(&pr, "abc", true);
        assert!(reason.contains("chatgpt-codex-connector"), "got: {reason}");
        assert!(reason.contains("<watching"), "got: {reason}");
    }

    // ── Watching idle-allow classification (x-e2c8) ───────────────────────
    /// An open PR whose head matches local HEAD, CI still pending, no findings.
    fn watch_pr() -> PrInfo {
        PrInfo {
            state: PrState::Open,
            number: 404,
            head_oid: "abc".to_string(),
            ci_conclusion: CiConclusion::Pending,
            failing_checks: vec![],
            ci_has_pending: true,
            mergeable: "UNKNOWN".to_string(),
            latest_review_ts: "none".to_string(),
            reviewed: false,
            missing_bots: vec![],
            bot_nudges: vec![],
            usage_limited: vec![],
            unaddressed_findings: vec![],
            review_skipped: false,
            unattested_reviewers: vec![],
            malformed_attestations: 0,
            coverage: CoverageReport {
                coverage: Coverage::Covered(0),
                verdicts: vec![],
            },
        }
    }

    /// x-9ab2: the terminal fires only when the quota bounce is the SOLE unmet
    /// conjunct of `reviewed`. Each case drops one other conjunct and must
    /// block; reverting `awaiting_review_only` to the bare
    /// `!usage_limited.is_empty()` check fails them.
    #[test]
    fn awaiting_review_only_requires_every_other_conjunct() {
        let bounced = || {
            let mut pr = watch_pr();
            pr.usage_limited = vec!["chatgpt-codex-connector".to_string()];
            pr
        };
        assert!(awaiting_review_only(&bounced()), "the terminal's own case");
        assert!(!awaiting_review_only(&watch_pr()), "no bounce to report");

        // A bot that has not reviewed YET is owed its nudge window: one bot's
        // quota state must not end the session on the others' behalf.
        let mut still_pending = bounced();
        still_pending.missing_bots = vec!["gemini-code-assist".to_string()];
        assert!(
            !awaiting_review_only(&still_pending),
            "bot still owed a wait"
        );

        // A standing blocking finding is work the agent must DO; parking hands
        // a human a PR carrying an unaddressed P1.
        let mut with_finding = bounced();
        with_finding.unaddressed_findings = vec![Finding {
            id: 1,
            author: "gemini-code-assist".to_string(),
            path: "src/lib.rs".to_string(),
            line: 12,
            created_at: "2026-08-06T00:00:00Z".to_string(),
            severity: "P1",
            had_reply: true,
        }];
        assert!(!awaiting_review_only(&with_finding), "unaddressed P1");

        let mut unattested = bounced();
        unattested.unattested_reviewers = vec![UnattestedReviewer {
            name: "sigma".to_string(),
            superseded_head: None,
            failed_at_head: false,
        }];
        assert!(!awaiting_review_only(&unattested), "local review never ran");
    }

    #[test]
    fn watch_idle_classifies_pending_ci() {
        assert_eq!(async_wait_class(&watch_pr(), "abc", true), Some("ci"));
    }

    #[test]
    fn codex_watch_harness_gate_is_claude_only() {
        // Only Claude self-wakes on a background-task exit, so only Claude idles.
        assert!(harness_can_idle(Some("claude"), false));
        // A loop-run child (FNO_DRIVER_LIB) exits on allow -> never idles.
        assert!(!harness_can_idle(Some("claude"), true));
        // codex/gemini have no self-wake; their daemon-consumer waker ships
        // separately, so until then they keep today's block behavior.
        assert!(!harness_can_idle(Some("codex"), false));
        assert!(!harness_can_idle(Some("gemini"), false));
        // Unknown harness (bare shell / daemon): conservative block.
        assert!(!harness_can_idle(None, false));
    }

    #[test]
    fn watch_idle_classifies_awaiting_review() {
        // CI green, no pending checks, and a required GitHub bot has not reviewed.
        let pr = PrInfo {
            ci_conclusion: CiConclusion::Success,
            ci_has_pending: false,
            reviewed: false,
            review_skipped: false,
            missing_bots: vec!["chatgpt-codex-connector".into()],
            bot_nudges: vec![],
            ..watch_pr()
        };
        assert_eq!(async_wait_class(&pr, "abc", true), Some("review"));
    }

    #[test]
    fn watch_idle_rejects_ci_pending_with_a_failure() {
        // gemini finding: a check has ALREADY concluded red while others run.
        // The agent should debug now, not idle out the remaining pending checks.
        let pr = PrInfo {
            ci_conclusion: CiConclusion::Failure(Some("unit".into())),
            ci_has_pending: true,
            ..watch_pr()
        };
        assert_eq!(async_wait_class(&pr, "abc", true), None);
    }

    #[test]
    fn watch_idle_rejects_local_attestation_review_gate() {
        // codex P1: reviewed=false with an EMPTY missing_bots is a local
        // attestation (sigma) or unaddressed-finding gate - no GitHub reviewer
        // will ever post to wake the session, so idling would park it forever.
        let pr = PrInfo {
            ci_conclusion: CiConclusion::Success,
            ci_has_pending: false,
            reviewed: false,
            review_skipped: false,
            missing_bots: vec![],
            bot_nudges: vec![],
            ..watch_pr()
        };
        assert_eq!(async_wait_class(&pr, "abc", true), None);
    }

    // ── x-b167 idle rule + message rendering ──────────────────────────────────

    fn bn(login: &str, class: NudgeClass, nudges: usize, newest: i64, span: i64) -> BotNudge {
        BotNudge {
            login: login.into(),
            class,
            review_handle: "@codex review".into(),
            ceiling: 3,
            nudges,
            newest_age_min: newest,
            span_min: span,
        }
    }
    fn bot_review_pr(login: &str, nudges: Vec<BotNudge>) -> PrInfo {
        PrInfo {
            number: 618,
            ci_conclusion: CiConclusion::Success,
            ci_has_pending: false,
            reviewed: false,
            review_skipped: false,
            missing_bots: vec![login.into()],
            bot_nudges: nudges,
            ..watch_pr()
        }
    }

    #[test]
    fn nudge_needs_nudge_blocks_and_names_the_command() {
        // AC1: not idlable; reason gives the exact gh command; no arm-and-tag hint.
        let pr = bot_review_pr(
            "chatgpt-codex-connector",
            vec![bn(
                "chatgpt-codex-connector",
                NudgeClass::NeedsNudge,
                0,
                0,
                0,
            )],
        );
        assert_eq!(async_wait_class(&pr, "abc", true), None);
        let reason = build_block_reason(&pr, "abc", true);
        assert!(
            reason.contains("gh pr comment 618 --body \"@codex review\""),
            "{reason}"
        );
        assert!(
            !reason.contains("harness-tracked watcher"),
            "no arm hint: {reason}"
        );
    }

    #[test]
    fn nudge_awaiting_idles_with_the_arm_hint() {
        // AC2: a genuine async wait - idlable, message says nudged + awaiting,
        // and the arm-and-tag ritual is present.
        let pr = bot_review_pr(
            "chatgpt-codex-connector",
            vec![bn("chatgpt-codex-connector", NudgeClass::Awaiting, 1, 3, 3)],
        );
        assert_eq!(async_wait_class(&pr, "abc", true), Some("review"));
        let reason = build_block_reason(&pr, "abc", true);
        assert!(reason.contains("nudged"), "{reason}");
        assert!(reason.contains("awaiting"), "{reason}");
        assert!(
            reason.contains("harness-tracked watcher"),
            "arm hint present: {reason}"
        );
    }

    #[test]
    fn nudge_unresponsive_blocks_and_names_optional_apps() {
        // AC3: not idlable; names the give-up + optional_apps; no arm-and-tag hint.
        let pr = bot_review_pr(
            "chatgpt-codex-connector",
            vec![bn(
                "chatgpt-codex-connector",
                NudgeClass::Unresponsive,
                3,
                20,
                47,
            )],
        );
        assert_eq!(async_wait_class(&pr, "abc", true), None);
        let reason = build_block_reason(&pr, "abc", true);
        assert!(
            reason.contains("did not review after 3 nudges over 47m"),
            "{reason}"
        );
        assert!(reason.contains("config.review.optional_apps"), "{reason}");
        assert!(reason.contains("do not arm a watcher"), "{reason}");
        assert!(
            !reason.contains("harness-tracked watcher"),
            "no arm hint: {reason}"
        );
    }

    #[test]
    fn nudge_not_nudgeable_keeps_todays_behavior() {
        // AC5: a non-nudgeable required bot keeps today's string + arm hint and
        // stays idlable, regardless of comment history.
        let pr = bot_review_pr(
            "gemini-code-assist",
            vec![bn("gemini-code-assist", NudgeClass::NotNudgeable, 0, 0, 0)],
        );
        assert_eq!(async_wait_class(&pr, "abc", true), Some("review"));
        let reason = build_block_reason(&pr, "abc", true);
        assert!(
            reason.contains("gemini-code-assist has not reviewed"),
            "{reason}"
        );
        assert!(
            reason.contains("harness-tracked watcher"),
            "arm hint present: {reason}"
        );
    }

    #[test]
    fn nudge_empty_classification_is_status_quo() {
        // A non-empty missing_bots with an EMPTY bot_nudges (not classified)
        // behaves exactly as pre-x-b167: idlable, today's string.
        let pr = bot_review_pr("chatgpt-codex-connector", vec![]);
        assert_eq!(async_wait_class(&pr, "abc", true), Some("review"));
        let reason = build_block_reason(&pr, "abc", true);
        assert!(reason.contains("has not reviewed"), "{reason}");
    }

    #[test]
    fn finding_block_reason_names_the_reply_handle() {
        // AC14: an unaddressed finding by a known bot names the handle a reply
        // must address, not just "reply in-thread".
        let pr = PrInfo {
            ci_conclusion: CiConclusion::Success,
            ci_has_pending: false,
            reviewed: false,
            unaddressed_findings: vec![Finding {
                id: 1,
                author: "chatgpt-codex-connector".into(),
                path: "a.rs".into(),
                line: 10,
                created_at: "2026-07-06T01:00:00Z".into(),
                severity: "P1",
                had_reply: true,
            }],
            ..watch_pr()
        };
        let reason = build_block_reason(&pr, "abc", true);
        assert!(reason.contains("@chatgpt-codex-connector"), "{reason}");
    }

    #[test]
    fn nudge_post_is_suppressed_by_the_escape_hatch() {
        // The test suite must never comment on a real PR. With the guard set,
        // post_nudge_comment returns false without spawning gh (a bogus bin here
        // would otherwise error, not silently succeed).
        std::env::set_var("FNO_LOOPCHECK_NO_COMMENT", "1");
        let posted = post_nudge_comment(
            "/nonexistent/gh",
            std::path::Path::new("/tmp"),
            618,
            "@codex review",
        );
        std::env::remove_var("FNO_LOOPCHECK_NO_COMMENT");
        assert!(!posted);
    }

    #[test]
    fn unresponsive_bot_drives_the_giveup_message() {
        // AC13: the NoProgress message names the bot, the nudge count, and the
        // elapsed time instead of a bare fingerprint streak.
        let pr = bot_review_pr(
            "chatgpt-codex-connector",
            vec![bn(
                "chatgpt-codex-connector",
                NudgeClass::Unresponsive,
                3,
                20,
                47,
            )],
        );
        let n = unresponsive_bot(&pr).expect("an unresponsive bot");
        let msg = nudge_giveup_message(n);
        assert!(msg.contains("chatgpt-codex-connector"), "{msg}");
        assert!(msg.contains("3 nudges over 47m"), "{msg}");
        assert!(msg.contains("config.review.optional_apps"), "{msg}");
    }

    #[test]
    fn no_giveup_for_an_awaiting_bot() {
        let pr = bot_review_pr(
            "chatgpt-codex-connector",
            vec![bn("chatgpt-codex-connector", NudgeClass::Awaiting, 1, 3, 3)],
        );
        assert!(unresponsive_bot(&pr).is_none());
    }

    /// The exact state PR #618 sat in for ~15 turns: CI green, no required
    /// bots, no unaddressed findings, and a `reviewers: [sigma]` gate with no
    /// head-pinned attestation. `reviewers_ok` was the sole failing term.
    fn reviewers_gate_pr() -> PrInfo {
        PrInfo {
            ci_conclusion: CiConclusion::Success,
            ci_has_pending: false,
            reviewed: false,
            review_skipped: false,
            missing_bots: vec![],
            bot_nudges: vec![],
            unaddressed_findings: vec![],
            unattested_reviewers: vec![UnattestedReviewer {
                name: "sigma".to_string(),
                superseded_head: None,
                failed_at_head: false,
            }],
            ..watch_pr()
        }
    }

    #[test]
    fn block_reason_names_the_reviewers_gate_not_a_bot() {
        // AC2: the old string claimed a bot had not reviewed while
        // required_bots was empty and the real blocker was local.
        let reason = build_block_reason(&reviewers_gate_pr(), "abc", true);
        assert!(reason.contains("reviewers gate unmet"), "got: {reason}");
        assert!(reason.contains("sigma"), "got: {reason}");
        assert!(reason.contains("/fno:review sigma"), "got: {reason}");
        assert!(!reason.contains("bot reviewer"), "got: {reason}");
    }

    #[test]
    fn block_reason_names_the_local_peer_invocation() {
        let mut pr = reviewers_gate_pr();
        pr.unattested_reviewers[0].name = LOCAL_PEER_REVIEWER.to_string();
        let reason = build_block_reason(&pr, "abc", true);
        assert!(
            reason.contains("/fno:review peer --attest"),
            "got: {reason}"
        );
        assert!(
            !reason.contains("wait on a GitHub reviewer"),
            "got: {reason}"
        );
    }

    #[test]
    fn block_reason_explains_same_model_local_peer_refusal() {
        let mut pr = reviewers_gate_pr();
        pr.unattested_reviewers[0].name = SAME_MODEL_LOCAL_PEER_SENTINEL.to_string();
        let reason = build_block_reason(&pr, "abc", true);
        assert!(
            reason.contains("configure a cross-model peer"),
            "got: {reason}"
        );
        assert!(
            !reason.contains(SAME_MODEL_LOCAL_PEER_SENTINEL),
            "got: {reason}"
        );
    }

    #[test]
    fn block_reason_reviewers_gate_emits_no_idle_ritual() {
        // AC3: async_wait_class already excluded this blocker from idling
        // (watch_idle_rejects_local_attestation_review_gate), so prescribing
        // the arm-and-tag ritual here is the code contradicting itself.
        let pr = reviewers_gate_pr();
        assert_eq!(async_wait_class(&pr, "abc", true), None);
        let reason = build_block_reason(&pr, "abc", true);
        assert!(!reason.contains("<watching"), "got: {reason}");
        assert!(
            !reason.contains("Arm a harness-tracked watcher"),
            "got: {reason}"
        );
        assert!(!reason.contains("gh pr checks"), "got: {reason}");
    }

    #[test]
    fn block_reason_names_a_superseded_attestation_head() {
        // A session that ran sigma and then pushed must not read "you never
        // ran sigma"; name the head the pass is pinned to.
        let pr = PrInfo {
            unattested_reviewers: vec![UnattestedReviewer {
                name: "sigma".to_string(),
                superseded_head: Some("0123456789abcdef".to_string()),
                failed_at_head: false,
            }],
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true);
        assert!(reason.contains("01234567"), "got: {reason}");
        assert!(reason.contains("superseded"), "got: {reason}");
    }

    #[test]
    fn block_reason_generic_review_fallback_has_no_idle_ritual() {
        // The fallback is only reachable with an EMPTY missing_bots, which
        // async_wait_class refuses to idle. It must not teach the ritual either.
        let pr = PrInfo {
            unattested_reviewers: vec![],
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true);
        assert!(!reason.contains("<watching"), "got: {reason}");
        assert!(!reason.contains("bot reviewer"), "got: {reason}");
    }

    #[test]
    fn block_reason_missing_bot_still_teaches_the_ritual() {
        // AC7-adjacent regression: a REAL outstanding GitHub bot, and nothing
        // local outstanding, is a valid async wait and keeps today's message.
        let pr = PrInfo {
            missing_bots: vec!["chatgpt-codex-connector".into()],
            bot_nudges: vec![],
            unattested_reviewers: vec![],
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true);
        assert!(reason.contains("chatgpt-codex-connector"), "got: {reason}");
        assert!(reason.contains("<watching"), "got: {reason}");
    }

    #[test]
    fn block_reason_local_work_outranks_a_bot_wait() {
        // Codex review of this PR: with a bot AND a local reviewer both
        // outstanding, naming only the bot hides the half the session can act
        // on now. Worse, if the bot never posts, the local work never happens
        // and the run dies on budget with the gate still unmet - the #618 shape
        // this node exists to delete.
        let pr = PrInfo {
            missing_bots: vec!["chatgpt-codex-connector".into()],
            bot_nudges: vec![],
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true);
        assert!(reason.contains("reviewers gate unmet"), "got: {reason}");
        assert!(!reason.contains("<watching"), "got: {reason}");
        // Once the local half is attested, the bot wait is the sole blocker and
        // the arm-and-tag message returns.
        let after = PrInfo {
            unattested_reviewers: vec![],
            ..pr
        };
        assert!(build_block_reason(&after, "abc", true).contains("<watching"));
    }

    #[test]
    fn reviewers_gate_stays_fail_closed() {
        // AC7: promoting the predicate's return value to a list must not move
        // the gate. Missing file, missing event, stale head, and a `fail`
        // verdict all still leave the reviewer unsatisfied.
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("absent.jsonl");
        let sigma = vec!["sigma".to_string()];
        assert!(!unattested_reviewers(&missing, &sigma, "h").is_empty());

        let stale = tmp.path().join("stale.jsonl");
        std::fs::write(
            &stale,
            r#"{"type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"pass"}}"#,
        )
        .unwrap();
        let out = unattested_reviewers(&stale, &sigma, "NEW");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].superseded_head.as_deref(), Some("OLD"));
        assert!(!out[0].failed_at_head);

        let failed = tmp.path().join("fail.jsonl");
        std::fs::write(
            &failed,
            r#"{"type":"review_attestation","data":{"reviewer":"sigma","head_sha":"h","verdict":"fail"}}"#,
        )
        .unwrap();
        let out = unattested_reviewers(&failed, &sigma, "h");
        assert_eq!(out.len(), 1);
        // A head-pinned fail is not a superseded pass; do not offer a stale head.
        assert_eq!(out[0].superseded_head, None);
        // ...but it IS an attestation at this head, and the message says so
        // rather than claiming none exists. Pinned at the PARSER: the message
        // test hand-builds the struct and never exercises this derivation, so
        // `failed_at_head: false` survived the whole suite before this line.
        assert!(
            out[0].failed_at_head,
            "a fail at HEAD must be reported as such"
        );
    }

    #[test]
    fn unpinned_attestation_never_counts_as_evidence() {
        // codex P1 on this PR: defaulting a missing head_sha to "" made an
        // unpinned event MATCH a caller whose own head_sha is "", turning
        // no-evidence into a pass.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("e.jsonl");
        std::fs::write(
            &p,
            r#"{"type":"review_attestation","data":{"reviewer":"sigma","verdict":"pass"}}"#,
        )
        .unwrap();
        let out = unattested_reviewers(&p, &["sigma".to_string()], "");
        assert_eq!(out.len(), 1, "unpinned evidence must not satisfy the gate");
        assert_eq!(out[0].superseded_head, None);
    }

    #[test]
    fn a_failed_old_head_is_not_reported_as_superseded() {
        // codex P2: "attested at X, superseded" implies a prior PASS. An
        // old-head fail rendered that way invents a review that never passed.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("e.jsonl");
        std::fs::write(
            &p,
            r#"{"type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"fail"}}"#,
        )
        .unwrap();
        let out = unattested_reviewers(&p, &["sigma".to_string()], "NEW");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].superseded_head, None);
    }

    #[test]
    fn a_corrupt_attestation_line_is_counted_and_named() {
        // A torn write leaves an unparseable review_attestation in the file.
        // The gate must still fail closed, but reporting "no head-pinned
        // review_attestation" over a corrupt one is the same class of lie this
        // node deletes. The sibling review_finding scanner already counts its
        // malformed lines; this one did not.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("e.jsonl");
        std::fs::write(
            &p,
            concat!(
                r#"{"type":"review_attestation","data":{"reviewer":"sigma","hea"#,
                "\n",
                r#"{"type":"loop_check","data":{}}"#,
            ),
        )
        .unwrap();
        let (out, malformed) =
            unattested_reviewers_scan(&p, &["sigma".to_string()], &sha_equality_freshness("h"));
        assert_eq!(out.len(), 1, "a corrupt line never satisfies the gate");
        assert_eq!(malformed, 1, "and it is counted, not silently dropped");

        let pr = PrInfo {
            malformed_attestations: malformed,
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true);
        assert!(
            reason.contains("unparseable attestation line"),
            "got: {reason}"
        );

        // A clean file adds nothing to the message.
        std::fs::write(&p, r#"{"type":"loop_check","data":{}}"#).unwrap();
        assert_eq!(
            unattested_reviewers_scan(&p, &["sigma".to_string()], &sha_equality_freshness("h")).1,
            0
        );
        assert!(!build_block_reason(&reviewers_gate_pr(), "abc", true)
            .contains("unparseable attestation line"));
    }

    #[test]
    fn a_revoked_pass_falls_back_to_an_older_passing_head() {
        // codex P2: `pass A, pass B, fail B` with HEAD C. A single "most recent
        // pass" entry overwrites A with B and then drops B, so the message
        // claims no prior pass while A is still a real one - the misleading
        // guidance this whole node exists to delete, reappearing in exactly the
        // multi-round review/fix cycle that produces this sequence.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("e.jsonl");
        let line = |head: &str, verdict: &str| {
            format!(
                r#"{{"type":"review_attestation","data":{{"reviewer":"sigma","head_sha":"{head}","verdict":"{verdict}"}}}}"#
            )
        };
        std::fs::write(
            &p,
            [
                line("AAA", "pass"),
                line("BBB", "pass"),
                line("BBB", "fail"),
            ]
            .join("\n"),
        )
        .unwrap();
        let out = unattested_reviewers(&p, &["sigma".to_string()], "CCC");
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].superseded_head.as_deref(),
            Some("AAA"),
            "a still-valid older pass must survive a newer head's retraction"
        );

        // The newest STILL-PASSING head wins when several are valid.
        std::fs::write(&p, [line("AAA", "pass"), line("BBB", "pass")].join("\n")).unwrap();
        let out = unattested_reviewers(&p, &["sigma".to_string()], "CCC");
        assert_eq!(out[0].superseded_head.as_deref(), Some("BBB"));

        // Every old head retracted -> nothing to name.
        std::fs::write(
            &p,
            [
                line("AAA", "pass"),
                line("BBB", "pass"),
                line("BBB", "fail"),
                line("AAA", "fail"),
            ]
            .join("\n"),
        )
        .unwrap();
        let out = unattested_reviewers(&p, &["sigma".to_string()], "CCC");
        assert_eq!(out[0].superseded_head, None);
    }

    #[test]
    fn a_later_fail_revokes_the_superseded_pass_for_that_head() {
        // Append-ordered pass-then-fail on the SAME old head. The pass was
        // recorded as superseded and the fail merely skipped, so the message
        // kept claiming that head was successfully attested after its latest
        // verdict retracted exactly that (codex P2 on this PR).
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("e.jsonl");
        std::fs::write(
            &p,
            concat!(
                r#"{"type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"pass"}}"#,
                "\n",
                r#"{"type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"fail"}}"#,
            ),
        )
        .unwrap();
        let out = unattested_reviewers(&p, &["sigma".to_string()], "NEW");
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].superseded_head, None,
            "a retracted pass is not evidence"
        );

        // A re-run pass after the fail restores it: revocation is latest-wins,
        // not a one-way latch.
        std::fs::write(
            &p,
            concat!(
                r#"{"type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"pass"}}"#,
                "\n",
                r#"{"type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"fail"}}"#,
                "\n",
                r#"{"type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"pass"}}"#,
            ),
        )
        .unwrap();
        let out = unattested_reviewers(&p, &["sigma".to_string()], "NEW");
        assert_eq!(out[0].superseded_head.as_deref(), Some("OLD"));
    }

    #[test]
    fn short_sha_never_panics_on_multibyte() {
        // codex P2: `&s[..8]` panics when byte 8 lands inside a character, and
        // superseded_head comes from a user-writable events.jsonl.
        assert_eq!(short_sha("0123456789ab"), "01234567");
        assert_eq!(short_sha("abc"), "abc");
        assert_eq!(short_sha(""), "");
        assert_eq!(short_sha("1234567\u{e9}xyz"), "1234567\u{e9}");
        let pr = PrInfo {
            unattested_reviewers: vec![UnattestedReviewer {
                name: "sigma".to_string(),
                superseded_head: Some("1234567\u{e9}abc".to_string()),
                failed_at_head: false,
            }],
            ..reviewers_gate_pr()
        };
        build_block_reason(&pr, "1234567\u{e9}abc", true);
    }

    #[test]
    fn watcher_hint_never_contradicts_the_idle_classifier() {
        // codex P1: the missing-bot branch emitted the ritual unconditionally,
        // including for states async_wait_class refuses to idle (an unaddressed
        // finding, or an open operator finding). The hint is now derived from
        // that same classifier, so the two agree by construction.
        let bot_only = PrInfo {
            missing_bots: vec!["chatgpt-codex-connector".into()],
            bot_nudges: vec![],
            unattested_reviewers: vec![],
            ..reviewers_gate_pr()
        };
        for (label, pr, open_empty) in [
            // Reaches the FINDINGS branch, not the bot branch: unaddressed
            // findings render first. Kept because the invariant under test is
            // "no hint for a non-idlable state", which holds branch-wide - but
            // it is the case below that reaches `missing_bots`, so that one is
            // what would catch a reintroduced unconditional `arm_watch_hint`
            // there. A sigma round caught this test silently losing its teeth
            // when the branch order moved out from under it.
            (
                "bot + unaddressed finding (renders as the finding)",
                PrInfo {
                    missing_bots: vec!["chatgpt-codex-connector".into()],
                    bot_nudges: vec![],
                    unattested_reviewers: vec![],
                    unaddressed_findings: vec![Finding {
                        id: 1,
                        author: "codex".into(),
                        path: "a.rs".into(),
                        line: 1,
                        created_at: "2026-07-27T00:00:00Z".into(),
                        severity: "P1",
                        had_reply: true,
                    }],
                    ..reviewers_gate_pr()
                },
                true,
            ),
            (
                // The one that DOES reach `missing_bots` while non-idlable.
                "bot + open operator finding",
                PrInfo {
                    missing_bots: vec!["chatgpt-codex-connector".into()],
                    bot_nudges: vec![],
                    unattested_reviewers: vec![],
                    ..reviewers_gate_pr()
                },
                false,
            ),
        ] {
            let reason = build_block_reason(&pr, "abc", open_empty);
            assert_eq!(async_wait_class(&pr, "abc", open_empty), None, "{label}");
            assert!(!reason.contains("<watching"), "{label}: {reason}");
        }
        // The genuinely idlable state keeps the ritual.
        assert_eq!(async_wait_class(&bot_only, "abc", true), Some("review"));
        assert!(build_block_reason(&bot_only, "abc", true).contains("<watching"));
    }

    #[test]
    fn an_unaddressed_finding_is_named_before_the_reviewers_gate() {
        // Sigma review of this PR: addressing an inline finding MOVES HEAD,
        // which supersedes any attestation produced first. Naming the reviewer
        // first would make the session run the panel twice.
        let pr = PrInfo {
            unaddressed_findings: vec![Finding {
                id: 1,
                author: "codex".into(),
                path: "a.rs".into(),
                line: 7,
                created_at: "2026-07-27T00:00:00Z".into(),
                severity: "P1",
                had_reply: true,
            }],
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true);
        assert!(reason.contains("unaddressed"), "got: {reason}");
        assert!(!reason.contains("reviewers gate unmet"), "got: {reason}");
        // With the finding cleared, the reviewers gate is what is named.
        let after = PrInfo {
            unaddressed_findings: vec![],
            ..pr
        };
        assert!(build_block_reason(&after, "abc", true).contains("reviewers gate unmet"));
    }

    #[test]
    fn unaddressed_finding_with_no_reply_names_the_top_level_blind_spot() {
        // A finding answered with a top-level PR comment reads as unaddressed
        // because the gate walks in_reply_to_id chains only. The block reason
        // must name the mechanism and the exact gh command, not the ambiguous
        // "reply in-thread" a worker who posted a top-level comment reads as
        // "I did reply" (PR #447, #787 both stalled green PRs this way).
        let pr = PrInfo {
            unaddressed_findings: vec![Finding {
                id: 1,
                author: "codex".into(),
                path: "a.rs".into(),
                line: 7,
                created_at: "2026-07-27T00:00:00Z".into(),
                severity: "P1",
                had_reply: false,
            }],
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true);
        assert!(reason.contains("no in-thread reply"), "got: {reason}");
        assert!(reason.contains("in_reply_to_id"), "got: {reason}");
        assert!(reason.contains("top-level PR comment"), "got: {reason}");
        assert!(reason.contains("in_reply_to=<id>"), "got: {reason}");
    }

    #[test]
    fn a_failed_attestation_at_this_head_is_not_reported_as_absent() {
        // "no head-pinned review_attestation" reads as "you never ran it" to a
        // session that ran the reviewer and was told no.
        let pr = PrInfo {
            unattested_reviewers: vec![UnattestedReviewer {
                name: "sigma".to_string(),
                superseded_head: None,
                failed_at_head: true,
            }],
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true);
        assert!(reason.contains("verdict NOT pass"), "got: {reason}");
    }

    #[test]
    fn the_stop_gate_marks_declare_as_a_self_cert() {
        // AC5: every surface that prints `declare` says it asserts nothing.
        // The Rust block message is such a surface.
        let pr = PrInfo {
            unattested_reviewers: vec![UnattestedReviewer {
                name: "declare".to_string(),
                superseded_head: None,
                failed_at_head: false,
            }],
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true);
        assert!(reason.contains("self-cert"), "got: {reason}");
        assert!(
            reason.contains("asserts no review evidence"),
            "got: {reason}"
        );
        // A real reviewer carries no such mark.
        assert!(!build_block_reason(&reviewers_gate_pr(), "abc", true).contains("self-cert"));
    }

    #[test]
    fn an_empty_head_sha_never_becomes_a_superseded_head() {
        // Option<String> cannot say "non-empty", so normalize at construction
        // rather than leaving is_empty() as a convention every reader re-derives.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("e.jsonl");
        std::fs::write(
            &p,
            r#"{"type":"review_attestation","data":{"reviewer":"sigma","head_sha":"","verdict":"pass"}}"#,
        )
        .unwrap();
        let out = unattested_reviewers(&p, &["sigma".to_string()], "NEW");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].superseded_head, None);
    }

    #[test]
    fn an_outstanding_local_reviewer_is_never_an_idlable_wait() {
        // The classifier half of the same fix: with a bot AND a local reviewer
        // outstanding, a stray <watching> tag must not park the session on work
        // it could do now.
        let pr = PrInfo {
            missing_bots: vec!["chatgpt-codex-connector".into()],
            bot_nudges: vec![],
            ..reviewers_gate_pr()
        };
        assert_eq!(async_wait_class(&pr, "abc", true), None);
    }

    #[test]
    fn reviewer_invocations_cover_the_descriptor_table() {
        // The parity script enforces this against the Python side in CI; this
        // keeps the Rust half self-consistent at unit-test speed.
        for (name, inv, self_cert, _per) in REVIEWER_INVOCATIONS {
            assert!(!inv.is_empty(), "{name} has no invocation");
            assert_eq!(
                reviewer_invocation_for(name, None),
                Some((*inv, *self_cert))
            );
        }
        assert_eq!(reviewer_invocation_for("teleport", None), None);
        // AC5: the ONE self-cert must stay visibly marked on this surface too.
        assert_eq!(
            reviewer_invocation_for("declare", None).map(|(_, sc)| sc),
            Some(true)
        );
        assert_eq!(
            reviewer_invocation_for("sigma", None).map(|(_, sc)| sc),
            Some(false)
        );
    }

    #[test]
    fn reviewer_invocation_resolves_the_author_harness_verb() {
        // Per-harness: code-review names the harness's own verb. A codex author
        // is told /review, a claude author /code-review; unknown harness and
        // override-less reviewers fall back to the scalar default.
        assert_eq!(
            reviewer_invocation_for("code-review", Some("codex")),
            Some(("/review", false))
        );
        assert_eq!(
            reviewer_invocation_for("code-review", Some("claude")),
            Some(("/code-review", false))
        );
        assert_eq!(
            reviewer_invocation_for("code-review", None),
            Some(("/code-review", false))
        );
        assert_eq!(
            reviewer_invocation_for("sigma", Some("codex")),
            Some(("/fno:review sigma", false))
        );
        // The codex self-review verb must stay bare: prose after it flips codex
        // to a no-merge-base review target (a verified constraint, not a style).
        let (codex_verb, _) = reviewer_invocation_for("code-review", Some("codex")).unwrap();
        assert!(
            !codex_verb.chars().any(|c| c.is_whitespace()),
            "codex self-review verb must be bare, got {codex_verb:?}"
        );
    }

    #[test]
    fn code_payload_classifies_code_and_docs() {
        // Code: source, config, lockfile, script. Docs: markdown, docs/.
        assert!(payload_is_code(&["cli/src/fno/x.py".into()]));
        assert!(payload_is_code(&["crates/fno-agents/src/lib.rs".into()]));
        assert!(payload_is_code(&[".fno/config.toml".into()]));
        assert!(payload_is_code(&["Cargo.lock".into()]));
        assert!(payload_is_code(&["scripts/ci/gate.sh".into()]));
        assert!(!payload_is_code(&["README.md".into()]));
        assert!(!payload_is_code(&[
            "docs/architecture/review-lanes.md".into()
        ]));
        assert!(!payload_is_code(&["docs/preflight.txt".into()]));
    }

    #[test]
    fn code_payload_empty_or_docs_only_diff_is_not_code() {
        // No diff -> no ship -> no gate. Docs-only -> unchanged behavior.
        assert!(!payload_is_code(&[]));
        assert!(!payload_is_code(&[
            "docs/a.md".into(),
            "CHANGELOG.md".into()
        ]));
    }

    #[test]
    fn code_payload_mixed_diff_is_code() {
        // One code file among docs is enough to carry a code payload.
        assert!(payload_is_code(&["docs/a.md".into(), "src/lib.rs".into()]));
    }

    #[test]
    fn self_review_gate_classifies_unreadable_diff_as_code() {
        // AC4-ERR: a git that cannot produce a diff fails CLOSED - code with
        // assumed=true - so a degraded probe cannot wave the obligation away.
        let (is_code, assumed) =
            classify_payload("definitely-not-a-real-git-binary", Path::new("."));
        assert!(is_code);
        assert!(assumed);
    }

    #[test]
    fn self_review_gate_floors_code_review_for_a_code_payload() {
        // AC1 floor: a code payload on a lane-less stock install floors
        // code-review onto the required set.
        assert_eq!(
            floor_self_review(&[], false, true, true),
            Some("code-review".to_string())
        );
    }

    #[test]
    fn self_review_gate_floor_respects_opt_out_lanes_docs_and_existing() {
        // AC6-CON: opt-out -> None.
        assert_eq!(floor_self_review(&[], false, true, false), None);
        // A configured lane -> None: the lane already expresses review intent.
        assert_eq!(floor_self_review(&[], true, true, true), None);
        // A docs payload -> None: nothing to review.
        assert_eq!(floor_self_review(&[], false, false, true), None);
        // code-review already named -> None: no double-add.
        assert_eq!(
            floor_self_review(&["code-review".to_string()], false, true, true),
            None
        );
        // A leading slash on an existing entry is still recognized as present.
        assert_eq!(
            floor_self_review(&["/code-review".to_string()], false, true, true),
            None
        );
    }

    #[test]
    fn self_review_gate_held_reason_names_code_review_and_its_verb() {
        // AC1-HP: a code payload that reaches the stop gate with no head-pinned
        // code-review attestation is held, and the reason names the reviewer
        // and the verb served by the ambient harness.
        let mut pr = reviewers_gate_pr();
        pr.unattested_reviewers[0].name = "code-review".to_string();
        let reason = build_block_reason(&pr, "abc", true);
        let harness = crate::claims::resolve_harness();
        let expected = reviewer_invocation_for("code-review", harness.as_deref())
            .expect("code-review descriptor")
            .0;
        assert!(reason.contains("reviewers gate unmet"), "got: {reason}");
        assert!(reason.contains("code-review"), "got: {reason}");
        assert!(reason.contains(&format!("`{expected}`")), "got: {reason}");
    }

    #[test]
    fn self_review_gate_pass_attestation_clears_code_review() {
        // AC2-HP: once a head-pinned code-review pass lands, the scan no longer
        // holds it - the floor is satisfiable by the self-serve route, not a wait.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("e.jsonl");
        std::fs::write(
            &p,
            r#"{"type":"review_attestation","data":{"reviewer":"code-review","head_sha":"h","verdict":"pass"}}"#,
        )
        .unwrap();
        let out = unattested_reviewers(&p, &["code-review".to_string()], "h");
        assert!(
            out.is_empty(),
            "code-review should clear on a pass: {out:?}"
        );
    }

    #[test]
    fn self_review_gate_only_floors_harnesses_with_a_verb() {
        // The floor wedges a harness whose native verb the session cannot run,
        // so it applies only where a self-review verb exists. Route 3 (a spawned
        // reviewer) is the path for harnesses without one and is deferred.
        assert!(harness_can_self_review(Some("claude")));
        assert!(harness_can_self_review(Some("codex")));
        assert!(!harness_can_self_review(Some("gemini")));
        assert!(!harness_can_self_review(Some("agy")));
        assert!(!harness_can_self_review(Some("opencode")));
        assert!(!harness_can_self_review(None));
    }

    #[test]
    fn graphql_exhausted_reason_names_reset_and_rest_lane() {
        // The message must make a session STOP retrying and say where the
        // answer still lives; "retrying next fire" is the advice it replaces.
        let q = GraphqlQuota {
            remaining: 0,
            reset_epoch: Utc::now().timestamp() + 40 * 60 + 5,
        };
        let msg = graphql_exhausted_reason(&q);
        assert!(msg.contains("GraphQL quota exhausted"), "got: {msg}");
        assert!(msg.contains("~40m"), "got: {msg}");
        assert!(msg.contains("fno pr status"), "got: {msg}");
        assert!(!msg.contains("retrying next fire"), "got: {msg}");
    }

    #[test]
    fn graphql_exhausted_reason_never_reports_a_past_reset() {
        let q = GraphqlQuota {
            remaining: 0,
            reset_epoch: Utc::now().timestamp() - 120,
        };
        assert!(graphql_exhausted_reason(&q).contains("~0m"));
    }

    fn write_exec(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    #[test]
    fn probe_graphql_quota_parses_the_graphql_bucket() {
        let tmp = tempfile::tempdir().unwrap();
        let gh = write_exec(
            tmp.path(),
            "gh",
            "#!/bin/sh\n[ \"$1\" = api ] && [ \"$2\" = rate_limit ] && \
             echo '{\"resources\":{\"graphql\":{\"remaining\":0,\"reset\":1750000000}}}' && exit 0\n\
             exit 1\n",
        );
        // Retry the spawn a few times: under a loaded CI runner (this crate's
        // suite forks hundreds of fake `gh`/`git` subprocesses in parallel),
        // `Command::output()` has measured an intermittent fork/exec failure
        // that has nothing to do with the parser under test - probe_graphql_
        // quota's own `.ok()?` already treats that as "unavailable, degrade
        // gracefully" in production, so retrying here absorbs the same
        // transient blip instead of failing the build on an infra hiccup.
        let mut q = None;
        for _ in 0..5 {
            q = probe_graphql_quota(gh.to_str().unwrap(), tmp.path());
            if q.is_some() {
                break;
            }
        }
        let q = q.expect("gh spawn kept failing across 5 retries - a real regression, not a blip");
        assert_eq!(q.remaining, 0);
        assert_eq!(q.reset_epoch, 1750000000);
    }

    #[test]
    fn probe_graphql_quota_failure_is_none_not_a_false_exhaustion() {
        // A failed probe must degrade to the transient wording, never
        // fabricate an exhaustion verdict that stalls a healthy session.
        let tmp = tempfile::tempdir().unwrap();
        let gh = write_exec(tmp.path(), "gh", "#!/bin/sh\nexit 1\n");
        assert!(probe_graphql_quota(gh.to_str().unwrap(), tmp.path()).is_none());
    }

    #[test]
    fn unwatched_async_nudge_review_uses_review_aware_watcher() {
        // codex P2: the review-wait watcher must poll REVIEW state, not
        // checks. It must also poll on REST - the GraphQL
        // reviews read is part of what exhausts the shared quota.
        let hint = arm_watch_hint(404, "review");
        assert!(hint.contains("pulls/404/reviews"), "got: {hint}");
        assert!(hint.contains("gh api"), "got: {hint}");
        assert!(!hint.contains("gh pr view"), "got: {hint}");
        assert!(hint.contains("sleep 60"), "got: {hint}");
        // The CI-wait watcher polls the REST status chokepoint for the
        // POSITIVE settled marker, never `gh pr checks --watch` (GraphQL).
        let ci_hint = arm_watch_hint(404, "ci");
        assert!(ci_hint.contains("fno pr status 404"), "got: {ci_hint}");
        assert!(!ci_hint.contains("gh pr checks"), "got: {ci_hint}");
        assert!(ci_hint.contains("'\"settled\": true'"), "got: {ci_hint}");
        assert!(ci_hint.contains("sleep 60"), "got: {ci_hint}");
    }

    #[test]
    fn watch_idle_rejects_head_mismatch() {
        // AC2-ERR: unpushed work (PR head != local HEAD) is never async-wait.
        assert_eq!(async_wait_class(&watch_pr(), "def", true), None);
    }

    #[test]
    fn watch_idle_rejects_ci_red() {
        // AC1-ERR: settled-red CI (no pending) blocks, never idles.
        let pr = PrInfo {
            ci_conclusion: CiConclusion::Failure(Some("unit".into())),
            ci_has_pending: false,
            ..watch_pr()
        };
        assert_eq!(async_wait_class(&pr, "abc", true), None);
    }

    #[test]
    fn watch_idle_rejects_unaddressed_finding() {
        // AC2-ERR: an unaddressed blocking inline finding is not async-wait.
        let pr = PrInfo {
            unaddressed_findings: vec![Finding {
                id: 1,
                author: "codex".into(),
                path: "a.rs".into(),
                line: 1,
                created_at: "none".into(),
                severity: "P1",
                had_reply: true,
            }],
            ..watch_pr()
        };
        assert_eq!(async_wait_class(&pr, "abc", true), None);
    }

    #[test]
    fn watch_idle_rejects_open_operator_finding() {
        // An open operator review_finding for the node also blocks idling.
        assert_eq!(async_wait_class(&watch_pr(), "abc", false), None);
    }

    #[test]
    fn watch_idle_rejects_non_open_pr() {
        // A merged/closed PR is not an async wait (green+merged is DonePRGreen).
        let pr = PrInfo {
            state: PrState::Merged,
            ..watch_pr()
        };
        assert_eq!(async_wait_class(&pr, "abc", true), None);
    }

    #[test]
    fn watch_idle_window_defaults_clamps_and_slacks() {
        // Default (no tag timeout): 30m + 12m slack.
        assert_eq!(watch_window_ms(None), 30 * 60_000 + WATCH_SLACK_MS);
        // Honored within range.
        assert_eq!(watch_window_ms(Some("30m")), 30 * 60_000 + WATCH_SLACK_MS);
        // Below the 5m floor clamps up.
        assert_eq!(watch_window_ms(Some("1m")), 5 * 60_000 + WATCH_SLACK_MS);
        // Above the 2h ceiling clamps down.
        assert_eq!(watch_window_ms(Some("5h")), 2 * 3_600_000 + WATCH_SLACK_MS);
        // Garbage falls back to the default.
        assert_eq!(watch_window_ms(Some("soon")), 30 * 60_000 + WATCH_SLACK_MS);
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
    fn watch_idle_event_is_non_terminal_allow() {
        // AC1-HP invariant: the idle branch emits allow + null termination, so
        // the stop-hook shim (which runs finalize only on a NON-null
        // termination_reason) never invokes finalize / stamps the ledger /
        // graduates a plan on an idle fire. This is the exact output shape the
        // idle branch returns.
        let json = allow_output(
            "allow",
            None,
            "watching: idling until watcher fires (PR #404, ci pending)",
            3,
            Some("sha|OPEN|PENDING|none".to_string()),
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["decision"], "allow");
        assert!(
            v["termination_reason"].is_null(),
            "idle-allow MUST be non-terminal or finalize would run"
        );
        assert!(v["message"].as_str().unwrap().contains("watching"));
    }

    #[test]
    fn termination_reason_variant_names_byte_identical() {
        // Fix 6: all TerminationReason variants must serialize to the exact strings
        // the spec names - no rename attributes applied.
        let cases = [
            (TerminationReason::DonePRGreen, "DonePRGreen"),
            (TerminationReason::DoneAdvisory, "DoneAdvisory"),
            (TerminationReason::DoneAwaitingReview, "DoneAwaitingReview"),
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
    fn identity_free_peer_uses_local_attestation_not_a_login() {
        let s = Settings {
            github_apps: Some(Vec::new()),
            peers: vec![PeerEntry {
                provider: "gemini".into(),
                model: None,
                identity: None,
            }],
            peer_identity: None,
            ..Default::default()
        };
        assert!(resolved_required_bots_for_author(&s, Some("codex")).is_empty());
        assert_eq!(
            resolved_local_peer_reviewers_for_author(&s, Some("codex")),
            vec![LOCAL_PEER_REVIEWER.to_string()]
        );
    }

    #[test]
    fn identity_free_same_model_peer_is_an_unsatisfiable_local_gate() {
        let s = Settings {
            peers: vec![PeerEntry {
                provider: "codex".into(),
                model: None,
                identity: None,
            }],
            ..Default::default()
        };
        assert_eq!(
            resolved_local_peer_reviewers_for_author(&s, Some("codex")),
            vec![SAME_MODEL_LOCAL_PEER_SENTINEL.to_string()]
        );
    }

    #[test]
    fn identity_free_mixed_peers_form_one_composite_gate() {
        let s = Settings {
            peers: vec![
                PeerEntry {
                    provider: "codex".into(),
                    model: None,
                    identity: None,
                },
                PeerEntry {
                    provider: "claude".into(),
                    model: Some("zai,glm-5.2".into()),
                    identity: None,
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            resolved_local_peer_reviewers_for_author(&s, Some("codex")),
            vec![LOCAL_PEER_REVIEWER.to_string()]
        );
    }

    #[test]
    fn explicit_peer_identity_keeps_login_gate_only() {
        let s = Settings {
            peers: vec![PeerEntry {
                provider: "gemini".into(),
                model: None,
                identity: Some("fno-gemini-bot".into()),
            }],
            ..Default::default()
        };
        assert_eq!(
            resolved_required_bots_for_author(&s, Some("codex")),
            vec!["fno-gemini-bot".to_string()]
        );
        assert!(resolved_local_peer_reviewers_for_author(&s, Some("codex")).is_empty());
    }

    #[test]
    fn local_peer_attestation_is_head_pinned() {
        let td = tempfile::tempdir().unwrap();
        let events = td.path().join("events.jsonl");
        std::fs::write(
            &events,
            r#"{"type":"review_attestation","data":{"reviewer":"peer","head_sha":"OLD","verdict":"pass"}}"#,
        )
        .unwrap();
        let peer = vec![LOCAL_PEER_REVIEWER.to_string()];
        assert!(!reviewers_all_attested(&events, &peer, "NEW"));
        std::fs::write(
            &events,
            r#"{"type":"review_attestation","data":{"reviewer":"peer","head_sha":"NEW","verdict":"pass"}}"#,
        )
        .unwrap();
        assert!(reviewers_all_attested(&events, &peer, "NEW"));
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
        let info = compute_review_info(&json, &required, &|_| Freshness::Fresh);
        assert!(!info.all_required_passed());
        assert_eq!(info.missing_bots, vec!["gemini-code-assist".to_string()]);
        assert_eq!(info.latest_ts, "2026-06-05T01:00:00Z");
    }

    // ── x-b167 nudge state ────────────────────────────────────────────────────

    fn nudge_cfg() -> NudgeConfig {
        NudgeConfig {
            login: "chatgpt-codex-connector".into(),
            review_handle: "@codex review".into(),
            wait_minutes: 15,
            ceiling: 3,
        }
    }
    fn nudge_now() -> DateTime<Utc> {
        "2026-07-06T02:00:00Z".parse().unwrap()
    }
    fn mention(body: &str, created: &str) -> Value {
        serde_json::json!({"body": body, "createdAt": created})
    }

    #[test]
    fn nudge_needs_nudge_when_never_mentioned() {
        let cfg = nudge_cfg();
        let b = classify_bot_nudge("chatgpt-codex-connector", &[], Some(&cfg), nudge_now());
        assert_eq!(b.class, NudgeClass::NeedsNudge);
        assert_eq!(b.nudges, 0);
        assert_eq!(b.review_handle, "@codex review");
    }

    #[test]
    fn nudge_awaiting_within_window() {
        let cfg = nudge_cfg();
        let comments = vec![mention("@codex review", "2026-07-06T01:58:00Z")];
        let b = classify_bot_nudge(
            "chatgpt-codex-connector",
            &comments,
            Some(&cfg),
            nudge_now(),
        );
        assert_eq!(b.class, NudgeClass::Awaiting);
        assert_eq!(b.nudges, 1);
        assert!(b.newest_age_min <= 2);
    }

    #[test]
    fn nudge_unresponsive_after_ceiling() {
        // AC3 building block: 3 mentions, newest older than wait_minutes.
        let cfg = nudge_cfg();
        let comments = vec![
            mention("@codex review", "2026-07-06T00:00:00Z"),
            mention("hey @codex review please", "2026-07-06T00:30:00Z"),
            mention("@codex review", "2026-07-06T01:00:00Z"),
        ];
        let b = classify_bot_nudge(
            "chatgpt-codex-connector",
            &comments,
            Some(&cfg),
            nudge_now(),
        );
        assert_eq!(b.class, NudgeClass::Unresponsive);
        assert_eq!(b.nudges, 3);
        assert!(b.span_min >= 120, "span was {}", b.span_min);
    }

    #[test]
    fn nudge_reask_after_timeout_below_ceiling() {
        // One mention 60m ago, ceiling 3: the previous nudge timed out, ask again.
        let cfg = nudge_cfg();
        let comments = vec![mention("@codex review", "2026-07-06T01:00:00Z")];
        let b = classify_bot_nudge(
            "chatgpt-codex-connector",
            &comments,
            Some(&cfg),
            nudge_now(),
        );
        assert_eq!(b.class, NudgeClass::NeedsNudge);
        assert_eq!(b.nudges, 1);
    }

    #[test]
    fn nudge_none_cfg_is_not_nudgeable() {
        // AC7: a peer-login sentinel classifies NotNudgeable.
        let b2 = classify_bot_nudge(SAME_MODEL_PEER_SENTINEL, &[], None, nudge_now());
        assert_eq!(b2.class, NudgeClass::NotNudgeable);
    }

    #[test]
    fn nudge_malformed_created_at_is_needs_nudge() {
        // A mention with an unparseable createdAt must not push toward Unresponsive.
        let cfg = nudge_cfg();
        let comments = vec![mention("@codex review", "not-a-date")];
        let b = classify_bot_nudge(
            "chatgpt-codex-connector",
            &comments,
            Some(&cfg),
            nudge_now(),
        );
        assert_eq!(b.class, NudgeClass::NeedsNudge);
        assert_eq!(b.nudges, 1);
    }

    #[test]
    fn resolved_nudge_configs_default_nudges_codex_only() {
        let cfgs = resolved_nudge_configs(&Settings::default());
        let codex = cfgs
            .iter()
            .find(|c| c.login == "chatgpt-codex-connector")
            .expect("codex nudgeable by default");
        assert_eq!(codex.review_handle, "@codex review");
        assert_eq!(codex.wait_minutes, 15);
        assert_eq!(codex.ceiling, 3);
        // gemini ships with an empty review_handle -> not nudgeable.
        assert!(cfgs.iter().all(|c| c.login != "gemini-code-assist"));
    }

    #[test]
    fn nudge_override_sets_wait_and_ceiling_inheriting_handle() {
        let s = parse_settings(
            "[review.nudge]\n\"chatgpt-codex-connector\" = { wait_minutes = 30, ceiling = 5 }\n",
        );
        let cfgs = resolved_nudge_configs(&s);
        let codex = cfgs
            .iter()
            .find(|c| logins_correspond(&c.login, "chatgpt-codex-connector"))
            .unwrap();
        assert_eq!(codex.wait_minutes, 30);
        assert_eq!(codex.ceiling, 5);
        assert_eq!(codex.review_handle, "@codex review");
    }

    #[test]
    fn nudge_override_disabled_removes_login() {
        let s =
            parse_settings("[review.nudge]\n\"chatgpt-codex-connector\" = { enabled = false }\n");
        let cfgs = resolved_nudge_configs(&s);
        assert!(cfgs
            .iter()
            .all(|c| !logins_correspond(&c.login, "chatgpt-codex-connector")));
    }

    #[test]
    fn nudge_override_new_login() {
        let s = parse_settings(
            "[review.nudge]\n\"some-bot\" = { review_handle = \"@somebot review\", wait_minutes = 10, ceiling = 2 }\n",
        );
        let cfgs = resolved_nudge_configs(&s);
        let b = cfgs.iter().find(|c| c.login == "some-bot").unwrap();
        assert_eq!(b.review_handle, "@somebot review");
        assert_eq!(b.wait_minutes, 10);
        assert_eq!(b.ceiling, 2);
    }

    #[test]
    fn nudge_malformed_override_degrades_to_non_nudgeable() {
        // AC8: a scalar, a list, and a non-integer wait_minutes each drop the
        // login to non-nudgeable without panicking.
        for body in [
            "[review.nudge]\n\"chatgpt-codex-connector\" = \"scalar\"\n",
            "[review.nudge]\n\"chatgpt-codex-connector\" = [1, 2]\n",
            "[review.nudge]\n\"chatgpt-codex-connector\" = { wait_minutes = \"soon\" }\n",
            // An absurd wait_minutes would overflow chrono::Duration::minutes and
            // panic the stop gate; it must degrade to non-nudgeable, not panic.
            "[review.nudge]\n\"chatgpt-codex-connector\" = { wait_minutes = 9999999999999999 }\n",
        ] {
            let s = parse_settings(body);
            let cfgs = resolved_nudge_configs(&s);
            assert!(
                cfgs.iter()
                    .all(|c| !logins_correspond(&c.login, "chatgpt-codex-connector")),
                "malformed override must be non-nudgeable: {body}"
            );
        }
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
        let info = compute_review_info(&json, &required, &|_| Freshness::Fresh);
        assert!(!info.all_required_passed());
    }

    #[test]
    fn compute_review_info_usage_limited_bot_blocks_gate() {
        // x-9ab2: a required bot that posted only a usage-limit (quota) comment,
        // never a review, is detected as rate-limited (moved to usage_limited,
        // out of missing_bots) AND must FAIL the gate closed: a quota bounce is
        // not a review, so all_required_passed is false and the PR does not
        // merge. Reverting all_required_passed to `missing_bots.is_empty()`
        // alone makes this assertion fail - that is the regression guard.
        let required = vec!["chatgpt-codex-connector".to_string()];
        let json = serde_json::json!({
            "reviews": [],
            "comments": [
                {"author": {"login": "chatgpt-codex-connector"},
                 "body": "You have reached your Codex usage limits for code reviews.",
                 "createdAt": "2026-07-06T01:00:00Z"}
            ]
        });
        let info = compute_review_info(&json, &required, &|_| Freshness::Fresh);
        // Detection still holds: the bot is classified rate-limited, not missing.
        assert!(info.missing_bots.is_empty());
        assert_eq!(
            info.usage_limited,
            vec!["chatgpt-codex-connector".to_string()]
        );
        // The gate decision: a usage-limit body does NOT satisfy the gate.
        assert!(
            !info.all_required_passed(),
            "a usage-limit comment must not satisfy the review gate"
        );
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
        let info = compute_review_info(&json, &required, &|_| Freshness::Fresh);
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
        let info = compute_review_info(&json, &required, &|_| Freshness::Fresh);
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

#[cfg(test)]
mod done_probe_tests {
    use super::*;
    use std::time::Duration;

    fn fm(body: &str) -> String {
        format!("---\ntitle: t\n{body}\n---\n\n# doc\n")
    }

    fn probes_of(doc: &str) -> Vec<String> {
        match parse_done_probes(doc) {
            ProbeDecl::Probes(p) => p,
            other => panic!("expected probes, got {other:?}"),
        }
    }

    #[test]
    fn parses_block_list() {
        let doc = fm("done_probes:\n  - \"fno mail list --since 24h | grep -q groom\"\n  - 'echo ok'\nstatus: ready");
        assert_eq!(
            probes_of(&doc),
            vec![
                "fno mail list --since 24h | grep -q groom".to_string(),
                "echo ok".to_string()
            ]
        );
    }

    #[test]
    fn parses_inline_list_keeping_commas_inside_commands() {
        let doc = fm(r#"done_probes: ["gh api x --jq '.a,.b'", "echo ok"]"#);
        assert_eq!(
            probes_of(&doc),
            vec!["gh api x --jq '.a,.b'".to_string(), "echo ok".to_string()],
            "a comma inside a quoted command must not split it into two probes"
        );
    }

    #[test]
    fn absent_field_and_explicit_empty_list_are_both_no_gate() {
        assert_eq!(parse_done_probes(&fm("done_probes: []")), ProbeDecl::None);
        assert_eq!(parse_done_probes(&fm("status: ready")), ProbeDecl::None);
        assert_eq!(parse_done_probes("no frontmatter here"), ProbeDecl::None);
    }

    #[test]
    fn a_plain_scalar_is_one_probe_but_a_block_scalar_refuses() {
        // The plan schema advertises `str | list` for close_probes/done_probes,
        // so a scalar must EVALUATE, not refuse - refusing turned a legal
        // declaration into an unevaluable gate at the close verbs.
        assert_eq!(
            parse_done_probes(&fm("done_probes: \"echo ok\"")),
            ProbeDecl::Probes(vec!["echo ok".to_string()])
        );
        // A YAML block scalar's value lives on the following lines; this parser
        // cannot read it, so it stays fail-closed.
        assert_eq!(
            parse_done_probes(&fm("done_probes: |\n  echo ok")),
            ProbeDecl::Unparseable
        );
    }

    #[test]
    fn a_declaration_this_parser_cannot_read_is_never_no_gate() {
        // The vacuous-pass shape: the field is there, so the plan MEANT to gate.
        // Reporting None here would silently drop the gate entirely.
        let multiline_inline = fm("done_probes: [\n  \"echo a\",\n  \"echo b\"\n]");
        assert_eq!(parse_done_probes(&multiline_inline), ProbeDecl::Unparseable);
        assert_eq!(
            parse_done_probes(&fm("done_probes:\nstatus: ready")),
            ProbeDecl::Unparseable,
            "a declared-but-empty block must refuse, not pass"
        );
    }

    #[test]
    fn inline_list_keeps_escaped_quotes_inside_a_command() {
        // A mis-parsed probe is worse than a refused one: it would run a
        // DIFFERENT command than the plan declared and gate on its result.
        let doc = fm(r#"done_probes: ["sh -c \"echo hi\"", "echo ok"]"#);
        assert_eq!(
            probes_of(&doc),
            vec![r#"sh -c "echo hi""#.to_string(), "echo ok".to_string()]
        );
    }

    #[test]
    fn inline_list_preserves_a_trailing_bracket_and_refuses_an_unterminated_one() {
        assert_eq!(
            probes_of(&fm(r#"done_probes: ["echo [hi]"]"#)),
            vec!["echo [hi]".to_string()],
            "only the list's own closing bracket may be stripped"
        );
        assert_eq!(
            parse_done_probes(&fm(r#"done_probes: ["echo a""#)),
            ProbeDecl::Unparseable,
            "an unterminated inline list must refuse, not silently parse"
        );
    }

    #[test]
    fn a_comment_inside_the_block_does_not_swallow_the_probes() {
        let doc = fm("done_probes:\n  # why this probe exists\n  - echo a\n  - echo b\ntags: []");
        assert_eq!(
            probes_of(&doc),
            vec!["echo a".to_string(), "echo b".to_string()]
        );
    }

    #[test]
    fn block_list_stops_at_the_next_key() {
        let doc = fm("done_probes:\n  - echo a\ntags: []\nother: x");
        assert_eq!(probes_of(&doc), vec!["echo a".to_string()]);
    }

    #[test]
    fn probe_outcomes_render_pass_fail_and_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Duration::from_secs(10);
        assert_eq!(run_probe("exit 0", tmp.path(), t).render(), "pass");
        assert_eq!(run_probe("exit 3", tmp.path(), t).render(), "fail:3");
        assert_eq!(
            run_probe("fno-no-such-binary-xyz", tmp.path(), t).render(),
            "fail:127",
            "a missing binary must fail closed as 127, never pass"
        );
    }

    /// The close-probe gate reads its OWN key, not done_probes. One parser,
    /// two keys; this pins that the parameterization did not silently collapse
    /// both onto done_probes.
    #[test]
    fn parse_probes_for_reads_close_probes_key() {
        let doc = fm("close_probes:\n  - \"exit 0\"\ndone_probes:\n  - \"exit 1\"\nstatus: ready");
        assert_eq!(
            probes_for(&doc, "close_probes"),
            vec!["exit 0".to_string()],
            "close_probes must read the close_probes list, not done_probes"
        );
        assert_eq!(probes_for(&doc, "done_probes"), vec!["exit 1".to_string()]);
    }

    /// `probe-run` exit contract: 0 passes, 1 fails a probe, 2 is undeterminable
    /// (unparseable / unreadable). stdout is always JSON.
    #[test]
    fn probe_run_exit_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let passing = tmp.path().join("pass.md");
        std::fs::write(&passing, fm("close_probes:\n  - \"exit 0\"")).unwrap();
        let failing = tmp.path().join("fail.md");
        std::fs::write(&failing, fm("close_probes:\n  - \"exit 7\"")).unwrap();
        let bad = tmp.path().join("bad.md");
        std::fs::write(&bad, fm("close_probes: [unterminated")).unwrap();

        let (code, json) = decide_probe_run(&[
            "--plan".into(),
            passing.to_string_lossy().into(),
            "--key".into(),
            "close_probes".into(),
            "--cwd".into(),
            tmp.path().to_string_lossy().into(),
            "--json".into(),
        ]);
        assert_eq!(code, 0);
        assert_eq!(
            serde_json::from_str::<Value>(&json).unwrap()["passed"],
            true
        );

        let (code, json) = decide_probe_run(&[
            "--plan".into(),
            failing.to_string_lossy().into(),
            "--key".into(),
            "close_probes".into(),
            "--cwd".into(),
            tmp.path().to_string_lossy().into(),
        ]);
        assert_eq!(code, 1);
        let body = serde_json::from_str::<Value>(&json).unwrap();
        assert_eq!(body["passed"], false);
        assert!(body["reason"].as_str().unwrap().contains("exited 7"));

        // Unparseable declaration: undeterminable, fail closed.
        let (code, _) = decide_probe_run(&[
            "--plan".into(),
            bad.to_string_lossy().into(),
            "--key".into(),
            "close_probes".into(),
        ]);
        assert_eq!(code, 2);

        // Unreadable plan: undeterminable, fail closed.
        let (code, _) = decide_probe_run(&[
            "--plan".into(),
            tmp.path().join("nope.md").to_string_lossy().into(),
            "--key".into(),
            "close_probes".into(),
        ]);
        assert_eq!(code, 2);
    }

    fn probes_for(doc: &str, key: &str) -> Vec<String> {
        match parse_probes_for(doc, key) {
            ProbeDecl::Probes(p) => p,
            other => panic!("expected probes for {key}, got {other:?}"),
        }
    }

    #[test]
    fn hanging_probe_is_killed_within_the_timeout_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let outcome = run_probe("sleep 30", tmp.path(), Duration::from_millis(200));
        assert_eq!(outcome.render(), "timeout");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "run_probe must return on its own timeout, not wait out the child"
        );
    }

    #[test]
    fn chatty_probe_does_not_deadlock_on_the_stderr_pipe() {
        // A probe writing past the 64KB pipe buffer would hang forever if
        // stderr were drained only after exit.
        let tmp = tempfile::tempdir().unwrap();
        let outcome = run_probe(
            "head -c 200000 /dev/zero | tr '\\0' 'x' >&2; exit 1",
            tmp.path(),
            Duration::from_secs(20),
        );
        assert_eq!(outcome.render(), "fail:1");
        match outcome {
            ProbeOutcome::Fail { stderr, .. } => assert!(
                stderr.len() <= PROBE_STDERR_CAP,
                "stderr must be truncated to {PROBE_STDERR_CAP}"
            ),
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn over_cap_declaration_refuses_without_running_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("plan.md");
        let sentinel = tmp.path().join("ran");
        std::fs::write(
            &plan,
            fm(&format!(
                "done_probes:\n  - touch {0}\n  - echo b\n  - echo c\n  - echo d",
                sentinel.display()
            )),
        )
        .unwrap();
        let events = tmp.path().join("events.jsonl");
        match evaluate_done_probes(
            plan.to_str(),
            None,
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Fail { reason, .. } => {
                assert!(
                    reason.contains("cap is 3"),
                    "reason names the cap: {reason}"
                )
            }
            _ => panic!("over-cap declaration must refuse"),
        }
        assert!(!sentinel.exists(), "an over-cap list must not execute");
    }

    #[test]
    fn unreadable_plan_fails_closed_only_when_probes_were_seen_before() {
        let tmp = tempfile::tempdir().unwrap();
        let events = tmp.path().join("events.jsonl");
        let missing = tmp.path().join("gone.md");

        // AC2-FR: no probe history -> today's behavior exactly.
        assert!(matches!(
            evaluate_done_probes(
                missing.to_str(),
                None,
                tmp.path(),
                &events,
                "s1",
                Duration::from_secs(10)
            ),
            ProbeGate::Absent
        ));

        // AC1-FR: a prior fire recorded probes -> undeterminable, fail closed.
        std::fs::write(
            &events,
            "{\"type\":\"loop_check\",\"data\":{\"session_id\":\"s1\",\"done_probes\":{\"echo ok\":\"pass\"}}}\n",
        )
        .unwrap();
        match evaluate_done_probes(
            missing.to_str(),
            None,
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Fail { reason, .. } => assert!(
                reason.contains("undeterminable"),
                "reason must say undeterminable: {reason}"
            ),
            _ => panic!("unreadable plan with probe history must fail closed"),
        }
    }

    #[test]
    fn a_refusal_where_nothing_ran_still_records_probe_history() {
        // Otherwise prior_fires_declared_probes sees no history, and a plan that
        // tripped the cap and then went missing degrades to "no gate".
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("plan.md");
        std::fs::write(
            &plan,
            fm("done_probes:\n  - echo a\n  - echo b\n  - echo c\n  - echo d"),
        )
        .unwrap();
        let events = tmp.path().join("events.jsonl");
        let ProbeGate::Fail { results, .. } = evaluate_done_probes(
            plan.to_str(),
            None,
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) else {
            panic!("over-cap must refuse");
        };
        std::fs::write(
            &events,
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "loop_check",
                    "data": {"session_id": "s1", "done_probes": results}
                })
            ),
        )
        .unwrap();
        assert!(
            prior_fires_declared_probes(&events, "s1"),
            "a declared-but-never-ran refusal must be visible as probe history"
        );
    }

    #[test]
    fn relative_plan_path_resolves_against_the_session_cwd() {
        // plan_path is repo-relative in practice; resolving against the process
        // cwd would read nothing and silently drop the gate.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("plan.md"), fm("done_probes:\n  - exit 0")).unwrap();
        let events = tmp.path().join("events.jsonl");
        assert!(
            matches!(
                evaluate_done_probes(
                    Some("plan.md"),
                    None,
                    tmp.path(),
                    &events,
                    "s1",
                    Duration::from_secs(10)
                ),
                ProbeGate::Pass(_)
            ),
            "a relative plan_path must resolve against cwd, not the process cwd"
        );
    }

    #[test]
    fn timeout_reaches_the_gate_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("plan.md");
        std::fs::write(&plan, fm("done_probes:\n  - sleep 30")).unwrap();
        let events = tmp.path().join("events.jsonl");
        match evaluate_done_probes(
            plan.to_str(),
            None,
            tmp.path(),
            &events,
            "s1",
            Duration::from_millis(200),
        ) {
            ProbeGate::Fail { reason, results } => {
                assert!(
                    reason.contains("timed out"),
                    "reason names the timeout: {reason}"
                );
                assert_eq!(results["sleep 30"], "timeout");
            }
            _ => panic!("a hanging probe must refuse done"),
        }
    }

    #[test]
    fn a_pipeline_probe_timeout_does_not_hang_the_gate() {
        // `sh -c "a | b"` forks: killing only sh leaves grandchildren holding
        // the stderr pipe, so the drain thread never sees EOF. This is the
        // documented probe shape, so a regression here wedges every session.
        let tmp = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let outcome = run_probe("sleep 30 | cat", tmp.path(), Duration::from_millis(200));
        assert_eq!(outcome.render(), "timeout");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "a pipeline probe must not outlive its timeout (took {:?})",
            start.elapsed()
        );
    }

    #[test]
    fn multibyte_stderr_is_truncated_without_panicking() {
        // String::drain panics off a char boundary; probe stderr routinely
        // carries arrows and box-drawing characters.
        let mut s = "→".repeat(400); // 3 bytes each, straddles the cut
        keep_last_on_char_boundary(&mut s, PROBE_STDERR_CAP);
        assert!(s.len() <= PROBE_STDERR_CAP);
        assert!(s.chars().all(|c| c == '→'), "must not split a character");
    }

    #[test]
    fn stderr_cap_keeps_the_tail_where_the_error_is() {
        let mut s = format!("{}\nthe actual error", "noise ".repeat(200));
        keep_last_on_char_boundary(&mut s, PROBE_STDERR_CAP);
        assert!(
            s.ends_with("the actual error"),
            "the last line is the diagnostic; keeping the prefix drops it: {s}"
        );
    }

    #[test]
    fn block_scalar_escapes_decode_to_the_command_the_plan_meant() {
        // Leaving `\"` in would hand sh a DIFFERENT command than declared, and
        // would key the event by a string the PyYAML-side grader never matches.
        let doc = fm("done_probes:\n  - \"test -n \\\"$(echo hi)\\\"\"");
        assert_eq!(probes_of(&doc), vec![r#"test -n "$(echo hi)""#.to_string()]);
    }

    #[test]
    fn single_quoted_scalar_undoubles_its_quote() {
        let doc = fm("done_probes:\n  - 'echo it''s fine'");
        assert_eq!(probes_of(&doc), vec!["echo it's fine".to_string()]);
    }

    #[test]
    fn plan_path_fragment_is_stripped_before_reading() {
        // `plans/p.md#wave-1` must resolve to plans/p.md, not a literal filename
        // containing the fragment (which would read nothing -> silent Absent).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("plan.md"), fm("done_probes:\n  - exit 0")).unwrap();
        let events = tmp.path().join("events.jsonl");
        assert!(
            matches!(
                evaluate_done_probes(
                    Some("plan.md#wave-1"),
                    None,
                    tmp.path(),
                    &events,
                    "s1",
                    Duration::from_secs(10)
                ),
                ProbeGate::Pass(_)
            ),
            "a fragment in plan_path must not silently disable the gate"
        );
    }

    #[test]
    fn a_backgrounding_probe_does_not_block_the_drain() {
        // sh exits immediately while the descendant keeps stderr open, so the
        // timeout loop is already over and only the group kill bounds the join.
        let tmp = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let outcome = run_probe("sleep 300 & exit 0", tmp.path(), Duration::from_secs(30));
        assert_eq!(outcome.render(), "pass");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "a backgrounded descendant must not hold the drain open (took {:?})",
            start.elapsed()
        );
    }

    // ── project-level done_probes (x-a534) ────────────────────────────────
    //
    // A repo-wide guardrail must apply to every plan in the repo, and no plan
    // doc may switch it off - a guard on one of two reachable paths is
    // decorative.

    fn project(cmds: &[&str]) -> Result<Vec<String>, String> {
        Ok(cmds.iter().map(|c| c.to_string()).collect())
    }

    /// A plan doc that declares no probes of its own.
    fn bare_plan(dir: &Path) -> std::path::PathBuf {
        let plan = dir.join("plan.md");
        std::fs::write(&plan, fm("title: p")).unwrap();
        plan
    }

    #[test]
    fn a_project_probe_gates_a_plan_that_declares_none() {
        // AC1-HP: the repo-wide guardrail runs without being retyped per plan,
        // and its result reaches the event payload.
        let tmp = tempfile::tempdir().unwrap();
        let plan = bare_plan(tmp.path());
        let events = tmp.path().join("events.jsonl");
        match evaluate_done_probes(
            plan.to_str(),
            Some(&project(&["true"])),
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Pass(results) => assert_eq!(results["true"], "pass"),
            _ => panic!("a passing project probe must let the gate through"),
        }
    }

    #[test]
    fn a_failing_project_probe_blocks_and_names_its_source() {
        // AC2-ERR: `probe X exited 1` is ambiguous once there are two
        // declarations; the operator has to know which file to edit.
        let tmp = tempfile::tempdir().unwrap();
        let plan = bare_plan(tmp.path());
        let events = tmp.path().join("events.jsonl");
        match evaluate_done_probes(
            plan.to_str(),
            Some(&project(&["false"])),
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Fail { reason, .. } => assert!(
                reason.contains("project probe `false`"),
                "the reason must name the source: {reason}"
            ),
            _ => panic!("a failing project probe must block"),
        }
    }

    // AC3-INV (a plan declaring `done_probes: []` cannot silence the project's
    // gate) is covered end to end by
    // done_probes_ac3_inv_a_plan_cannot_silence_the_project_gate in
    // tests/loop_check.rs, which drives the real settings merge rather than a
    // hand-built probe list. A unit-level twin would assert strictly less.

    #[test]
    fn an_unparseable_project_declaration_blocks_rather_than_degrading() {
        // AC4-ERR: a config key that degrades to no-gate is a guardrail that
        // disappears when you typo it.
        let tmp = tempfile::tempdir().unwrap();
        let plan = bare_plan(tmp.path());
        let events = tmp.path().join("events.jsonl");
        let junk: Result<Vec<String>, String> = value_as_probe_list(
            &"done_probes = { a = 1 }".parse::<toml::Value>().unwrap()["done_probes"],
        );
        assert!(junk.is_err(), "a mapping is not a probe list");
        match evaluate_done_probes(
            plan.to_str(),
            Some(&junk),
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Fail { reason, results } => {
                assert!(
                    reason.contains("undeterminable"),
                    "must use the plan side's vocabulary: {reason}"
                );
                assert_eq!(results["_undeterminable"], "unparseable-config-declaration");
            }
            _ => panic!("an unreadable project declaration must block"),
        }
    }

    #[test]
    fn the_cap_is_per_source_not_per_union() {
        // AC5-BOUND: 3 + 3 all run. Sharing one budget would make two
        // independent authors compete for one number, so a project policy
        // would eat a plan's operational probes.
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("plan.md");
        std::fs::write(
            &plan,
            fm("done_probes:\n  - echo d\n  - echo e\n  - echo f"),
        )
        .unwrap();
        let events = tmp.path().join("events.jsonl");
        match evaluate_done_probes(
            plan.to_str(),
            Some(&project(&["echo a", "echo b", "echo c"])),
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Pass(results) => assert_eq!(
                results.as_object().unwrap().len(),
                6,
                "all six probes must run: {results}"
            ),
            other => panic!(
                "3 + 3 is within the per-source cap: {}",
                match other {
                    ProbeGate::Fail { reason, .. } => reason,
                    _ => "Absent".to_string(),
                }
            ),
        }

        // A 4th in the project declaration is still a loud refusal.
        match evaluate_done_probes(
            plan.to_str(),
            Some(&project(&["true", "true", "true", "true"])),
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Fail { reason, .. } => assert!(
                reason.contains("config.toml declares 4") && reason.contains("per source"),
                "an over-cap project list must refuse loudly: {reason}"
            ),
            _ => panic!("4 project probes must refuse"),
        }
    }

    #[test]
    fn no_declaration_on_either_source_stays_absent() {
        // The zero-subprocess path must survive the second source.
        let tmp = tempfile::tempdir().unwrap();
        let plan = bare_plan(tmp.path());
        let events = tmp.path().join("events.jsonl");
        assert!(matches!(
            evaluate_done_probes(
                plan.to_str(),
                Some(&project(&[])),
                tmp.path(),
                &events,
                "s1",
                Duration::from_secs(10)
            ),
            ProbeGate::Absent
        ));
    }

    #[test]
    fn config_done_probes_parses_off_the_flat_root() {
        // The file is flat: `done_probes` at the root, not nested under a
        // `config` table.
        let s = parse_settings("done_probes = [\"make a11y-check\"]\n");
        assert_eq!(s.done_probes, Some(Ok(vec!["make a11y-check".to_string()])),);
        assert_eq!(parse_settings("plans_dir = \"x\"\n").done_probes, None);
        assert!(parse_settings("done_probes = \"nope\"\n")
            .done_probes
            .unwrap()
            .is_err());
        assert!(parse_settings("done_probes = [1]\n")
            .done_probes
            .unwrap()
            .is_err());
    }
}
