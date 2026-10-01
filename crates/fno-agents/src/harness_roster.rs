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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_read_serves_the_roster_const_under_known() {
        let parsed = roster_json();
        let served: Vec<&str> = parsed["known"]
            .as_array()
            .expect("known is an array")
            .iter()
            .map(|v| v.as_str().expect("roster names are strings"))
            .collect();
        assert_eq!(served, KNOWN_HARNESSES.to_vec());
    }

    #[test]
    fn the_read_refuses_arguments() {
        let arg = ["stray".to_string()].to_vec();
        assert_eq!(run_harness_roster(&arg), 2);
    }
}
