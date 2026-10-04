//! `fno-agents terminals`: the daemon-free JSON read of the delivered-terminal
//! vocabulary. The one owner is [`TerminationReason::is_delivered`]; Python's
//! ledger promotion gate, scoreboard fold, and delivery reference read this
//! verb instead of carrying a set copy that could drift from the enum.
//!
//! Transport-only and unregistered in `ALL_CLIENT_ACTIONS` (the shrink law,
//! d-fe66560a, allows no new client action): every caller reaches it through
//! `resolve_binary`, like `harness-roster`.

use crate::client_verbs::to_python_json;
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
