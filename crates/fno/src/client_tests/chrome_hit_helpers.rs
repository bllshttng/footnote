//! Two assertion helpers over `ChromeHit`, moved out of `client_tests.rs`
//! (file budget). `chrome_hit_label` is exhaustive on purpose: a new variant
//! fails to compile here rather than reading as an unhelpful panic message.

use super::*;

pub(super) fn cmds(hit: Option<ChromeHit>) -> Vec<Command> {
    match hit {
        Some(ChromeHit::Cmds(c)) => c,
        other => panic!("expected Cmds, got {}", chrome_hit_label(&other)),
    }
}

pub(super) fn chrome_hit_label(hit: &Option<ChromeHit>) -> &'static str {
    match hit {
        None => "None",
        Some(ChromeHit::Cmds(_)) => "Cmds",
        Some(ChromeHit::Notice(_)) => "Notice",
        Some(ChromeHit::Confirm(_)) => "Confirm",
        Some(ChromeHit::OpenCreate) => "OpenCreate",
        Some(ChromeHit::CycleSection(_)) => "CycleSection",
        Some(ChromeHit::SortColumn(_)) => "SortColumn",
        Some(ChromeHit::ToggleIdle(_)) => "ToggleIdle",
        Some(ChromeHit::OpenSidelineMenu { .. }) => "OpenSidelineMenu",
        Some(ChromeHit::CycleDensity) => "CycleDensity",
        Some(ChromeHit::OpenFeedDetail(_)) => "OpenFeedDetail",
    }
}
