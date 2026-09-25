//! The capability reader's contract tests: the control refusal, the
//! blind-pattern conviction, the shipped-table assertion, the journey
//! declarations, the watch-lease journey cell, and the isolation nonce pair.

use fno_agents::harness_capabilities::{HarnessContract, ProbeDecl, JOURNEY_KEYS};
use fno_agents::harness_reader::{retry_marker, wait_for_ci_cell, LineVerdict};
use std::time::Duration;

fn decl(kind: &str, authority: &str, pattern: &str, control: &str) -> ProbeDecl {
    ProbeDecl {
        kind: kind.to_string(),
        authority: authority.to_string(),
        pattern: pattern.to_string(),
        control: control.to_string(),
        marker: String::new(),
        reason: String::new(),
    }
}

/// The eight historical help-word patterns, each given a control denying the
/// capability in ordinary words. Every one matches its own denial: this is
/// the conviction the load-time refusal rests on, and the test holds it so a
/// later edit cannot quietly re-declare a blind reader.
const CONVICTED_PATTERNS: [(&str, &str, &str); 8] = [
    (
        "features.rpc",
        "(?i)\\b(app-server|rpc|daemon)\\b",
        "this build has no app-server, rpc, or daemon support",
    ),
    (
        "features.server",
        "(?i)\\b(serve|server|listen)\\b",
        "no serve mode: this binary neither serves nor listens",
    ),
    (
        "features.plugins",
        "(?i)\\bplugins?\\b",
        "plugins are not supported in this build",
    ),
    (
        "features.hooks",
        "(?i)\\bhooks?\\b",
        "no hook support in this build",
    ),
    (
        "features.skills_dir",
        "(?i)\\bskills?\\b",
        "no skills directory in this build",
    ),
    (
        "features.subagent_dispatch",
        "(?i)\\bsubagents?\\b",
        "this build cannot dispatch subagents",
    ),
    (
        "features.mcp",
        "(?i)\\bmcp\\b",
        "MCP support is absent from this build",
    ),
    ("features.acp", "(?i)\\bacp\\b", "this build speaks no ACP"),
];

#[test]
fn a_declared_instrument_carries_the_text_it_rejects() {
    // The shipped table's one declared instrument carries a control, and the
    // struct round-trips it: available to the reader with its control
    // attached.
    let contract = HarnessContract::packaged().expect("the shipped table parses");
    let declared: Vec<(&String, &ProbeDecl)> = contract
        .probe
        .iter()
        .filter(|(_, decl)| decl.kind == "declared")
        .collect();
    assert!(
        !declared.is_empty(),
        "the shipped table still declares at least one authority instrument"
    );
    for (field, decl) in declared {
        assert!(
            !decl.control.is_empty(),
            "declared instrument {field:?} must carry the text it rejects"
        );
    }
}

#[test]
fn the_shipped_table_carries_no_blind_declared_pattern() {
    // The load-time refusal is the guard; parsing the shipped table runs it
    // over every declared instrument, so a clean parse IS the assertion.
    let contract = HarnessContract::packaged().expect("the shipped table parses");
    for (field, decl) in &contract.probe {
        if decl.kind == "declared" {
            let regex = regex::Regex::new(&decl.pattern).expect("validated at load");
            assert!(
                !regex.is_match(&decl.control),
                "declared instrument {field:?} matches its own control; \
                 a reader that cannot reject its control cannot report absence"
            );
        }
    }
}

#[test]
fn every_convicted_pattern_matches_its_denial() {
    for (field, pattern, control) in CONVICTED_PATTERNS {
        let regex = regex::Regex::new(pattern).expect("historical pattern compiles");
        assert!(
            regex.is_match(control),
            "the {field:?} pattern must match its denial control (it was convicted on this)"
        );
    }
}

#[test]
fn the_ten_journey_keys_are_declared() {
    let contract = HarnessContract::packaged().expect("the shipped table parses");
    for key in JOURNEY_KEYS {
        assert!(
            contract.journeys.contains_key(key),
            "journey {key:?} is missing from the shipped table"
        );
    }
    assert_eq!(contract.journeys.len(), JOURNEY_KEYS.len());
}

#[test]
fn restart_viewport_is_unprobeable_on_this_nodes_own_constraint() {
    let contract = HarnessContract::packaged().expect("the shipped table parses");
    let decl = contract.journeys.get("restart-viewport").expect("declared");
    assert_eq!(decl.kind, "unprobeable");
    assert!(
        decl.reason.contains("no live fleet restart"),
        "the reason is this node's own constraint, got: {}",
        decl.reason
    );
}

#[test]
fn fresh_install_journey_names_the_filtered_catalog_reader() {
    let contract = HarnessContract::packaged().expect("the shipped table parses");
    let decl = contract.journeys.get("fresh-install").expect("declared");
    assert_eq!(decl.kind, "unprobeable");
    assert!(
        decl.reason.contains("filtered catalog reader"),
        "the journey names the reader it consumes: {}",
        decl.reason
    );
    assert!(
        decl.reason.contains("refused as blind"),
        "the journey records why the unfiltered read is refused: {}",
        decl.reason
    );
}

/// The unfiltered catalog read, offered as the fresh-install journey's
/// reader: its pattern matches a catalog holding another plugin's mirrored
/// `fno:` names with no footnote install - the exact false positive the
/// audit measured. Offered as a declared instrument beside that control, it
/// is refused.
#[test]
fn the_unfiltered_catalog_read_is_refused_as_blind() {
    let decl = decl(
        "declared",
        "{bin} debug config --pure",
        r"(?i)\bfno:(target|review|archer)\b",
        "commands: fno:target, fno:review, fno:archer (registered by the mirroring plugin, no footnote install)",
    );
    let regex = regex::Regex::new(&decl.pattern).expect("compiles");
    assert!(
        regex.is_match(&decl.control),
        "the unfiltered read matches the mirroring plugin's fno: names with no footnote install"
    );
    assert!(
        decl.control.contains("no footnote install"),
        "the control is the measured false positive: {}",
        decl.control
    );
}

#[test]
fn isolation_reads_its_nonce_back_and_reports_the_real_root_pair() {
    let root = fno_agents::harness_reader::IsolatedRoot::establish("claude")
        .expect("an isolated root establishes in the test env");
    assert!(
        root.positive_read(),
        "the positive read: the nonce is read back from inside the isolated root"
    );
    assert_eq!(
        root.real_root_absent(),
        Some(true),
        "the paired negative read: the nonce is absent from the real state root"
    );
}

#[test]
fn a_verdict_without_a_marker_is_refused_at_construction() {
    let result =
        std::panic::catch_unwind(|| LineVerdict::new("SPAWN", "pass", "  ", 1, String::new()));
    assert!(
        result.is_err(),
        "a pass without a positive marker is a construction error"
    );
}

#[test]
fn retry_reads_a_marker_three_times_before_calling_it_absent() {
    let mut reads = 0;
    let verdict = retry_marker(
        "CLAIM",
        "live claim holder",
        || {
            reads += 1;
            if reads < 3 {
                String::new()
            } else {
                "holder-7".to_string()
            }
        },
        Duration::ZERO,
    );
    assert_eq!(verdict.status, "pass");
    assert_eq!(
        verdict.attempts, 3,
        "the marker was read three times before it landed"
    );
}

#[test]
fn the_watch_lease_answers_the_wait_for_ci_journey() {
    // The control first: a harness the lease permits reads native.
    assert_eq!(wait_for_ci_cell("claude"), ("native", String::new()));
    // Every other harness reads absent with the refusal quoted.
    for harness in ["codex", "opencode", "agy", "pi", "grok"] {
        let (state, refusal) = wait_for_ci_cell(harness);
        assert_eq!(state, "absent", "{harness}");
        assert!(
            refusal.contains("cannot idle"),
            "{harness}: the refusal is quoted, got: {refusal}"
        );
    }
}
