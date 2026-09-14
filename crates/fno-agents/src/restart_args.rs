//! The `fno-agents restart` argv contract as a named parse.
//!
//! Python's `fno agents restart` shells this verb, so the accepted set is a
//! cross-language contract. A named function lets a unit test pin it without
//! restarting a daemon.

/// Returns the `--force` break-glass flag, or the refusal text for anything
/// else. Keep the refusal wording stable: `cli/src/fno/restart.py` quotes it.
pub fn parse_restart_args(args: &[String]) -> Result<bool, String> {
    match args {
        [] => Ok(false),
        [f] if f == "--force" => Ok(true),
        other => Err(format!(
            "fno-agents: restart takes no arguments besides --force (got: {})",
            other.join(" ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_argv_is_a_plain_restart() {
        assert_eq!(parse_restart_args(&args(&[])), Ok(false));
    }

    #[test]
    fn lone_force_is_the_break_glass() {
        assert_eq!(parse_restart_args(&args(&["--force"])), Ok(true));
    }

    #[test]
    fn json_flag_is_refused_naming_the_argv() {
        let err = parse_restart_args(&args(&["--json"])).unwrap_err();
        assert!(err.contains("takes no arguments besides --force"));
        assert!(err.contains("got: --json"));
    }

    #[test]
    fn multiple_args_are_refused() {
        assert!(parse_restart_args(&args(&["--force", "--force"])).is_err());
    }
}
