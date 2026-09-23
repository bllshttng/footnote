//! What did the manifest and config say? Manifest/target-state frontmatter parsing, Settings/config.toml parsing, and session ledger cost parsing.

use super::*;

/// Fields parsed from target-state.md YAML frontmatter.
#[derive(Debug)]
pub(super) struct Manifest {
    pub(super) session_id: Option<String>,
    /// The harness session that ran `fno do target init` in this worktree
    /// (claude UUID / codex thread / etc). Distinct from `session_id`, which is
    /// the target run id: the two differ, and this is the value an attestation's
    /// attester_session_id is compared against to detect self-attestation.
    pub(super) harness_session_id: Option<String>,
    pub(super) created_at: Option<String>,
    pub(super) attended: bool, // default true when absent
    pub(super) advisory: bool,
    pub(super) no_ship: bool,
    pub(super) no_external: bool,
    /// batch-lane member: commits ship via the batch PR, not a per-node PR.
    pub(super) batched: bool,
    /// plan-only thread: reaches the plan boundary and terminates DonePlanned
    /// (not DoneAdvisory, which would graduate the plan).
    pub(super) planned: bool,
    /// Plan doc backing this session; source of the `done_probes` declaration.
    pub(super) plan_path: Option<String>,
    pub(super) legacy_status: Option<String>, // COMPLETE | BLOCKED | ABORTED
    /// None = absent (unlimited). Some(Ok(v)) = valid cap. Some(Err(s)) = malformed raw value.
    pub(super) budget_wall_clock_cap_minutes: Option<Result<u64, String>>,
    /// None = absent (unlimited). Some(Ok(v)) = valid cap. Some(Err(s)) = malformed raw value.
    pub(super) budget_cost_cap_usd: Option<Result<f64, String>>,
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
/// the frontmatter block. `fno do target init` writes the immutable frontmatter
/// first, then APPENDS the node-claim fields (`target_claim_key/holder/ttl`)
/// after the closing `---`, so `parse_manifest` (frontmatter-bounded) never sees
/// them. Renewal reads them here instead. Surrounding quotes stripped.
pub(crate) fn scan_manifest_field(content: &str, field: &str) -> Option<String> {
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
pub(super) fn parse_manifest(content: &str) -> Option<Manifest> {
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
    pub(super) attended_wall_cap_minutes: Option<Result<u64, String>>,
    /// config.budget.attended.cost_cap_usd
    pub(super) attended_cost_cap_usd: Option<Result<f64, String>>,
    /// config.budget.unattended.wall_clock_cap_minutes
    pub(super) unattended_wall_cap_minutes: Option<Result<u64, String>>,
    /// config.budget.unattended.cost_cap_usd
    pub(super) unattended_cost_cap_usd: Option<Result<f64, String>>,
    /// flat budget_cap: (folds in ) - applies as cost cap for both modes
    pub(super) flat_budget_cap: Option<Result<f64, String>>,
    /// config.ci.declared_none: true
    pub(super) ci_declared_none: bool,
    /// config.external_reviewers list
    pub(super) external_reviewers: Vec<String>,
    /// config.review.github_apps.
    /// None = key absent -> code default (empty, no gate).
    /// Some([]) = explicitly `[]` -> declared no-review-gate path.
    /// Some(list) = every listed login must have a completed review pass.
    pub(super) github_apps: Option<Vec<String>>,
    /// config.review.required_bots: legacy alias for `github_apps` (a straight
    /// rename). `github_apps` wins when both are set. Same fail-closed rules.
    pub(super) required_bots: Option<Vec<String>>,
    /// config.review.peers: local review harnesses. Identity-free entries form
    /// one composite, head-pinned local-attestation gate; entries with a shared
    /// or per-entry identity retain the legacy GitHub-login gate.
    pub(super) peers: Vec<PeerEntry>,
    /// config.review.peer_identity: the shared login peers post under.
    pub(super) peer_identity: Option<String>,
    /// config.review.optional_apps: reviewer logins honored-if-present but NOT
    /// required. The gate never WAITS for them (their absence never blocks -
    /// this kills the App-bot usage-limit wedge), but a blocking finding from
    /// one still holds the gate until addressed ("honor if present"). None =
    /// no optional reviewers.
    pub(super) optional_apps: Option<Vec<String>>,
    /// config.review.reviewers: local reviewer names (sigma |
    /// code-review | declare) satisfied by a head-pinned `review_attestation`
    /// event in events.jsonl, NOT a GitHub login. Empty = no reviewers gate
    /// (additive to the login gate; no "declared empty" distinction needed). A
    /// leading '/' is stripped on store so `/code-review` == `code-review`.
    /// Resolvability is validated Python-side; Rust fails closed by matching
    /// evidence, so an unresolvable name is simply never satisfied.
    pub(super) reviewers: Vec<String>,
    /// config.review.self_review_required (default true): floor the
    /// harness-resolved self-review reviewer onto `reviewers` when a code
    /// payload would otherwise ship unreviewed on a stock install. None means
    /// absent, normalized to true (the obligation defaults ON); `false` is the
    /// documented escape hatch.
    pub(super) self_review_required: Option<bool>,
    /// config.review.posture: the named rung of the review ladder.
    /// None = unset, which resolves through the legacy inference (mirroring
    /// `fno.config.resolve_review_posture`) or the shipped self_review floor.
    /// A value the ladder does not carry stays None here: the Python loader
    /// rejects the same config, so nothing that loads can disagree.
    pub(super) posture: Option<String>,
    /// config.review.github_approval_satisfies (default true): a non-author
    /// human GitHub APPROVED review counts toward coverage on its own and
    /// satisfies coverage on its own. GitHub refuses an author's approval
    /// of their own PR server-side; the gate still asserts the property
    /// itself (an unreadable PR author fails closed to "exclude"). The limit:
    /// GitHub's refusal is per identity, not per human - a second account
    /// with its own token can still self-approve.
    pub(super) github_approval_satisfies: Option<bool>,
    /// config.review.max_rounds (default 2, clamped at least 1 at read): the
    /// review-round budget. Once this many rounds pass, the review obligation
    /// is discharged: the gate reads covered and the PR merges on green CI.
    /// A round is one
    /// reviewed HEAD, so two verdicts at one unchanged head are one round.
    /// CI failures, lint failures and rebases are not rounds, and a pass
    /// refunds nothing: it is one round like any other verdict.
    pub(super) max_rounds: Option<i64>,
    /// config.review.carry_interdiff_lines (law d-608344c1, default 100):
    /// how many interdiff lines (multiset symmetric difference of the two
    /// PR-code patches against base) a rebase or fix may add while a verdict
    /// from the older head still carries. `0` disables the arm. Parsed in the
    /// same block as `max_rounds`; resolved by `carry_interdiff_lines_resolved`.
    pub(super) carry_interdiff_lines: Option<i64>,
    /// config.review.nudge: per-login overrides for the bot-review
    /// nudge, resolved against BOT_PROFILES by `resolved_nudge_configs`. Empty =
    /// no overrides (the built-in profiles alone decide nudgeability). A
    /// malformed entry degrades that login to non-nudgeable, never panics (AC8).
    pub(super) nudge_overrides: Vec<NudgeOverride>,
    /// Top-level `done_probes`: the repo-wide probe list, evaluated
    /// alongside the plan's own. The file is FLAT, so this reads off the TOML
    /// root, not out of a `config` table. None = key absent (no project gate);
    /// Some(Err(why)) = present but not an array of strings, which BLOCKS - a
    /// config key that degrades to no-gate is a guardrail that disappears when
    /// you typo it. pub(crate): the config-parse tests moved with the probe
    /// substrate to acceptance_evidence.
    pub(crate) done_probes: Option<Result<Vec<String>, String>>,
}

/// Normalize a config.review.reviewers entry / an event's reviewer name: strip a
/// leading '/' so `/code-review` and `code-review` name the same reviewer
/// (parity with the Python validator). Quote/comment stripping is the caller's.
pub(super) fn normalize_reviewer(raw: &str) -> String {
    raw.trim().trim_start_matches('/').to_string()
}

/// Fail-closed sentinel for a structurally-malformed `reviewers:` value (e.g. a
/// `{...}` mapping). Python raises loudly on such a value; the Rust parser must
/// NOT silently drop it to an empty list (= no gate, fail OPEN). Instead it
/// stores this sentinel so the gate stays active but UNSATISFIABLE - the NUL
/// byte can never appear in an emitted `review_attestation.reviewer`, so no
/// evidence ever clears it (codex peer review P1).
pub(super) const MALFORMED_REVIEWERS_SENTINEL: &str = "\u{0}malformed-reviewers";

/// A `config.review.peers` entry. `provider` is kept for messaging and the
/// same-model guard; `model` carries an optional `"route_provider,route_model"`
/// route (the claude CLI as transport for a genuinely different model); the gate
/// identity selects the legacy posting carrier; otherwise the entry contributes
/// to the composite local-attestation gate.
#[derive(Debug, Default, Clone)]
pub(super) struct PeerEntry {
    pub(super) provider: String,
    pub(super) model: Option<String>,
    pub(super) identity: Option<String>,
}

/// Strip a trailing YAML inline comment (` # ...`) from a raw scalar value
/// (codex P2 on #448). YAML requires whitespace before the `#`; a value that
/// IS a comment strips to empty. Quoted values containing '#' are out of
/// scope for this minimal parser (no known bot login contains '#').
pub(super) fn strip_inline_comment(raw: &str) -> &str {
    if raw.starts_with('#') {
        return "";
    }
    match raw.find(" #").or_else(|| raw.find("\t#")) {
        Some(i) => raw[..i].trim_end(),
        None => raw,
    }
}

/// Fail-closed sentinel for an unparseable config.toml). A
/// scanner error (e.g. tab-indentation, which YAML forbids) previously caused
/// the hand-parser to silently drop the whole config.review subtree, yielding
/// zero required_bots and shipping the PR unreviewed. Now such a file fails
/// CLOSED: this sentinel is placed in the login gate so it can never be
/// satisfied (no real bot login contains a NUL), the gate blocks visibly, and a
/// `loop_check_settings_unparseable` event records it. Distinct from
/// MALFORMED_REVIEWERS_SENTINEL so an audit sees which gate the config tripped.
pub(super) const UNPARSEABLE_SETTINGS_SENTINEL: &str = "\u{0}unparseable-settings\u{0}";

/// A bare scalar RHS (`key: value`) as a single-item login list. Used when a
/// list key was written scalar-form: it must GATE on that one login, never
/// silently fail open to "no gate" (codex P1 on #205). A structurally-malformed
/// value (a `{...}` flow mapping) is NOT a login - degrade to None so both
/// parsers agree (Python's typed reader drops a mapping to None too; codex P1 on
/// the two-parser-agreement invariant). Empty -> None.
pub(super) fn scalar_as_singleton(rest: &str) -> Option<Vec<String>> {
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
pub(super) fn scalar_string(v: &toml::Value) -> Option<String> {
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
pub(super) fn value_as_login_list(v: &toml::Value) -> Option<Vec<String>> {
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

/// One `[review.nudge]` per-login override. Every field is optional in
/// TOML; a value of the wrong type sets `malformed` so that login degrades to
/// non-nudgeable rather than panicking - the stop gate must never panic (AC8).
#[derive(Debug, Clone, Default)]
pub(super) struct NudgeOverride {
    pub(super) login: String,
    pub(super) review_handle: Option<String>,
    pub(super) wait_minutes: Option<i64>,
    pub(super) ceiling: Option<usize>,
    /// Defaults to true; `enabled = false` opts a repo out (back to plain
    /// block-and-wait, NOT a faster give-up).
    pub(super) enabled: bool,
    /// Any field of the wrong type: the whole login drops to non-nudgeable.
    pub(super) malformed: bool,
}

/// Parse the `[review.nudge]` table (`login -> { review_handle, wait_minutes,
/// ceiling, enabled }`). Lenient by construction, matching `value_as_login_list`:
/// a non-table value, or any field of the wrong type / a non-positive integer,
/// marks that login `malformed`. Never panics (AC8).
pub(super) fn value_as_nudge_overrides(v: &toml::Value) -> Vec<NudgeOverride> {
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

/// Classify a config.review.reviewers value.
/// Unlike the login lists, a structurally-wrong mapping fails CLOSED (Python
/// raises) via the unsatisfiable sentinel, never a silent empty gate. A leading
/// '/' is normalized off each entry.
pub(super) fn value_as_reviewers(v: &toml::Value) -> Vec<String> {
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
pub(super) fn value_as_peers(v: &toml::Value) -> Vec<PeerEntry> {
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
pub(super) fn read_f64_cap(v: &toml::Value, ctx: &str) -> Option<Result<f64, String>> {
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
pub(super) fn read_u64_cap(v: &toml::Value, ctx: &str) -> Option<Result<u64, String>> {
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
pub(crate) fn value_as_probe_list(v: &toml::Value) -> Result<Vec<String>, String> {
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
/// when config.toml cannot be parsed as TOML at all). The
/// sentinel goes into BOTH github_apps and required_bots: resolved_required_bots
/// prefers github_apps.or(required_bots), so pinning required_bots alone would
/// be silently outranked by a parseable global file's github_apps during the
/// global+local merge (an unparseable LOCAL file would then resolve to the
/// global gate, re-opening the fail-open this fix removes).
pub(super) fn fail_closed_settings() -> Settings {
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
///). A genuine YAML scanner error (e.g. tab indentation) returns
/// Err so the caller can fail closed + emit an event, rather than silently
/// zeroing the gate. The typed-Value classification preserves every semantic
/// the old ListForm branches encoded (see the value_as_* helpers).
pub(super) fn parse_settings_result(content: &str) -> Result<Settings, String> {
    let root: toml::Value = content.parse::<toml::Value>().map_err(|e| e.to_string())?;
    let mut s = Settings::default();

    // Top-level flat budget cap.
    if let Some(v) = root.get("budget_cap") {
        s.flat_budget_cap = read_f64_cap(v, "budget_cap");
    }

    // Top-level flat `done_probes`. Presence is recorded even when the
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
            let explicit_empty = matches!(v, toml::Value::Array(items) if items.is_empty());
            if !explicit_empty || live_merge_gating_optout("review.optional_apps") {
                s.optional_apps = value_as_login_list(v);
            }
        }
        if let Some(v) = review.get("reviewers") {
            s.reviewers = value_as_reviewers(v);
        }
        if let Some(v) = review.get("posture") {
            // Only a name the ladder carries is kept; anything else (a typo,
            // a non-string) stays None so the legacy inference decides. The
            // Python loader rejects a bad value outright, so a config that
            // loads on one side can never parse differently on the other.
            if let Some(raw) = v.as_str() {
                let trimmed = raw.trim();
                if posture_components(trimmed).is_some() {
                    s.posture = Some(trimmed.to_string());
                }
            }
        }
        if let Some(v) = review.get("github_approval_satisfies") {
            // One coercion contract for every lax-bool review leaf:
            // for every config bool, so the two gates cannot disagree on a
            // config that loads at all. Malformed stays None -> true (the
            // default-ON direction), matching the Python loader's rejection.
            s.github_approval_satisfies = lax_bool(v);
        }
        if let Some(v) = review.get("max_rounds") {
            // An integer at least 1, read-side clamped; anything else stays
            // None -> the default 2, so a typo cannot zero the budget (a
            // missing cap discharges the review obligation on every second
            // round).
            let parsed = v
                .as_integer()
                .or_else(|| v.as_str().and_then(|raw| raw.trim().parse::<i64>().ok()));
            if parsed.is_some_and(|n| n >= 1) {
                s.max_rounds = parsed;
            }
        }
        if let Some(v) = review.get("carry_interdiff_lines") {
            // Same lax contract: an integer (>= 0, where 0 disables the arm)
            // or its string form; anything else stays None -> the law's
            // default 100, so a typo cannot silently widen or close the carry.
            let parsed = v
                .as_integer()
                .or_else(|| v.as_str().and_then(|raw| raw.trim().parse::<i64>().ok()));
            if parsed.is_some_and(|n| n >= 0) {
                s.carry_interdiff_lines = parsed;
            }
        }
        if let Some(v) = review.get("self_review_required") {
            // A malformed value stays None -> normalized to true (obligation on,
            // fail-closed); an explicit false is also obligation-on unless the
            // global claim that backs this opt-out is LIVE.
            let parsed = v.as_bool();
            if parsed != Some(false) || live_merge_gating_optout("review.self_review_required") {
                s.self_review_required = parsed;
            }
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

/// One lax-bool coercion for config booleans: a real bool, a 0/1 integer, or
/// pydantic's string spellings ("true"/"yes"/"on"/"y"/"t"/"1" and the false
/// mirror). Anything else is None, so the caller's default decides - the
/// Python config loader rejects the same shapes, so no config can load green
/// on one side and parse differently here.
pub(super) fn lax_bool(v: &toml::Value) -> Option<bool> {
    v.as_bool()
        .or_else(|| {
            v.as_integer().and_then(|i| match i {
                1 => Some(true),
                0 => Some(false),
                _ => None,
            })
        })
        .or_else(|| {
            v.as_str()
                .and_then(|raw| match raw.trim().to_ascii_lowercase().as_str() {
                    "true" | "yes" | "on" | "y" | "t" | "1" => Some(true),
                    "false" | "no" | "off" | "n" | "f" | "0" => Some(false),
                    _ => None,
                })
        })
}

pub(super) fn live_merge_gating_optout(key: &str) -> bool {
    let claim_key = format!("config-optout:{key}");
    matches!(
        crate::claims::status(&claim_key, None).0,
        crate::claims::ClaimState::Live
    )
}

/// Infallible wrapper: an unparseable file fails CLOSED (unsatisfiable login
/// gate) rather than silently defaulting to no gate. Test-only - production
/// calls parse_settings_result directly so it can also emit the
/// `loop_check_settings_unparseable` event on the Err path.
#[cfg(test)]
pub(crate) fn parse_settings(content: &str) -> Settings {
    parse_settings_result(content).unwrap_or_else(|_| fail_closed_settings())
}

// ── ledger parsing ────────────────────────────────────────────────────────────

/// Sum cost_usd for entries matching session_id. Tolerate missing/malformed as 0.
pub(super) fn session_cost_from_ledger(ledger_path: &Path, session_id: &str) -> f64 {
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
