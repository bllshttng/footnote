//! The machine-output contract. See docs/architecture/json-output-contract.md.

/// True for the two spellings of the machine-output request.
pub fn is_flag(arg: &str) -> bool {
    arg == "--json" || arg == "-J"
}

/// True when the caller asked for JSON before any `--argv` or `--` boundary.
pub fn requested(args: &[String]) -> bool {
    args.iter()
        .take_while(|a| a.as_str() != "--argv" && a.as_str() != "--")
        .any(|a| is_flag(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn requested_accepts_both_spellings() {
        assert!(requested(&v(&["--json"])));
        assert!(requested(&v(&["-J"])));
        assert!(requested(&v(&["--scope", "s", "-J"])));
    }

    #[test]
    fn requested_stops_at_payload_boundary() {
        assert!(!requested(&v(&["--argv", "-J"])));
        assert!(!requested(&v(&["--", "-J"])));
        assert!(!requested(&v(&["--scope", "s"])));
    }

    #[test]
    fn is_flag_accepts_only_exact_spellings() {
        assert!(is_flag("--json"));
        assert!(is_flag("-J"));
        assert!(!is_flag("--JSON"));
        assert!(!is_flag("-j"));
        assert!(!is_flag("--json=1"));
    }
}
