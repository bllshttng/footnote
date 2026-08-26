use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;
use serde::Deserialize;

pub const CAPABILITY_TOML: &str = include_str!("harness_capabilities.toml");

const SESSION_LANES: [&str; 5] = [
    "interactive_create",
    "interactive_resume",
    "interactive_attach",
    "headless_create",
    "headless_resume",
];
const RESPONSE_ACTIONS: [&str; 3] = ["allow_once", "allow_always", "deny"];
const RESUME_KINDS: [&str; 4] = ["flag", "subcommand", "session_flag", "unsupported"];
const MODEL_SWITCH_KINDS: [&str; 3] = ["direct", "menu_walk", "unsupported"];
const MODEL_SWITCH_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const STOP_STRATEGIES: [&str; 2] = ["claude-short-id", "registry-noop"];
const REMOVE_STRATEGIES: [&str; 3] = ["claude-short-id", "codex-session-index", "registry-only"];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("harness capability contract: {0}")]
pub struct ContractError(String);

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessContract {
    pub map_version: u32,
    pub harness: BTreeMap<String, HarnessCapabilities>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessCapabilities {
    pub permission_bypass: Vec<String>,
    pub resume: String,
    pub thread: bool,
    pub autonomous_pane: bool,
    pub route_on_pane: bool,
    pub stop_hook: String,
    pub command_surface: String,
    #[serde(default)]
    pub slash_prefix: String,
    pub ready_marker: String,
    pub ready_rule_ids: Vec<String>,
    pub manifest_rules: Vec<ManifestRuleRef>,
    pub send_keys_enter_delay_ms: i64,
    pub submit_keys: Vec<String>,
    pub stop_strategy: String,
    pub remove_strategy: String,
    pub session_binding: SessionBinding,
    pub permission_response: BTreeMap<String, PermissionResponse>,
    pub resume_strategy: ResumeStrategy,
    pub model_switch_strategy: ModelSwitchStrategy,
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

impl HarnessContract {
    pub fn packaged() -> Result<Self, ContractError> {
        Self::parse(CAPABILITY_TOML)
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
        for (harness, caps) in &self.harness {
            let manifest = crate::manifest::load_manifest(harness, None)
                .ok_or_else(|| field_error(harness, "ready_marker", "no bundled manifest"))?
                .map_err(|error| field_error(harness, "ready_marker", &error.to_string()))?;
            for declared in &caps.manifest_rules {
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
            if caps.ready_marker != "unsupported"
                && !caps.ready_rule_ids.contains(&caps.ready_marker)
            {
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
                    || (lane == "interactive_attach"
                        && form.kind != "unsupported"
                        && !form.tokens.iter().any(|token| token == "{short_id}"))
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
            }
            validate_model_switch_strategy(harness, &caps.model_switch_strategy)?;
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
                "store-lookup",
                "unsupported",
            ]
            .contains(&caps.session_binding.strategy.as_str())
                || (caps.session_binding.required && caps.session_binding.timeout_ms == 0)
            {
                return Err(field_error(harness, "session_binding", "invalid strategy"));
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
            return Ok(tokens);
        }
        if short_id.is_some_and(|id| !id.is_empty()) {
            return Err(field_error(
                harness,
                "resume_strategy",
                &format!("lane {lane:?} accepts a session_id, not a short_id"),
            ));
        }
        let Some(index) = form.tokens.iter().position(|token| token == "{session_id}") else {
            return Ok(form.tokens.clone());
        };
        if let Some(id) = session_id.filter(|id| !id.is_empty()) {
            let mut tokens = form.tokens.clone();
            tokens[index] = id.to_string();
            return Ok(tokens);
        }
        if lane.ends_with("create") {
            let start = index
                .checked_sub(1)
                .filter(|prior| form.tokens[*prior].starts_with('-'))
                .unwrap_or(index);
            let mut tokens = form.tokens.clone();
            tokens.drain(start..=index);
            return Ok(tokens);
        }
        Err(field_error(
            harness,
            "resume_strategy",
            &format!("lane {lane:?} needs a non-empty session id"),
        ))
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

fn field_error(harness: &str, field: &str, detail: &str) -> ContractError {
    ContractError(format!("harness {harness:?} field {field:?}: {detail}"))
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
        assert_eq!(
            CAPABILITY_TOML,
            include_str!("../../../cli/src/fno/agents/harness_capabilities.toml"),
            "Python and packaged Rust contract copies diverged",
        );
        let contract = HarnessContract::packaged().unwrap();
        assert_eq!(contract.map_version, 9);
        assert_eq!(
            contract.harness.keys().cloned().collect::<Vec<_>>(),
            ["agy", "claude", "codex", "gemini", "opencode"]
        );
        for (name, caps) in &contract.harness {
            assert_eq!(caps.permission_response.len(), 3, "{name}");
            assert_eq!(caps.resume_strategy.forms.len(), 5, "{name}");
            assert!(!caps.model_switch_strategy.kind.is_empty(), "{name}");
            assert!(!caps.submit_keys.is_empty(), "{name}");
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
        assert!(contract
            .render_session_argv("agy", "interactive_resume", Some("x"))
            .unwrap_err()
            .to_string()
            .contains("unsupported"));
    }

    #[test]
    fn contract_exposes_permission_input_and_teardown_differences() {
        let contract = HarnessContract::packaged().unwrap();
        let claude = contract.capabilities("claude").unwrap();
        let codex = contract.capabilities("codex").unwrap();
        let opencode = contract.capabilities("opencode").unwrap();
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
}
