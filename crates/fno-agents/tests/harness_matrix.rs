//! The renderer's contract tests: measured and unmeasured cells, the three
//! distinct unmeasured reasons, the version-named cell, the denominator
//! sentences, the loop refusal, and byte-identical regeneration.

use fno_agents::harness_capabilities::{FeatureClaim, HarnessContract, MeasuredBy, ProbeDecl};
use fno_agents::harness_matrix::BLIND_REFUSAL_MARKER;

fn claim(state: &str) -> FeatureClaim {
    FeatureClaim {
        state: state.to_string(),
        verbs: vec![],
        measured_by: None,
    }
}

fn measured(state: &str, reader: &str, version: &str, date: &str) -> FeatureClaim {
    FeatureClaim {
        state: state.to_string(),
        verbs: vec![],
        measured_by: Some(MeasuredBy {
            reader: reader.to_string(),
            version: version.to_string(),
            date: date.to_string(),
        }),
    }
}

fn unprobeable_decl(reason: &str) -> ProbeDecl {
    ProbeDecl {
        kind: "unprobeable".to_string(),
        authority: String::new(),
        pattern: String::new(),
        control: String::new(),
        marker: String::new(),
        reason: reason.to_string(),
    }
}

fn harness_cell(decl: Option<&ProbeDecl>, claim: Option<&FeatureClaim>) -> String {
    fno_agents::harness_matrix::features_cell(decl, claim)
}

#[test]
fn a_claim_with_a_receipt_prints_its_state_and_date() {
    let cell = harness_cell(
        None,
        Some(&measured(
            "native",
            "agy spawn journey",
            "1.1.24",
            "2026-09-03",
        )),
    );
    assert!(cell.starts_with("`native` ("), "{cell}");
    assert!(cell.contains("2026-09-03"), "{cell}");
}

#[test]
fn a_receipt_carrying_a_version_names_the_measured_version() {
    let cell = harness_cell(
        None,
        Some(&measured(
            "native",
            "pi keeper journey",
            "0.84.2",
            "2026-09-01",
        )),
    );
    assert!(
        cell.contains("0.84.2"),
        "the cell names the version measured on: {cell}"
    );
}

#[test]
fn a_claim_without_a_receipt_prints_the_reason_the_state_is_unprinted() {
    let decl = unprobeable_decl(
        format!("{BLIND_REFUSAL_MARKER}: the word match caught a denial").as_str(),
    );
    let blind = harness_cell(Some(&decl), Some(&claim("native")));
    assert_eq!(blind, "`unmeasured` (reader refused as blind)");

    let runnable = harness_cell(
        Some(&ProbeDecl {
            kind: "behavioral".to_string(),
            authority: String::new(),
            pattern: String::new(),
            control: String::new(),
            marker: "the marker".to_string(),
            reason: String::new(),
        }),
        Some(&claim("native")),
    );
    assert_eq!(runnable, "`unmeasured` (reader declared but not run)");

    let no_reader = harness_cell(None, Some(&claim("absent")));
    assert_eq!(no_reader, "`unmeasured` (no reader declared)");
}

#[test]
fn zero_of_zero_and_zero_of_eleven_render_as_different_sentences() {
    assert_eq!(
        fno_agents::harness_matrix::denominator_line("hermes", 0, 0),
        "- hermes: no feature keys are declared, so nothing can be measured"
    );
    assert_eq!(
        fno_agents::harness_matrix::denominator_line("claude", 0, 11),
        "- claude: 0 of 11 measured"
    );
    assert!(fno_agents::harness_matrix::denominator_line("agy", 1, 11).contains("1 of 11"));
}

#[test]
fn the_grok_loop_cell_reads_absent_with_the_refusal_quoted() {
    let contract = HarnessContract::packaged().expect("the shipped table parses");
    let (state, refusal) =
        fno_agents::harness_matrix::loop_cell(contract.capabilities("grok").ok());
    assert_eq!(
        state, "absent",
        "the grok loop cell reads absent, not native"
    );
    assert!(
        refusal
            .as_ref()
            .is_some_and(|r| r.contains("nothing invokes loop-check")),
        "the refusal is quoted: {refusal:?}"
    );
    // The control: a harness whose loop extension is shipped, which the same
    // reader admits as native.
    for admitted in ["opencode", "pi"] {
        let (state, refusal) =
            fno_agents::harness_matrix::loop_cell(contract.capabilities(admitted).ok());
        assert_eq!(
            state, "native",
            "{admitted}: the shipped extension is admitted"
        );
        assert!(refusal.is_none());
    }
}

#[test]
fn the_rendered_matrix_is_byte_identical_across_runs() {
    let contract = HarnessContract::packaged().expect("the shipped table parses");
    let roster = [
        "claude",
        "codex",
        "gemini",
        "agy",
        "opencode",
        "pi",
        "hermes",
        "openclaw",
        "cursor-agent",
        "grok",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect::<Vec<_>>();
    let first = fno_agents::harness_matrix::render_matrix(&contract, &roster);
    let second = fno_agents::harness_matrix::render_matrix(&contract, &roster);
    assert_eq!(first, second, "regeneration is byte-identical");
}
