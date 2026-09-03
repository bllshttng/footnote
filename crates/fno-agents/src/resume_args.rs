//! The `fno agents resume` argv parser.
//!
//! LIVES OUTSIDE client_verbs for the same reason usage.rs does: the file
//! is over the 5,000-line budget and shrink-only, and this parser is what
//! every wake touches. The row's recorded launch account stays the binding
//! authority; `--account` parses so the seam's appended flag never exits 2
//! at argv (run_resume validates the value it carries).

use crate::client_verbs::{echo_extra, expand_eq};

/// Expanded from client_verbs.rs where it lived beside `run_resume`; the
/// move is mechanical, the shape unchanged.
pub fn parse_resume_args(
    rest: &[String],
) -> Result<
    (
        String,
        bool,
        Option<String>,
        bool,
        Option<String>,
        Option<String>,
    ),
    i32,
> {
    let mut name: Option<String> = None;
    let mut print_command = false;
    let mut message: Option<String> = None;
    let mut cross_project = false;
    let mut cwd: Option<String> = None;
    let mut account: Option<String> = None;
    // Every sibling parser in this file (`parse_trace_args`, `parse_logs_args`)
    // expands `--flag=value` into `--flag value` before iterating; without it
    // `--message=continue` falls into the `starts_with("--")` unknown-flag arm
    // instead of being recognized.
    let rest = expand_eq(rest);
    let mut iter = rest.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--print-command" => print_command = true,
            "--cross-project" => cross_project = true,
            "--message" | "-m" => {
                message = Some(match iter.next() {
                    Some(v) => v.clone(),
                    None => {
                        eprintln!("fno-agents: {a} needs a value");
                        return Err(2);
                    }
                });
            }
            "--cwd" => {
                cwd = Some(match iter.next() {
                    Some(v) if !v.starts_with("--") => v.clone(),
                    _ => {
                        eprintln!("fno-agents: --cwd needs a value");
                        return Err(2);
                    }
                });
            }
            "--account" => {
                // x-5cef: the spawn seam's account picker rides the shared
                // worker-dir seam, so a wake arrives with `--account` appended.
                // The spawn arm parses the flag; this arm refused it at parse,
                // which exited 2 before the name ever resolved and broke the
                // wake ladder three receipts advertise. Parse it here.
                account = Some(match iter.next() {
                    Some(v) if !v.starts_with("--") => v.clone(),
                    _ => {
                        eprintln!("fno-agents: --account needs a value");
                        return Err(2);
                    }
                });
            }
            other if other.starts_with("--") => {
                eprintln!("fno-agents: unknown resume flag: {other}");
                return Err(2);
            }
            other => {
                if name.is_some() {
                    // The remedy, not just the refusal: `resume` reattaches a
                    // session and carries no message, so the operator who
                    // typed a prompt wanted `ask`. Matches the sibling
                    // refusal for a live pane worker, which already ends in
                    // the command to run instead.
                    eprintln!(
                        "fno-agents: resume takes one NAME and no prompt (got extra: {}).",
                        echo_extra(other)
                    );
                    eprintln!(
                        "resume reattaches a session; it does not carry a message. \
                         Send one with: fno agents ask <name> \"<prompt>\""
                    );
                    return Err(2);
                }
                name = Some(other.to_string());
            }
        }
    }
    match name {
        Some(n) => Ok((n, print_command, message, cross_project, cwd, account)),
        None => {
            eprintln!("fno-agents: resume needs a <name>");
            Err(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_args_accept_cross_project_and_replacement_cwd_forms() {
        let parsed = parse_resume_args(&[
            "full-session-id".to_string(),
            "--cross-project".to_string(),
            "--cwd".to_string(),
            "/replacement/checkout".to_string(),
        ])
        .unwrap();
        assert_eq!(parsed.0, "full-session-id");
        assert!(parsed.3);
        assert_eq!(parsed.4.as_deref(), Some("/replacement/checkout"));

        let parsed = parse_resume_args(&[
            "--cwd=/replacement/checkout".to_string(),
            "--cross-project".to_string(),
            "full-session-id".to_string(),
        ])
        .unwrap();
        assert!(parsed.3);
        assert_eq!(parsed.4.as_deref(), Some("/replacement/checkout"));

        assert_eq!(
            parse_resume_args(&["full-session-id".to_string(), "--cwd".to_string()]),
            Err(2)
        );
    }

    #[test]
    fn resume_parses_account_and_reaches_name_resolution() {
        // Regression for the exit-2 trap: the wake seam appends --account, so
        // `resume <name> --account <id>` must PARSE. A miss at parse printed
        // `unknown resume flag: --account` before the name ever resolved,
        // which broke the wake path three runtime receipts advertise.
        let (name, _, _, _, _, account) = parse_resume_args(&[
            "zzz-nonexistent-probe".to_string(),
            "--account".to_string(),
            "probeacct".to_string(),
        ])
        .expect("resume with --account parses");
        assert_eq!(name, "zzz-nonexistent-probe");
        assert_eq!(account.as_deref(), Some("probeacct"));
        // The control: no account named still parses (the seam only appends
        // when the caller named none, so both shapes arrive here).
        let (name, _, _, _, _, account) =
            parse_resume_args(&["zzz-nonexistent-probe".to_string()]).expect("bare resume parses");
        assert_eq!(name, "zzz-nonexistent-probe");
        assert_eq!(account, None);
        // Equals form rides the same expansion every sibling flag uses.
        let (_, _, _, _, _, account) = parse_resume_args(&[
            "zzz-nonexistent-probe".to_string(),
            "--account=probeacct".to_string(),
        ])
        .expect("resume --account= parses");
        assert_eq!(account.as_deref(), Some("probeacct"));
        // A value-less --account is a usage error, not an unknown flag.
        assert_eq!(
            parse_resume_args(&["zzz-nonexistent-probe".to_string(), "--account".to_string()]),
            Err(2)
        );
    }

    #[test]
    fn resume_message_and_account_parse_together() {
        // The arms now differ deliberately: ask carries a message and re-execs
        // Python where the overlay resolves; resume binds the row's recorded
        // account. Both accept the flag at parse.
        let (_, _, message, _, _, account) = parse_resume_args(&[
            "zzz-nonexistent-probe".to_string(),
            "--message".to_string(),
            "hi".to_string(),
            "--account".to_string(),
            "probeacct".to_string(),
        ])
        .expect("resume with message + account parses");
        assert_eq!(message.as_deref(), Some("hi"));
        assert_eq!(account.as_deref(), Some("probeacct"));
    }

    #[test]
    fn resume_args_accept_message_flag_long_and_short() {
        // code-review finding: --message/-m must not die with "unknown resume
        // flag" -- resume auto-routes to this binary by default, so this
        // parser is the only door the claude wake's --message option has.
        let (name, print_command, message, cross_project, cwd, _) = parse_resume_args(&[
            "alpha".to_string(),
            "--message".to_string(),
            "continue please".to_string(),
        ])
        .unwrap();
        assert_eq!(name, "alpha");
        assert!(!print_command);
        assert_eq!(message.as_deref(), Some("continue please"));
        assert!(!cross_project);
        assert_eq!(cwd, None);

        let (name, _, message, cross_project, cwd, _) =
            parse_resume_args(&["-m".to_string(), "hi".to_string(), "beta".to_string()]).unwrap();
        assert_eq!(name, "beta");
        assert_eq!(message.as_deref(), Some("hi"));
        assert!(!cross_project);
        assert_eq!(cwd, None);

        // No --message given: still parses, message is None (unchanged
        // pre-fix behavior for every other flag combination).
        let (name, print_command, message, cross_project, cwd, _) =
            parse_resume_args(&["gamma".to_string(), "--print-command".to_string()]).unwrap();
        assert_eq!(name, "gamma");
        assert!(print_command);
        assert_eq!(message, None);
        assert!(!cross_project);
        assert_eq!(cwd, None);
    }

    #[test]
    fn resume_args_message_flag_needs_a_value() {
        assert_eq!(
            parse_resume_args(&["alpha".to_string(), "--message".to_string()]),
            Err(2)
        );
    }

    #[test]
    fn resume_args_still_rejects_unknown_flags() {
        assert_eq!(
            parse_resume_args(&["alpha".to_string(), "--bogus".to_string()]),
            Err(2)
        );
    }
}
