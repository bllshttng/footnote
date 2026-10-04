//! `fno-agents terminals`: the daemon-free JSON read of the delivered-terminal
//! vocabulary. The one owner is [`TerminationReason::is_delivered`]; Python's
//! ledger promotion gate, scoreboard fold, and delivery reference read this
//! verb instead of carrying a set copy that could drift from the enum.
//!
//! Transport-only and unregistered in `ALL_CLIENT_ACTIONS` (the shrink law,
//! d-fe66560a, allows no new client action): every caller reaches it through
//! `resolve_binary`, like `harness-roster`.

use crate::client_verbs::to_python_json;
use crate::loopcheck::TerminationReason;
use crate::scoreboard::ALL_TERMINALS;
use serde_json::json;

/// Takes no arguments and starts nothing; the vocabulary is a compile-time
/// match over the terminal enum.
pub fn run_terminals(rest: &[String]) -> i32 {
    if let Some(arg) = rest.first() {
        eprintln!("fno-agents: terminals takes no arguments (got: {arg})");
        return 2;
    }
    let mut delivered: Vec<String> = ALL_TERMINALS
        .iter()
        .filter(|t| t.is_delivered())
        .filter_map(|t| serde_json::to_value(t).ok())
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    delivered.sort();
    println!("{}", to_python_json(&json!({ "delivered": delivered })));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name_of(t: &TerminationReason) -> String {
        match serde_json::to_value(t) {
            Ok(serde_json::Value::String(s)) => s,
            _ => String::new(),
        }
    }

    #[test]
    fn delivered_lists_exactly_the_is_delivered_variants() {
        let listed: Vec<String> = ALL_TERMINALS
            .iter()
            .filter(|t| t.is_delivered())
            .map(name_of)
            .collect();
        assert_eq!(
            listed,
            vec![
                "DoneAdvisory".to_string(),
                "DoneBatched".to_string(),
                "DoneDelivery".to_string(),
                "DonePRGreen".to_string(),
            ]
        );
        for terminal in ALL_TERMINALS {
            assert_eq!(
                listed.contains(&name_of(&terminal)),
                terminal.is_delivered()
            );
        }
    }
}
