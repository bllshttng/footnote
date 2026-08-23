use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use fno_agents::harness_capabilities::{HarnessContract, CAPABILITY_TOML};
use fno_agents::provider::{
    for_name, gemini_session_id_from_blob, CreateContext, ResumeContext, KNOWN_PROVIDERS,
};
use fno_agents::ParsedEvent;

const SESSION_ID: &str = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4";

fn create_context() -> CreateContext {
    CreateContext {
        name: "provider-contract".into(),
        message: "a provider-contract seed".into(),
        cwd: PathBuf::from("/tmp/provider-contract-cwd"),
        from_name: None,
        session_id: Some(SESSION_ID.into()),
        yolo: false,
        reasoning_effort: None,
        append_system_prompt: None,
    }
}

fn resume_context() -> ResumeContext {
    ResumeContext {
        session_id: SESSION_ID.into(),
        message: "a provider-contract follow-up".into(),
        cwd: PathBuf::from("/tmp/provider-contract-cwd"),
        from_name: None,
        yolo: false,
    }
}

fn assert_ordered_tokens(harness: &str, actual: &[String], expected: &[String], lane: &str) {
    let mut offset = 0;
    for token in expected {
        let Some(relative) = actual[offset..]
            .iter()
            .position(|candidate| candidate == token)
        else {
            panic!(
                "harness {harness} {lane} argv is missing capability token {token:?}: {actual:?}"
            );
        };
        offset += relative + 1;
    }
}

fn validate_roster(roster: &[&str], contract: &HarnessContract) -> Result<(), String> {
    for name in roster {
        let provider = for_name(name)
            .ok_or_else(|| format!("harness {name:?} has no provider implementation coverage"))?;
        contract
            .capabilities(name)
            .map_err(|error| format!("harness {name:?} has no capability coverage: {error}"))?;
        if provider.name() != *name {
            return Err(format!(
                "harness {name:?} resolves to provider {:?}",
                provider.name()
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum SubmitObservation {
    Accepted,
    Refused,
    Unverified(&'static str),
}

fn validate_submit_claims(
    roster: &[&str],
    contract: &HarnessContract,
    observations: &BTreeMap<&str, SubmitObservation>,
) -> Result<(), String> {
    for name in roster {
        let caps = contract
            .capabilities(name)
            .map_err(|error| format!("harness {name:?} capability lookup failed: {error}"))?;
        match observations
            .get(name)
            .copied()
            .unwrap_or(SubmitObservation::Unverified("no behavior observation"))
        {
            SubmitObservation::Accepted if caps.submit_keys == vec!["unsupported".to_string()] => {
                return Err(format!(
                    "harness {name} capability submit_keys declares unsupported, but behavior accepted submit"
                ));
            }
            SubmitObservation::Refused if caps.submit_keys != vec!["unsupported".to_string()] => {
                return Err(format!(
                    "harness {name} capability submit_keys declares {:?}, but behavior refused submit",
                    caps.submit_keys
                ));
            }
            SubmitObservation::Unverified(reason) => {
                eprintln!("SELF-SKIP provider-contract {name}.submit_keys: {reason}");
            }
            _ => {}
        }
    }
    Ok(())
}

fn binary_help(harness: &str) -> Option<String> {
    let output = Command::new(harness).arg("--help").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (!text.trim().is_empty()).then_some(text)
}

#[test]
fn roster_drives_provider_argv_and_capability_coverage() {
    let contract = HarnessContract::packaged().expect("packaged capability contract");
    validate_roster(KNOWN_PROVIDERS, &contract).unwrap();
    let create = create_context();
    let resume = resume_context();

    for name in KNOWN_PROVIDERS {
        let provider = for_name(name).expect("roster provider implementation");
        let expected_create = contract
            .render_session_argv(name, "headless_create", Some(SESSION_ID))
            .unwrap();
        let expected_resume = contract
            .render_session_argv(name, "headless_resume", Some(SESSION_ID))
            .unwrap();
        let create_argv = provider.create_argv(&create);
        let resume_argv = provider.resume_argv(&resume);
        assert_ordered_tokens(name, &create_argv, &expected_create, "headless_create");
        assert_ordered_tokens(name, &resume_argv, &expected_resume, "headless_resume");
    }
}

#[test]
fn roster_name_without_coverage_is_a_named_failure() {
    let contract = HarnessContract::packaged().unwrap();
    let mut roster = KNOWN_PROVIDERS.to_vec();
    roster.push("new-harness");
    let error = validate_roster(&roster, &contract).unwrap_err();
    assert!(error.contains("new-harness"), "{error}");
    assert!(error.contains("coverage"), "{error}");
}

#[test]
fn wrong_capability_entry_is_a_named_failure() {
    let marker = "[harness.agy]";
    let start = CAPABILITY_TOML.find(marker).unwrap();
    let (prefix, section) = CAPABILITY_TOML.split_at(start);
    let section = section.replacen(
        "submit_keys = [\"enter\"]",
        "submit_keys = [\"unsupported\"]",
        1,
    );
    let bad_toml = format!("{prefix}{section}");
    let contract = HarnessContract::parse(&bad_toml).unwrap();
    let mut observations = BTreeMap::new();
    observations.insert("agy", SubmitObservation::Accepted);
    let error = validate_submit_claims(KNOWN_PROVIDERS, &contract, &observations).unwrap_err();
    assert!(error.contains("agy"), "{error}");
    assert!(error.contains("submit_keys"), "{error}");
}

#[test]
fn refused_behavior_rejects_a_supported_capability_claim() {
    let marker = "[harness.gemini]";
    let start = CAPABILITY_TOML.find(marker).unwrap();
    let (prefix, section) = CAPABILITY_TOML.split_at(start);
    let section = section.replacen(
        "submit_keys = [\"unsupported\"]",
        "submit_keys = [\"enter\"]",
        1,
    );
    let bad_toml = format!("{prefix}{section}");
    let contract = HarnessContract::parse(&bad_toml).unwrap();
    let mut observations = BTreeMap::new();
    observations.insert("gemini", SubmitObservation::Refused);
    let error = validate_submit_claims(KNOWN_PROVIDERS, &contract, &observations).unwrap_err();
    assert!(error.contains("gemini"), "{error}");
    assert!(error.contains("submit_keys"), "{error}");
}

#[test]
#[ignore = "requires a real harness binary driven through an interactive PTY"]
fn submit_claims_are_checked_against_behavior_or_named_skip() {
    let contract = HarnessContract::packaged().unwrap();
    let observations = KNOWN_PROVIDERS
        .iter()
        .map(|name| {
            let observation = if binary_help(name).is_some() {
                SubmitObservation::Unverified(
                    "binary is present, but submit requires an interactive PTY probe",
                )
            } else {
                SubmitObservation::Unverified("binary is unavailable in this environment")
            };
            (*name, observation)
        })
        .collect();
    validate_submit_claims(KNOWN_PROVIDERS, &contract, &observations).unwrap();
}

#[test]
fn agy_binary_effort_surface_is_driven_when_available() {
    let Some(help) = binary_help("agy") else {
        eprintln!("SELF-SKIP provider-contract agy --effort: agy binary unavailable");
        return;
    };
    assert!(
        help.contains("--effort"),
        "agy --help omitted --effort: {help}"
    );
    for value in ["low", "medium", "high"] {
        assert!(
            help.contains(value),
            "agy --help omitted effort value {value}: {help}"
        );
    }
}

#[test]
fn stream_fixtures_map_into_the_sealed_vocabulary() {
    let fixtures = [
        ("claude", "backgrounded · 7c5dcf5d · provider-contract"),
        ("codex", r#"{"type":"thread.started","thread_id":"cx-123"}"#),
        (
            "gemini",
            r#"{"response":"hello","stats":{"models":{"m":{"api":{"totalLatencyMs":12}}}}}"#,
        ),
        ("agy", "plain-text provider reply"),
        ("opencode", "plain-text provider reply"),
    ];
    for (name, chunk) in fixtures {
        let event = for_name(name).unwrap().parse_stream_event(chunk);
        assert!(
            !matches!(event, ParsedEvent::Unknown { .. }),
            "{name}: {event:?}"
        );
        assert!(matches!(
            for_name(name).unwrap().parse_stream_event(""),
            ParsedEvent::Unknown { .. }
        ));
    }
    assert_eq!(
        gemini_session_id_from_blob(r#"{"session_id":"gem-123","response":"hello"}"#),
        Some("gem-123".into())
    );
    assert!(matches!(
        for_name("claude").unwrap().parse_stream_event("backgrounded · 7c5dcf5d · worker"),
        ParsedEvent::SessionCreated { session_id } if session_id == "7c5dcf5d"
    ));
    assert!(matches!(
        for_name("codex").unwrap().parse_stream_event(
            r#"{"type":"item.started","item":{"type":"command_execution","command":"pwd"}}"#
        ),
        ParsedEvent::ToolUse { name, .. } if name == "command_execution"
    ));
}

#[test]
fn capability_copies_are_byte_identical() {
    assert_eq!(
        CAPABILITY_TOML,
        include_str!("../../../cli/src/fno/agents/harness_capabilities.toml")
    );
}
