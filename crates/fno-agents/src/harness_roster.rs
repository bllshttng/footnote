//! `fno-agents harness-roster`: the daemon-free JSON read of the one harness
//! roster. Python's `fno.harness_names` proxies this instead of
//! carrying a tuple copy of `provider::KNOWN_HARNESSES`; the parity gate
//! parses the same const out of the Rust source, so a name added in Rust
//! alone reaches every Python reader.
//!
//! Transport-only and unregistered in `ALL_CLIENT_ACTIONS` (the shrink law,
//! d-fe66560a, allows no new client action): every caller reaches it through
//! `resolve_binary`, like `sync-canonical`.

use crate::client_verbs::to_python_json;
use crate::provider::KNOWN_HARNESSES;
use serde_json::json;

/// Takes no arguments and starts nothing; the roster is a compile-time const.
pub fn run_harness_roster(rest: &[String]) -> i32 {
    if let Some(arg) = rest.first() {
        eprintln!("fno-agents: harness-roster takes no arguments (got: {arg})");
        return 2;
    }
    println!("{}", to_python_json(&roster_json()));
    0
}

fn roster_json() -> serde_json::Value {
    json!({ "known": KNOWN_HARNESSES })
}

// The read's substance is pinned on the consumer side: Python's roster
// content test reads this verb through the real binary, and provider.rs's
// round-trip test pins the const's subset and name format.
