use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;
use serde::Deserialize;

/// The capability contract. This in-crate TOML is the CANONICAL table
/// (x-244c, operator ruling 2026-09-02): `build.rs` copies it over the
/// Python-tree table (`cli/src/fno/agents/harness_capabilities.toml`) on
/// every build, so this is the only file a human edits. `crates/fno` reads
/// the table through the dep instead of carrying its own copy, and
/// `scripts/ci/check-harness-capabilities-fresh.sh` catches a hand-edited
/// generated copy committed without a rebuild.
pub const CAPABILITY_TOML: &str = include_str!("harness_capabilities.toml");

const SESSION_LANES: [&str; 5] = [
    "interactive_create",
    "interactive_resume",
    "interactive_attach",
    "headless_create",
    "headless_resume",
];
const RESPONSE_ACTIONS: [&str; 3] = ["allow_once", "allow_always", "deny"];
/// The rule states a capability row may DECLARE. A readiness manifest carries
/// more (`working`, and rules that hold the current state), but only these two
/// are part of the row's contract, and the Python validator enforces the same
/// pair.
const DECLARABLE_RULE_STATES: [&str; 2] = ["idle", "blocked"];
const RESUME_KINDS: [&str; 4] = ["flag", "subcommand", "session_flag", "unsupported"];
const MODEL_SWITCH_KINDS: [&str; 3] = ["direct", "menu_walk", "unsupported"];
const MODEL_SWITCH_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const STOP_STRATEGIES: [&str; 2] = ["claude-short-id", "registry-noop"];
/// Whether the fno target loop can CLOSE on a harness. `native` fires a shell
/// hook at the lifecycle boundary that invokes loop-check; `extension` reaches
/// loop-check only through a harness-native plugin fno ships (named by
/// `loop_extension`); `none` exposes no boundary at all. Kept identical to the
/// Python validator in cli/src/fno/agents/harness_map.py so the two runtimes
/// cannot disagree about which contracts are legal.
const LOOP_PARTICIPATION: [&str; 3] = ["native", "extension", "none"];
const REMOVE_STRATEGIES: [&str; 3] = ["claude-short-id", "codex-session-index", "registry-only"];
/// How a probe declaration says a field can be settled. `declared`: the
/// vendor states it about its own interface (help/version), and reading that
/// is not inference. `behavioral`: only a scratch-PTY run checking a
/// vendor-produced state marker settles it. `unprobeable`: no authority and
/// no marker, and the declaration must say why.
const PROBE_KINDS: [&str; 3] = ["declared", "behavioral", "unprobeable"];
/// The closed feature key set (x-a3e8). A feature is what a harness can DO -
/// a review command, an RPC surface, a plugin system - as opposed to the
/// keystroke mechanics above, which model how fno PUPPETS a pane. Closed so a
/// typo is a parse error rather than a silent new dimension.
const FEATURE_KEYS: [&str; 11] = [
    "review",
    "spawn",
    "attach",
    "rpc",
    "server",
    "plugins",
    "hooks",
    "skills_dir",
    "subagent_dispatch",
    "mcp",
    "acp",
];
/// The four states a feature claim may DECLARE. `native` means a wired lane
/// exists TODAY: the harness's own surface where it exposes one, otherwise a
/// lane fno hosts itself - opencode ships no attach verb and `fno agents
/// attach` still lands, through the daemon-kept portal. Exposure and
/// reachability are different answers, and conflating them is the defect
/// this dimension exists to delete. `capable` is the load-bearing third
/// state: real on the harness, and fno has NO wired arm for it. `thread =
/// false` could not say that, and `SPAWN_HARNESSES` could only encode it as
/// a tuple plus prose.
const FEATURE_STATES: [&str; 4] = ["native", "capable", "absent", "unmeasured"];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("harness capability contract: {0}")]
pub struct ContractError(String);

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessContract {
    pub map_version: u32,
    pub harness: BTreeMap<String, HarnessCapabilities>,
    /// HOW each named field can be settled (x-244c), keyed by field path.
    /// `declared` carries an authority command (`{bin}` = the harness's own
    /// binary) plus the pattern that settles it in the authority's output;
    /// `behavioral` carries the vendor-produced marker a scratch-PTY run
    /// must read; `unprobeable` carries the reason no one-shot probe exists.
    /// A field ABSENT from this table is UNDECLARED, and the probe reports
    /// it as such instead of guessing an instrument.
    #[serde(default)]
    pub probe: BTreeMap<String, ProbeDecl>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeDecl {
    /// One of [`PROBE_KINDS`].
    pub kind: String,
    #[serde(default)]
    pub authority: String,
    #[serde(default)]
    pub pattern: String,
    #[serde(default)]
    pub marker: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessCapabilities {
    pub permission_bypass: Vec<String>,
    pub resume: String,
    pub thread: bool,
    pub autonomous_pane: bool,
    pub route_on_pane: bool,
    /// One of [`LOOP_PARTICIPATION`]. Read at the dispatch seam, not at load.
    pub loop_participation: String,
    /// The plugin artifact an `extension` harness closes its loop through,
    /// repo-relative. EMPTY means fno ships none yet, which refuses a looping
    /// dispatch. `native` and `none` rows carry the empty string.
    #[serde(default)]
    pub loop_extension: String,
    pub command_surface: String,
    #[serde(default)]
    pub slash_prefix: String,
    /// This harness's own TUI slash verbs, measured. The verb normalizer
    /// rewrites a leading-slash token only when it names a shipped footnote
    /// verb absent from this list; absent means the harness declares none.
    #[serde(default)]
    pub native_verbs: Vec<String>,
    /// The subset the mail lane maps to a structured review RPC on a daemon
    /// thread; empty when the harness has no such transport.
    #[serde(default)]
    pub review_verbs: Vec<String>,
    pub ready_marker: String,
    pub ready_rule_ids: Vec<String>,
    pub manifest_rules: Vec<ManifestRuleRef>,
    pub send_keys_enter_delay_ms: i64,
    pub submit_keys: Vec<String>,
    pub stop_strategy: String,
    pub remove_strategy: String,
    pub session_binding: SessionBinding,
    /// How this harness stands toward the fno state root, keyed by substrate.
    /// The value is a carrier name, `unsandboxed` for a lane measured to need
    /// none, or `unmeasured`. An ABSENT key means the lane declares nothing,
    /// which the spawn gate refuses (epic rule R3). `default` keeps an older
    /// packaged copy parseable.
    #[serde(default)]
    pub state_root_grant: BTreeMap<String, String>,
    pub permission_response: BTreeMap<String, PermissionResponse>,
    pub resume_strategy: ResumeStrategy,
    pub model_switch_strategy: ModelSwitchStrategy,
    /// What this harness can DO, keyed by [`FEATURE_KEYS`] (x-a3e8) - a second
    /// dimension beside the pane mechanics, not a rewrite of them. A key
    /// ABSENT here reads `unmeasured`, which is legal and renders as a
    /// visible gap; the refusal lives in the lanes that consume a feature,
    /// not in the parse. Every key used on any row must carry a
    /// `[probe.features.<key>]` declaration, enforced in
    /// [`HarnessContract::validate`].
    #[serde(default)]
    pub features: BTreeMap<String, FeatureClaim>,
}

/// One feature claim. Its only field is `state`; the vocabulary and the
/// probe coupling are the contract.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureClaim {
    /// One of [`FEATURE_STATES`].
    pub state: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionResponse {
    pub supported: bool,
    pub rule_ids: Vec<String>,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestRuleRef {
    pub id: String,
    pub state: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionBinding {
    pub strategy: String,
    pub required: bool,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeStrategy {
    pub forms: BTreeMap<String, ResumeForm>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeForm {
    pub kind: String,
    pub tokens: Vec<String>,
    /// A command to run BEFORE `tokens`, for a harness whose attach needs a
    /// service up first. Declarative rather than fno-specific: codex names its
    /// own `codex app-server daemon start`, a documented no-op when the daemon
    /// is already running, so it is safe on every attach. Empty for a harness
    /// that needs nothing.
    ///
    /// The pair reads as action then assertion: `tokens` carries the assertion
    /// that the action took (codex's `--remote unix://` fails by name against
    /// a daemon that is not there, where a bare resume would silently run a
    /// private in-process app-server and hand back a session that looks
    /// correct).
    #[serde(default)]
    pub pre_exec: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSwitchStrategy {
    pub kind: String,
    pub tokens: Vec<String>,
    pub effort_labels: BTreeMap<String, String>,
    pub status_command: String,
    pub status_pattern: String,
}

impl HarnessCapabilities {
    /// How this harness stands toward the state root on `substrate`, or `None`
    /// when it declares nothing.
    ///
    /// Deliberately NOT named `carrier`: the value can be `unsandboxed` or
    /// `unmeasured`, and a caller that read either as a carrier name would try
    /// to spend a stance. `None` is the refusal case.
    pub fn state_root_stance(&self, substrate: &str) -> Option<&str> {
        self.state_root_grant.get(substrate).map(String::as_str)
    }
}

impl HarnessContract {
    pub fn packaged() -> Result<Self, ContractError> {
        Self::parse(CAPABILITY_TOML)
    }

    /// Gate ONE merged candidate row (bundled + config override) through the
    /// same per-row contract the bundled table ships under (x-244c). The row
    /// arrives as generic TOML so an override reader hands over its merged
    /// candidate without assembling a whole contract. `deny_unknown_fields`
    /// still applies: a key the row vocabulary does not carry is a
    /// rejection, not a silent keep.
    pub fn validate_merged_row(harness: &str, row: &toml::Value) -> Result<(), ContractError> {
        let caps = HarnessCapabilities::deserialize(row.clone())
            .map_err(|error| ContractError(error.to_string()))?;
        validate_row(harness, &caps)
    }

    /// [`HarnessCapabilities::state_root_stance`] for an unknown-harness name.
    /// An unknown harness declares nothing, so it too is refused.
    pub fn state_root_stance(&self, harness: &str, substrate: &str) -> Option<&str> {
        self.harness.get(harness)?.state_root_stance(substrate)
    }

    pub fn parse(text: &str) -> Result<Self, ContractError> {
        let contract: HarnessContract =
            toml::from_str(text).map_err(|error| ContractError(error.to_string()))?;
        contract.validate()?;
        Ok(contract)
    }

    fn validate(&self) -> Result<(), ContractError> {
        let actual: BTreeSet<&str> = self.harness.keys().map(String::as_str).collect();
        let expected: BTreeSet<&str> = crate::provider::KNOWN_PROVIDERS.iter().copied().collect();
        if actual != expected {
            return Err(ContractError(
                "harness set diverges from provider::KNOWN_PROVIDERS".into(),
            ));
        }
        for (harness, caps) in &self.harness {
            validate_row(harness, caps)?;
        }
        for (field, decl) in &self.probe {
            validate_probe_decl(field, decl)?;
        }
        // A feature declaration may only name a key in the closed set, and a
        // feature USED on any row may only exist beside a declaration: the
        // declaration is what makes the claim checkable, so a claim without
        // one is a guess the table refuses to carry (x-a3e8, AC2).
        for field in self.probe.keys() {
            if let Some(key) = field.strip_prefix("features.") {
                if !FEATURE_KEYS.contains(&key) {
                    return Err(ContractError(format!(
                        "probe {field:?}: {key:?} is not one of the feature keys"
                    )));
                }
            }
        }
        for (harness, caps) in &self.harness {
            for key in caps.features.keys() {
                let path = format!("features.{key}");
                if !self.probe.contains_key(&path) {
                    return Err(ContractError(format!(
                        "harness {harness:?} uses feature {key:?} with no [probe.{path}] declaration"
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn capabilities(&self, harness: &str) -> Result<&HarnessCapabilities, ContractError> {
        self.harness.get(harness).ok_or_else(|| {
            ContractError(format!(
                "unknown harness {harness:?} in capability contract"
            ))
        })
    }

    pub fn render_session_argv(
        &self,
        harness: &str,
        lane: &str,
        session_id: Option<&str>,
    ) -> Result<Vec<String>, ContractError> {
        self.render_session_argv_with_ids(harness, lane, session_id, None)
    }

    pub fn render_session_argv_with_ids(
        &self,
        harness: &str,
        lane: &str,
        session_id: Option<&str>,
        short_id: Option<&str>,
    ) -> Result<Vec<String>, ContractError> {
        let caps = self.capabilities(harness)?;
        let form =
            caps.resume_strategy.forms.get(lane).ok_or_else(|| {
                field_error(harness, "resume_strategy", &format!("no lane {lane:?}"))
            })?;
        if form.kind == "unsupported" {
            return Err(field_error(
                harness,
                "resume_strategy",
                &format!("lane {lane:?} is unsupported"),
            ));
        }
        if let Some(index) = form.tokens.iter().position(|token| token == "{short_id}") {
            if session_id.is_some_and(|id| !id.is_empty()) {
                return Err(field_error(
                    harness,
                    "resume_strategy",
                    &format!("lane {lane:?} needs a short_id, not a session_id"),
                ));
            }
            let Some(id) = short_id.filter(|id| !id.is_empty()) else {
                return Err(field_error(
                    harness,
                    "resume_strategy",
                    &format!("lane {lane:?} needs a non-empty short_id"),
                ));
            };
            let mut tokens = form.tokens.clone();
            tokens[index] = id.to_string();
            return Ok(with_pre_exec(form, tokens));
        }
        if short_id.is_some_and(|id| !id.is_empty()) {
            return Err(field_error(
                harness,
                "resume_strategy",
                &format!("lane {lane:?} accepts a session_id, not a short_id"),
            ));
        }
        let Some(index) = form.tokens.iter().position(|token| token == "{session_id}") else {
            return Ok(with_pre_exec(form, form.tokens.clone()));
        };
        if let Some(id) = session_id.filter(|id| !id.is_empty()) {
            let mut tokens = form.tokens.clone();
            tokens[index] = id.to_string();
            return Ok(with_pre_exec(form, tokens));
        }
        if lane.ends_with("create") {
            let start = index
                .checked_sub(1)
                .filter(|prior| form.tokens[*prior].starts_with('-'))
                .unwrap_or(index);
            let mut tokens = form.tokens.clone();
            tokens.drain(start..=index);
            return Ok(with_pre_exec(form, tokens));
        }
        Err(field_error(
            harness,
            "resume_strategy",
            &format!("lane {lane:?} needs a non-empty session id"),
        ))
    }

    /// Which thread lane this harness needs, from the capability contract
    /// alone - never from a name list, so a new row lands in its lane with
    /// no code edit here. Kept identical to `thread_lane` in
    /// `harness_map.py` so the two runtimes cannot disagree about lanes the
    /// way LOOP_PARTICIPATION is pinned across them.
    pub fn thread_lane(&self, harness: &str) -> Result<&'static str, ContractError> {
        let caps = self.capabilities(harness)?;
        let kind = |lane: &str| {
            caps.resume_strategy
                .forms
                .get(lane)
                .map(|form| form.kind.as_str())
                .unwrap_or("unsupported")
        };
        if kind("interactive_attach") != "unsupported" {
            return Ok("attach");
        }
        if kind("interactive_resume") != "unsupported" {
            return Ok("keeper");
        }
        Ok("none")
    }

    /// Whether this harness's attach lane needs a harness-owned server
    /// started outside the spawn. `pre_exec` IS that declaration: it names
    /// the process that keeps the session alive when no client holds it
    /// (codex starts its app-server; claude's detached client is itself the
    /// session, so its attach form declares none). Meaningful only on the
    /// attach lane; never a name list. An unknown harness errors rather
    /// than answering false: a false answer would route it to the wrong arm.
    pub fn attach_needs_server(&self, harness: &str) -> Result<bool, ContractError> {
        let caps = self.capabilities(harness)?;
        Ok(caps
            .resume_strategy
            .forms
            .get("interactive_attach")
            .map(|form| !form.pre_exec.is_empty())
            .unwrap_or(false))
    }

    pub fn permission_response_keys(
        &self,
        harness: &str,
        action: &str,
        rule_id: &str,
    ) -> Result<Vec<String>, ContractError> {
        let caps = self.capabilities(harness)?;
        let response = caps.permission_response.get(action).ok_or_else(|| {
            field_error(
                harness,
                "permission_response",
                &format!("unknown action {action:?}"),
            )
        })?;
        if !response.supported {
            return Err(field_error(
                harness,
                "permission_response",
                &format!("action {action:?} is unsupported"),
            ));
        }
        if !response
            .rule_ids
            .iter()
            .any(|candidate| candidate == rule_id)
        {
            return Err(field_error(
                harness,
                "permission_response",
                &format!("action {action:?} refuses rule {rule_id:?}"),
            ));
        }
        Ok(response.keys.clone())
    }
}

fn validate_model_switch_strategy(
    harness: &str,
    strategy: &ModelSwitchStrategy,
) -> Result<(), ContractError> {
    if !MODEL_SWITCH_KINDS.contains(&strategy.kind.as_str()) {
        return Err(field_error(
            harness,
            "model_switch_strategy",
            "unknown kind",
        ));
    }
    if strategy.tokens.iter().any(String::is_empty)
        || strategy.effort_labels.iter().any(|(effort, label)| {
            !MODEL_SWITCH_EFFORTS.contains(&effort.as_str()) || label.is_empty()
        })
    {
        return Err(field_error(
            harness,
            "model_switch_strategy",
            "malformed tokens or effort labels",
        ));
    }
    let placeholder_re = Regex::new(r"\{([^{}]+)\}").expect("static placeholder regex");
    let mut placeholders = Vec::new();
    for token in &strategy.tokens {
        placeholders.extend(
            placeholder_re
                .captures_iter(token)
                .map(|capture| capture[1].to_string()),
        );
        let remainder = placeholder_re.replace_all(token, "");
        if remainder.contains('{') || remainder.contains('}') {
            return Err(field_error(
                harness,
                "model_switch_strategy",
                "malformed placeholder",
            ));
        }
    }
    if placeholders
        .iter()
        .any(|value| !["model", "effort", "effort_label"].contains(&value.as_str()))
    {
        return Err(field_error(
            harness,
            "model_switch_strategy",
            "unknown placeholder",
        ));
    }
    if strategy.kind == "unsupported" {
        if !strategy.tokens.is_empty()
            || !strategy.effort_labels.is_empty()
            || !strategy.status_command.is_empty()
            || !strategy.status_pattern.is_empty()
        {
            return Err(field_error(
                harness,
                "model_switch_strategy",
                "unsupported strategy has executable data",
            ));
        }
        return Ok(());
    }
    if !strategy.status_command.starts_with('/') {
        return Err(field_error(
            harness,
            "model_switch_strategy",
            "missing status command",
        ));
    }
    let status_re = Regex::new(&strategy.status_pattern).map_err(|error| {
        field_error(
            harness,
            "model_switch_strategy",
            &format!("invalid status pattern: {error}"),
        )
    })?;
    let groups: BTreeSet<&str> = status_re.capture_names().flatten().collect();
    if !groups.contains("model") || !groups.contains("effort") {
        return Err(field_error(
            harness,
            "model_switch_strategy",
            "status pattern needs model and effort groups",
        ));
    }
    let model_count = placeholders
        .iter()
        .filter(|value| value.as_str() == "model")
        .count();
    let effort_count = placeholders
        .iter()
        .filter(|value| value.as_str() == "effort")
        .count();
    let effort_label_count = placeholders
        .iter()
        .filter(|value| value.as_str() == "effort_label")
        .count();
    if strategy.kind == "direct" {
        if model_count != 1
            || effort_count != 1
            || effort_label_count != 0
            || !strategy.effort_labels.is_empty()
        {
            return Err(field_error(
                harness,
                "model_switch_strategy",
                "direct needs model and effort placeholders",
            ));
        }
        return Ok(());
    }
    let model_index = placeholders.iter().position(|value| value == "model");
    let effort_index = placeholders
        .iter()
        .position(|value| value == "effort_label");
    let label_keys: BTreeSet<&str> = strategy.effort_labels.keys().map(String::as_str).collect();
    if model_count != 1
        || effort_count != 0
        || effort_label_count != 1
        || model_index >= effort_index
        || label_keys != MODEL_SWITCH_EFFORTS.into_iter().collect()
    {
        return Err(field_error(
            harness,
            "model_switch_strategy",
            "menu_walk needs ordered model and effort targets",
        ));
    }
    Ok(())
}

/// The per-harness half of [`HarnessContract::validate`], extracted so an
/// override reader can gate ONE merged candidate row through the same
/// contract the bundled table ships under (x-244c).
/// A probe declaration may carry only the instrument its kind names, and a
/// declared pattern must compile: the declaration IS an instrument spec, and
/// a spec that cannot run is a guess with extra steps.
fn validate_probe_decl(field: &str, decl: &ProbeDecl) -> Result<(), ContractError> {
    if !PROBE_KINDS.contains(&decl.kind.as_str()) {
        return Err(ContractError(format!(
            "probe {field:?}: unknown kind {:?}",
            decl.kind
        )));
    }
    let (need, forbid): (&[&str], &[&str]) = match decl.kind.as_str() {
        "declared" => (&["authority", "pattern"], &["marker", "reason"]),
        "behavioral" => (&["marker"], &["authority", "pattern", "reason"]),
        _ => (&["reason"], &["authority", "pattern", "marker"]),
    };
    let get = |name: &str| match name {
        "authority" => decl.authority.as_str(),
        "pattern" => decl.pattern.as_str(),
        "marker" => decl.marker.as_str(),
        _ => decl.reason.as_str(),
    };
    if let Some(missing) = need.iter().find(|name| get(name).is_empty()) {
        return Err(ContractError(format!(
            "probe {field:?}: kind {:?} needs {missing}",
            decl.kind
        )));
    }
    if let Some(extra) = forbid.iter().find(|name| !get(name).is_empty()) {
        return Err(ContractError(format!(
            "probe {field:?}: kind {:?} must not carry {extra}",
            decl.kind
        )));
    }
    if decl.kind == "declared" {
        if let Err(error) = Regex::new(&decl.pattern) {
            return Err(ContractError(format!(
                "probe {field:?}: invalid pattern: {error}"
            )));
        }
    }
    Ok(())
}

fn validate_row(harness: &str, caps: &HarnessCapabilities) -> Result<(), ContractError> {
    for (key, claim) in &caps.features {
        if !FEATURE_KEYS.contains(&key.as_str()) {
            return Err(field_error(
                harness,
                "features",
                &format!("unknown feature key {key:?}"),
            ));
        }
        if !FEATURE_STATES.contains(&claim.state.as_str()) {
            return Err(field_error(
                harness,
                &format!("features.{key}"),
                &format!("unknown state {:?}", claim.state),
            ));
        }
    }
    let allowed_keys: BTreeSet<&str> = [
        "1",
        "2",
        "3",
        "4",
        "5",
        "6",
        "7",
        "8",
        "9",
        "enter",
        "left",
        "right",
        "up",
        "down",
        "tab",
        "esc",
        "y",
        "a",
        "d",
        "unsupported",
    ]
    .into_iter()
    .collect();
    let manifest = crate::manifest::load_manifest(harness, None)
        .ok_or_else(|| field_error(harness, "ready_marker", "no bundled manifest"))?
        .map_err(|error| field_error(harness, "ready_marker", &error.to_string()))?;
    for declared in &caps.manifest_rules {
        // The declared state vocabulary is idle/blocked ONLY, matching
        // the Python validator in `fno.agents.harness_map`. A manifest
        // may carry other states (`working`, and the engine's
        // skip_state_update), but a capability ROW declaring one would
        // parse here and be refused there, so a harness would load in
        // one language and brick the other.
        if !DECLARABLE_RULE_STATES.contains(&declared.state.as_str()) {
            return Err(field_error(
                harness,
                "manifest_rules",
                &format!(
                    "state {:?} is not declarable here; use idle or blocked",
                    declared.state
                ),
            ));
        }
        if !manifest
            .rules()
            .iter()
            .any(|rule| rule.id == declared.id && rule.state == declared.state)
        {
            return Err(field_error(
                harness,
                "manifest_rules",
                &format!(
                    "unknown manifest rule {:?}/{:?}",
                    declared.id, declared.state
                ),
            ));
        }
    }
    let actions: BTreeSet<&str> = caps
        .permission_response
        .keys()
        .map(String::as_str)
        .collect();
    if actions != RESPONSE_ACTIONS.into_iter().collect() {
        return Err(field_error(
            harness,
            "permission_response",
            "needs all three actions",
        ));
    }
    for (action, response) in &caps.permission_response {
        if response
            .keys
            .iter()
            .any(|key| !allowed_keys.contains(key.as_str()))
        {
            return Err(field_error(
                harness,
                "permission_response",
                &format!("bad keys for {action}"),
            ));
        }
        if response.supported
            && (response.keys.is_empty() || response.rule_ids.iter().any(String::is_empty))
        {
            return Err(field_error(
                harness,
                "permission_response",
                &format!("empty supported {action}"),
            ));
        }
        for rule_id in &response.rule_ids {
            if !manifest
                .rules()
                .iter()
                .any(|rule| rule.id == *rule_id && rule.state == "blocked")
            {
                return Err(field_error(
                    harness,
                    "permission_response",
                    &format!("{action} names unknown blocked rule {rule_id:?}"),
                ));
            }
        }
    }
    if caps.ready_marker != "unsupported" && !caps.ready_rule_ids.contains(&caps.ready_marker) {
        return Err(field_error(
            harness,
            "ready_marker",
            &format!("unknown rule {:?}", caps.ready_marker),
        ));
    }
    if caps.ready_marker != "unsupported"
        && !manifest
            .rules()
            .iter()
            .any(|rule| rule.id == caps.ready_marker && rule.state == "idle")
    {
        return Err(field_error(
            harness,
            "ready_marker",
            &format!("unknown positive manifest rule {:?}", caps.ready_marker),
        ));
    }
    if caps.send_keys_enter_delay_ms < 0 {
        return Err(field_error(
            harness,
            "send_keys_enter_delay_ms",
            "must be non-negative",
        ));
    }
    if caps.submit_keys.is_empty()
        || caps
            .submit_keys
            .iter()
            .any(|key| !allowed_keys.contains(key.as_str()))
    {
        return Err(field_error(harness, "submit_keys", "invalid key token"));
    }
    // A lane that never submits must not carry a delay: the number
    // would describe a wait nothing performs. The CONVERSE does not
    // hold and was asserted here until codex disproved it. A supported
    // contract may legitimately need no wait - measured against codex
    // 0.148.0, a carriage return sent immediately after the text
    // submits correctly, while claude needs 800ms. Kept identical to
    // the Python validator in cli/src/fno/agents/harness_map.py so the
    // two runtimes cannot disagree about which contracts are legal.
    if caps.submit_keys == ["unsupported"] && caps.send_keys_enter_delay_ms != 0 {
        return Err(field_error(
            harness,
            "send_keys_enter_delay_ms",
            "an unsupported submit contract cannot carry a nonzero delay",
        ));
    }
    let lanes: BTreeSet<&str> = caps
        .resume_strategy
        .forms
        .keys()
        .map(String::as_str)
        .collect();
    if lanes != SESSION_LANES.into_iter().collect() {
        return Err(field_error(
            harness,
            "resume_strategy",
            "needs every session lane",
        ));
    }
    for (lane, form) in &caps.resume_strategy.forms {
        if !RESUME_KINDS.contains(&form.kind.as_str())
            || form.tokens.iter().any(String::is_empty)
            || (form.kind == "unsupported" && !form.tokens.is_empty())
            || (lane.ends_with("resume")
                && form.kind != "unsupported"
                && !form.tokens.iter().any(|token| token == "{session_id}"))
        {
            return Err(field_error(
                harness,
                "resume_strategy",
                &format!("malformed {lane}"),
            ));
        }
        // An attach form must name the id its harness's own attach
        // command takes: claude's short jobId, or a full session id
        // where a short one would collide (a codex UUIDv7 head-8 is a
        // ~65.5s clock bucket). A form naming NEITHER cannot address a
        // session, so it is not one.
        if lane == "interactive_attach"
            && form.kind != "unsupported"
            && !form
                .tokens
                .iter()
                .any(|token| token == "{short_id}" || token == "{session_id}")
        {
            return Err(field_error(
                harness,
                "resume_strategy",
                &format!("{lane} drops its attach id"),
            ));
        }
    }
    validate_model_switch_strategy(harness, &caps.model_switch_strategy)?;
    if !LOOP_PARTICIPATION.contains(&caps.loop_participation.as_str()) {
        return Err(field_error(harness, "loop_participation", "unknown member"));
    }
    // Only an `extension` row may name an artifact: a `native` row
    // closes its loop through a shell hook and a `none` row closes it
    // through nothing, so a path on either is a claim the member
    // contradicts. The converse is legal and load-bearing - an
    // `extension` row with an EMPTY path is a harness whose extension
    // fno has not written yet, and the dispatch seam refuses it.
    if caps.loop_participation != "extension" && !caps.loop_extension.is_empty() {
        return Err(field_error(
            harness,
            "loop_extension",
            "only an extension harness may name a loop artifact",
        ));
    }
    if !STOP_STRATEGIES.contains(&caps.stop_strategy.as_str()) {
        return Err(field_error(harness, "stop_strategy", "unknown strategy"));
    }
    if !REMOVE_STRATEGIES.contains(&caps.remove_strategy.as_str()) {
        return Err(field_error(harness, "remove_strategy", "unknown strategy"));
    }
    if ![
        "preassigned-or-session-start",
        "rollout-fd-or-daemon",
        "preassigned",
        // The caller mints the id AND the harness scopes its lookup by
        // cwd, so the identity is the PAIR and the id alone addresses
        // nothing. Distinct from "preassigned", where the id is the
        // whole handle: a resume issued from the wrong directory finds
        // no session here and, on pi, CREATES a second one under the
        // same id rather than failing.
        "caller-assigned-cwd-scoped",
        "callee-minted-read-back",
        "store-lookup",
        "unsupported",
    ]
    .contains(&caps.session_binding.strategy.as_str())
        || (caps.session_binding.required && caps.session_binding.timeout_ms == 0)
    {
        return Err(field_error(harness, "session_binding", "invalid strategy"));
    }
    Ok(())
}

fn field_error(harness: &str, field: &str, detail: &str) -> ContractError {
    ContractError(format!("harness {harness:?} field {field:?}: {detail}"))
}

/// Compose a form's `pre_exec` with its rendered argv:
/// `sh -c '<pre_exec>; exec <argv>'`.
///
/// The shape is load-bearing three ways. `exec` replaces the shell, so the
/// pane's child is the harness itself and no fno process sits between the
/// terminal and it - the exec-versus-proxy property this lane is measured on.
/// `;` rather than `&&` lets a failed pre-exec still run the attach, which
/// produces the more specific of the two errors. And both errors surface in
/// the pane the operator is already looking at.
///
/// MIRRORED in `fno::agents_view::AttachForm::render` (the mux viewport's
/// door). `fno` cannot link this crate, so the two are pinned by
/// `attach_argv_matches_the_mux_renderer` rather than shared.
fn with_pre_exec(form: &ResumeForm, argv: Vec<String>) -> Vec<String> {
    if form.pre_exec.is_empty() {
        return argv;
    }
    let script = format!("{}; exec {}", sh_join(&form.pre_exec), sh_join(&argv));
    vec!["sh".to_string(), "-c".to_string(), script]
}

fn sh_join(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|token| format!("'{}'", token.replace('\'', r"'\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn render_session_argv(
    harness: &str,
    lane: &str,
    session_id: Option<&str>,
) -> Result<Vec<String>, ContractError> {
    HarnessContract::packaged()?.render_session_argv(harness, lane, session_id)
}

pub fn render_session_argv_with_ids(
    harness: &str,
    lane: &str,
    session_id: Option<&str>,
    short_id: Option<&str>,
) -> Result<Vec<String>, ContractError> {
    HarnessContract::packaged()?.render_session_argv_with_ids(harness, lane, session_id, short_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_contract_is_complete_for_every_harness() {
        let contract = HarnessContract::packaged().unwrap();
        assert_eq!(
            contract.harness.keys().cloned().collect::<Vec<_>>(),
            [
                "agy",
                "claude",
                "codex",
                "cursor-agent",
                "gemini",
                "grok",
                "opencode",
                "pi"
            ]
        );
        for (name, caps) in &contract.harness {
            assert_eq!(caps.permission_response.len(), 3, "{name}");
            assert_eq!(caps.resume_strategy.forms.len(), 5, "{name}");
            assert!(!caps.model_switch_strategy.kind.is_empty(), "{name}");
            assert!(!caps.submit_keys.is_empty(), "{name}");
            assert!(
                LOOP_PARTICIPATION.contains(&caps.loop_participation.as_str()),
                "{name}"
            );
            assert!(!caps.stop_strategy.is_empty(), "{name}");
            assert!(!caps.remove_strategy.is_empty(), "{name}");
        }
        assert_eq!(
            contract
                .capabilities("claude")
                .unwrap()
                .model_switch_strategy
                .kind,
            "direct"
        );
        assert_eq!(
            contract
                .capabilities("codex")
                .unwrap()
                .model_switch_strategy
                .kind,
            "menu_walk"
        );
    }

    /// The lane assignment, pinned per harness: lane A where the attach form
    /// declares a kind, lane B where only the resume form does. The resolver
    /// itself reads the contract; this table is the measured answer it must
    /// keep producing.
    #[test]
    fn thread_lane_is_derived_from_the_attach_declaration() {
        let contract = HarnessContract::packaged().unwrap();
        let lane = |h: &str| contract.thread_lane(h).unwrap();
        assert_eq!(lane("claude"), "attach");
        assert_eq!(lane("codex"), "attach");
        assert_eq!(lane("agy"), "keeper");
        assert_eq!(lane("gemini"), "keeper");
        assert_eq!(lane("opencode"), "keeper");
        assert_eq!(lane("pi"), "keeper");
    }

    /// The attach-split answer, pinned per attach-lane harness: the attach
    /// form's `pre_exec` is the encoding of "a process outside this spawn
    /// owns the session's life", and this is the measured split.
    #[test]
    fn attach_needs_server_is_derived_from_the_pre_exec_declaration() {
        let contract = HarnessContract::packaged().unwrap();
        assert!(
            contract.attach_needs_server("codex").unwrap(),
            "codex declares an app-server pre_exec"
        );
        assert!(
            !contract.attach_needs_server("claude").unwrap(),
            "claude's detached client is the session; no pre_exec declared"
        );
        assert!(
            contract.attach_needs_server("no-such-harness").is_err(),
            "an unknown harness must error, never answer false"
        );
    }

    /// The measured answer, pinned per harness. `stop_hook` read "native" on
    /// every row for a year because nothing here would have noticed if it
    /// stopped being true. A uniform table passes this test only if every row
    /// is uniform on purpose.
    #[test]
    fn loop_participation_is_measured_per_harness_and_not_uniform() {
        let contract = HarnessContract::packaged().unwrap();
        let value = |h: &str| contract.capabilities(h).unwrap().loop_participation.clone();
        assert_eq!(value("claude"), "native");
        assert_eq!(value("codex"), "native");
        assert_eq!(value("agy"), "native");
        assert_eq!(value("gemini"), "none");
        assert_eq!(value("opencode"), "extension");
        assert_eq!(value("pi"), "extension");
        let distinct: BTreeSet<String> = contract
            .harness
            .values()
            .map(|caps| caps.loop_participation.clone())
            .collect();
        assert!(
            distinct.len() > 1,
            "one value across every row is an inherited declaration, not a measurement"
        );
        // Both shipped extensions name an artifact; a native harness names
        // none, which is what keeps the extension lanes honest about what
        // actually closes a loop there.
        assert!(!contract
            .capabilities("opencode")
            .unwrap()
            .loop_extension
            .is_empty());
        assert!(!contract
            .capabilities("pi")
            .unwrap()
            .loop_extension
            .is_empty());
        assert!(contract
            .capabilities("claude")
            .unwrap()
            .loop_extension
            .is_empty());
    }

    #[test]
    fn an_out_of_enum_loop_participation_is_refused() {
        let text = CAPABILITY_TOML.replacen(
            "loop_participation = \"native\"",
            "loop_participation = \"sometimes\"",
            1,
        );
        let err = HarnessContract::parse(&text).unwrap_err().to_string();
        assert!(err.contains("loop_participation"), "{err}");
        assert!(err.contains("claude"), "{err}");
    }

    #[test]
    fn a_non_extension_harness_may_not_name_a_loop_artifact() {
        let text = CAPABILITY_TOML.replacen(
            "loop_participation = \"native\"\nloop_extension = \"\"",
            "loop_participation = \"native\"\nloop_extension = \"some/plugin.js\"",
            1,
        );
        let err = HarnessContract::parse(&text).unwrap_err().to_string();
        assert!(err.contains("loop_extension"), "{err}");
    }

    #[test]
    fn renders_identity_skeletons_from_the_shared_strategy() {
        let contract = HarnessContract::packaged().unwrap();
        assert_eq!(
            contract
                .render_session_argv("claude", "interactive_create", Some("c-1"))
                .unwrap(),
            ["claude", "--session-id", "c-1"]
        );
        assert_eq!(
            contract
                .render_session_argv_with_ids(
                    "claude",
                    "interactive_attach",
                    None,
                    Some("deadbeef")
                )
                .unwrap(),
            ["claude", "attach", "deadbeef"]
        );
        assert!(contract
            .render_session_argv_with_ids(
                "claude",
                "interactive_attach",
                Some("00000000-1111-2222-3333-444444444444"),
                None,
            )
            .unwrap_err()
            .to_string()
            .contains("short_id"));
        assert_eq!(
            contract
                .render_session_argv("codex", "interactive_resume", Some("cx-1"))
                .unwrap(),
            ["codex", "resume", "cx-1"]
        );
        assert_eq!(
            contract
                .render_session_argv("codex", "headless_resume", Some("cx-1"))
                .unwrap(),
            ["codex", "exec", "resume", "cx-1"]
        );
        assert_eq!(
            contract
                .render_session_argv("opencode", "headless_resume", Some("ses_1"))
                .unwrap(),
            ["opencode", "run", "--session", "ses_1"]
        );
        // agy's resume primitive, measured 2026-08-26 on 1.1.19 and 1.1.21:
        // `agy --conversation <id>` from a fresh process quotes a token planted
        // in an earlier turn (num_turns 2). The interactive row renders the
        // same argv the headless_resume row records.
        assert_eq!(
            contract
                .render_session_argv("agy", "interactive_resume", Some("cv-1"))
                .unwrap(),
            ["agy", "--conversation", "cv-1"]
        );
    }

    #[test]
    fn contract_exposes_permission_input_and_teardown_differences() {
        let contract = HarnessContract::packaged().unwrap();
        let claude = contract.capabilities("claude").unwrap();
        let codex = contract.capabilities("codex").unwrap();
        let opencode = contract.capabilities("opencode").unwrap();
        // The thread bit asserts fno's OWN driver for the harness (driver +
        // unattended journey test), never the harness's resume primitive.
        // agy and opencode both have measured-working primitives yet read
        // false: agy has no driver, opencode's serve lane is launch-only
        // (ask refuses, steering unbuilt). claude and codex are the
        // journey-proven lanes, and they must stay true so the false bits are
        // not vacuous.
        assert!(claude.thread);
        assert!(codex.thread);
        assert!(!contract.capabilities("agy").unwrap().thread);
        assert!(!opencode.thread);
        assert_eq!(claude.ready_marker, "live_prompt_box");
        assert_eq!(codex.ready_marker, "idle_prompt");
        assert_eq!(claude.send_keys_enter_delay_ms, 800);
        // codex submits on a carriage return like claude. Measured floor is 0
        // (short payload, codex 0.148.0; and x-4b0b 2026-08-23: 0.7-2.0 KB
        // envelopes, idle and mid-turn, every D in 0..800ms submitted). The
        // table reads 800 as a margin: the operator's queued-envelope pile-up
        // with the table at 0 never reproduced, and 800 matches claude's row
        // and CR_SETTLE_MS, the two shipped settles never observed to lose
        // a CR.
        assert_eq!(codex.submit_keys, ["enter"]);
        assert_eq!(codex.send_keys_enter_delay_ms, 800);
        assert_eq!(
            opencode.permission_response["deny"].keys,
            ["right", "right", "enter"]
        );
        assert_eq!(claude.stop_strategy, "claude-short-id");
        assert_eq!(codex.stop_strategy, "registry-noop");
        assert_eq!(codex.remove_strategy, "codex-session-index");
        assert_eq!(codex.session_binding.strategy, "rollout-fd-or-daemon");
        assert!(codex.session_binding.required);
        assert_eq!(codex.session_binding.timeout_ms, 60_000);
    }

    #[test]
    fn permission_response_requires_the_matching_manifest_rule() {
        let contract = HarnessContract::packaged().unwrap();
        assert_eq!(
            contract
                .permission_response_keys("claude", "allow_once", "permission_prompt")
                .unwrap(),
            ["1"]
        );
        assert!(contract
            .permission_response_keys("claude", "allow_once", "trust_prompt")
            .unwrap_err()
            .to_string()
            .contains("trust_prompt"));
        assert!(contract
            .permission_response_keys("opencode", "deny", "permission_required")
            .unwrap_err()
            .to_string()
            .contains("unsupported"));
    }

    #[test]
    fn malformed_fields_fail_with_harness_and_field() {
        for (needle, replacement, field) in [
            (
                "ready_marker = \"idle_prompt\"",
                "ready_marker = \"missing_rule\"",
                "ready_marker",
            ),
            (
                "keys = [\"1\"]",
                "keys = [\"bogus-key\"]",
                "permission_response",
            ),
            (
                "send_keys_enter_delay_ms = 800",
                "send_keys_enter_delay_ms = -1",
                "send_keys_enter_delay_ms",
            ),
            // An unsupported submit contract carrying a delay: a number
            // describing a wait nothing performs. gemini is the first
            // `unsupported` block in the file, so replacen hits it.
            (
                "send_keys_enter_delay_ms = 0\nsubmit_keys = [\"unsupported\"]",
                "send_keys_enter_delay_ms = 42\nsubmit_keys = [\"unsupported\"]",
                "send_keys_enter_delay_ms",
            ),
            (
                "kind = \"subcommand\"",
                "kind = \"mystery\"",
                "resume_strategy",
            ),
            (
                "tokens = [\"/model {model}\", \"/effort {effort}\"]",
                "tokens = [\"/model {model}\"]",
                "model_switch_strategy",
            ),
            (
                "tokens = [\"/model\", \"{model}\", \"{effort_label}\"]",
                "tokens = [\"/model\", \"{effort_label}\", \"{model}\"]",
                "model_switch_strategy",
            ),
            (
                "tokens = [\"/model {model}\", \"/effort {effort}\"]",
                "tokens = [\"/model {bogus}\", \"/effort {effort}\"]",
                "model_switch_strategy",
            ),
            (
                // Header-anchored: bare `kind = "unsupported"\ntokens = []`
                // also occurs in codex's resume_strategy forms (interactive_attach),
                // and a first-occurrence replacen would error naming
                // resume_strategy instead of the field under test.
                "[harness.gemini.model_switch_strategy]\nkind = \"unsupported\"\ntokens = []",
                "[harness.gemini.model_switch_strategy]\nkind = \"unsupported\"\ntokens = [\"/model {model}\"]",
                "model_switch_strategy",
            ),
            (
                "status_command = \"/status\"",
                "status_command = \"status\"",
                "model_switch_strategy",
            ),
        ] {
            let bad = CAPABILITY_TOML.replacen(needle, replacement, 1);
            let err = HarnessContract::parse(&bad).unwrap_err().to_string();
            assert!(err.contains(field), "{field}: {err}");
        }
    }

    #[test]
    fn ready_marker_must_exist_in_the_real_bundled_manifest() {
        let bad = CAPABILITY_TOML
            .replacen(
                "ready_marker = \"idle_prompt\"",
                "ready_marker = \"invented_ready\"",
                1,
            )
            .replacen(
                "ready_rule_ids = [\"idle_prompt\"]",
                "ready_rule_ids = [\"invented_ready\"]",
                1,
            );
        let error = HarnessContract::parse(&bad).unwrap_err().to_string();
        assert!(error.contains("ready_marker"), "{error}");
        assert!(error.contains("invented_ready"), "{error}");
    }

    fn form_with_pre_exec() -> (ResumeForm, Vec<String>) {
        (
            ResumeForm {
                kind: "subcommand".into(),
                tokens: vec![
                    "codex".into(),
                    "resume".into(),
                    "{session_id}".into(),
                    "--remote".into(),
                    "unix://".into(),
                ],
                pre_exec: vec![
                    "codex".into(),
                    "app-server".into(),
                    "daemon".into(),
                    "start".into(),
                ],
            },
            vec![
                "codex".into(),
                "resume".into(),
                "SESS".into(),
                "--remote".into(),
                "unix://".into(),
            ],
        )
    }

    #[test]
    fn pre_exec_composes_sh_exec_with_the_semicolon_intact() {
        let (form, rendered) = form_with_pre_exec();
        let argv = with_pre_exec(&form, rendered);
        assert_eq!(argv.first().map(String::as_str), Some("sh"));
        assert_eq!(argv[1], "-c");
        let script = &argv[2];
        // The literal "; exec " is load-bearing twice over: the exec makes the
        // pane's child the harness itself (exec-versus-proxy), and the `;`
        // rather than `&&` lets a failed pre-exec still produce the attach's
        // own more specific error. Assert the literal, not just the first token.
        assert!(script.contains("; exec "), "{script}");
        assert!(
            script.starts_with("'codex' 'app-server' 'daemon' 'start'; exec "),
            "{script}"
        );
        assert!(
            script.ends_with("'codex' 'resume' 'SESS' '--remote' 'unix://'"),
            "{script}"
        );
    }

    #[test]
    fn pre_exec_single_quotes_each_token_so_one_cannot_escape() {
        let form = ResumeForm {
            kind: "subcommand".into(),
            tokens: vec!["at'tach".into(), "{session_id}".into()],
            pre_exec: vec!["pre'exec".into()],
        };
        let argv = with_pre_exec(&form, vec!["at'tach".into(), "S".into()]);
        let script = &argv[2];
        assert_eq!(script, r"'pre'\''exec'; exec 'at'\''tach' 'S'", "{script}");
    }

    #[test]
    fn a_form_without_pre_exec_renders_bare_with_no_shell_in_the_way() {
        let form = ResumeForm {
            kind: "subcommand".into(),
            tokens: vec!["claude".into(), "attach".into(), "{short_id}".into()],
            pre_exec: Vec::new(),
        };
        let argv = with_pre_exec(
            &form,
            vec!["claude".into(), "attach".into(), "deadbeef".into()],
        );
        assert_eq!(argv, ["claude", "attach", "deadbeef"]);
    }

    #[test]
    fn an_attach_form_may_name_either_id_spelling_but_not_neither() {
        let claude_attach = "tokens = [\"claude\", \"attach\", \"{short_id}\"]";
        // Either spelling is a legal attach id: a full session id where a
        // short one would collide.
        let session = CAPABILITY_TOML.replacen(
            claude_attach,
            "tokens = [\"claude\", \"attach\", \"{session_id}\"]",
            1,
        );
        HarnessContract::parse(&session)
            .unwrap_or_else(|e| panic!("a session_id attach form is legal: {e}"));
        // A form naming NEITHER cannot address a session, so it is not one,
        // and the refusal says what is missing.
        let idless = CAPABILITY_TOML.replacen(
            claude_attach,
            "tokens = [\"claude\", \"attach\", \"--last\"]",
            1,
        );
        let err = HarnessContract::parse(&idless).unwrap_err().to_string();
        assert!(err.contains("interactive_attach"), "{err}");
        assert!(err.contains("drops its attach id"), "{err}");
    }

    /// AC3 (x-296f): codex's declared attach form renders action-then-assertion
    /// (`daemon start`, then the TUI), execs so the pane's child is the
    /// harness, and substitutes the FULL session id - a codex UUIDv7 head-8 is
    /// a ~65.5s clock bucket and would attach the wrong sibling. Measured
    /// 2026-08-29 against 0.149.1: session absent from `thread/loaded/list`
    /// before the resume, present after.
    #[test]
    fn codex_declared_attach_renders_daemon_start_then_exec_with_full_session_id() {
        let contract = HarnessContract::packaged().unwrap();
        let session = "01a04ea5-a473-7872-8137-49ff3cc214e9";
        let argv = contract
            .render_session_argv_with_ids("codex", "interactive_attach", Some(session), None)
            .unwrap();
        assert_eq!(argv.first().map(String::as_str), Some("sh"));
        let script = &argv[2];
        // The literal "; exec " is the exec-versus-proxy property; assert the
        // literal, not just the first token.
        assert!(script.contains("; exec "), "{script}");
        assert!(
            script.contains("'codex' 'app-server' 'daemon' 'start'"),
            "{script}"
        );
        assert!(script.contains("'--remote' 'unix://'"), "{script}");
        assert!(
            script.contains(session),
            "the FULL session id is substituted: {script}"
        );
        assert!(
            !script.contains('{'),
            "no placeholder left unrendered: {script}"
        );
        // The short spelling is refused by name: a head-8 cannot address a
        // codex session.
        let err = contract
            .render_session_argv_with_ids("codex", "interactive_attach", None, Some(&session[..8]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("session_id, not a short_id"), "{err}");
    }

    /// The features dimension beside the keystrokes (x-a3e8). The measured
    /// cells are pinned: the spawn set must equal the roster the spawn
    /// refusal used to hardcode, and the pane-only rows must read absent,
    /// because a state that silently drifted would put a refusal back in a
    /// lane that works.
    #[test]
    fn feature_claims_read_their_measured_states() {
        let contract = HarnessContract::packaged().unwrap();
        let state = |h: &str, k: &str| {
            contract
                .capabilities(h)
                .unwrap()
                .features
                .get(k)
                .map(|claim| claim.state.as_str())
                .unwrap_or("unmeasured")
        };
        for harness in ["claude", "codex", "opencode", "pi", "cursor-agent", "grok"] {
            assert_eq!(state(harness, "spawn"), "native", "{harness}");
        }
        assert_eq!(state("agy", "spawn"), "absent");
        assert_eq!(state("gemini", "spawn"), "absent");
        assert_eq!(state("claude", "review"), "native");
        assert_eq!(state("codex", "review"), "native");
        assert_eq!(state("claude", "mcp"), "unmeasured");
        assert_eq!(state("agy", "acp"), "unmeasured");
    }

    #[test]
    fn an_out_of_set_feature_state_is_refused_naming_harness_key_and_value() {
        let bad = CAPABILITY_TOML.replacen(
            "[harness.claude.features.spawn]\nstate = \"native\"",
            "[harness.claude.features.spawn]\nstate = \"wired\"",
            1,
        );
        let err = HarnessContract::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("claude"), "{err}");
        assert!(err.contains("features.spawn"), "{err}");
        assert!(err.contains("wired"), "{err}");
    }

    #[test]
    fn the_feature_key_set_is_closed() {
        let bad = CAPABILITY_TOML.replacen(
            "[harness.claude.features.spawn]",
            "[harness.claude.features.spwan]",
            1,
        );
        let err = HarnessContract::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("claude"), "{err}");
        assert!(err.contains("spwan"), "{err}");
    }

    #[test]
    fn a_used_feature_needs_its_probe_declaration() {
        // Rename the declaration out of the features namespace (a legal
        // field declaration, so the only remaining defect is the missing
        // coupling) and the use below it refuses by name.
        let bad = CAPABILITY_TOML.replacen(
            "[probe.\"features.spawn\"]",
            "[probe.\"spawn-decl-removed\"]",
            1,
        );
        let err = HarnessContract::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("features.spawn"), "{err}");
        assert!(err.contains("declaration"), "{err}");
    }

    #[test]
    fn a_feature_declaration_may_only_name_a_feature_key() {
        let bad = CAPABILITY_TOML.replacen(
            "[probe.\"features.spawn\"]",
            "[probe.\"features.spwan\"]",
            1,
        );
        let err = HarnessContract::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("spwan"), "{err}");
        assert!(err.contains("feature keys"), "{err}");
    }

    #[test]
    fn a_row_without_a_features_table_loads_with_no_refusal() {
        let stanza = "[harness.agy.features.spawn]\nstate = \"absent\"\n";
        let stripped = CAPABILITY_TOML.replacen(stanza, "", 1);
        let contract = HarnessContract::parse(&stripped).unwrap();
        assert!(contract.capabilities("agy").unwrap().features.is_empty());
    }
}
