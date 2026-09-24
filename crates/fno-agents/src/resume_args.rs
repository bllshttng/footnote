//! The `fno agents resume` argv parser.
//!
//! LIVES OUTSIDE client_verbs for the same reason usage.rs does: the file
//! is over the 5,000-line budget and shrink-only, and this parser is what
//! every wake touches. The row's recorded launch account stays the binding
//! authority; `--account` parses so the seam's appended flag never exits 2
//! at argv (run_resume validates the value it carries).

use crate::client_verbs::{echo_extra, expand_eq};

/// One resume invocation's parsed argv. A struct rather than a tuple since
/// the substrate-conversion flags landed: nine positional elements is a
/// shape nobody reads correctly twice.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResumeArgs {
    pub name: String,
    pub print_command: bool,
    pub message: Option<String>,
    /// Internal fallback marker: the caller already accepted this message in
    /// the durable mail queue, so a Working route must not enqueue it again.
    pub message_already_queued: bool,
    pub cross_project: bool,
    pub cwd: Option<String>,
    pub account: Option<String>,
    /// `--substrate thread`: convert this live pane into a persistent
    /// thread instead of re-entering it. `None` is an ordinary resume.
    /// `thread` is the only accepted value, because thread-to-pane is not
    /// built and a pane-to-pane resume is what the bare verb already does.
    pub substrate: Option<String>,
    /// Print the conversion plan and change nothing.
    pub dry_run: bool,
    /// Accept a converted session whose harness minted a NEW id, recording
    /// the old one as the related id. Refused on a crowned row: moving a
    /// crown to a new id is succession, a separate operation.
    pub allow_new_id: bool,
}

/// Parse the argv and validate what argv alone can settle. The replacement
/// cwd is checked HERE rather than in `run_resume` for the reason this
/// module exists: client_verbs is over the line budget and shrink-only, and
/// a check on a parsed field belongs beside the parser that produced it.
pub fn parse_and_validate(rest: &[String]) -> Result<ResumeArgs, i32> {
    let parsed = parse_resume_args(rest)?;
    if let Some(path) = parsed.cwd.as_deref() {
        if !std::path::Path::new(path).is_dir() {
            eprintln!(
                "fno agents resume: replacement cwd {} is not an existing directory.",
                crate::client_verbs::py_repr_str(path)
            );
            return Err(13);
        }
    }
    Ok(parsed)
}

/// Expanded from client_verbs.rs where it lived beside `run_resume`; the
/// move is mechanical, the shape unchanged.
pub fn parse_resume_args(rest: &[String]) -> Result<ResumeArgs, i32> {
    let mut parsed = ResumeArgs::default();
    let mut name: Option<String> = None;
    // Every sibling parser in this file (`parse_trace_args`, `parse_logs_args`)
    // expands `--flag=value` into `--flag value` before iterating; without it
    // `--message=continue` falls into the `starts_with("--")` unknown-flag arm
    // instead of being recognized.
    let rest = expand_eq(rest);
    let mut iter = rest.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--print-command" => parsed.print_command = true,
            "--cross-project" => parsed.cross_project = true,
            "--dry-run" => parsed.dry_run = true,
            "--allow-new-id" => parsed.allow_new_id = true,
            "--message-already-queued" => parsed.message_already_queued = true,
            "--message" | "-m" => {
                parsed.message = Some(match iter.next() {
                    Some(v) => v.clone(),
                    None => {
                        eprintln!("fno-agents: {a} needs a value");
                        return Err(2);
                    }
                });
            }
            "--cwd" => {
                parsed.cwd = Some(match iter.next() {
                    Some(v) if !v.starts_with("--") => v.clone(),
                    _ => {
                        eprintln!("fno-agents: --cwd needs a value");
                        return Err(2);
                    }
                });
            }
            "--substrate" => {
                // The lifecycle move, not a launch selector: `resume
                // --substrate thread` converts a live pane into a persistent
                // thread under its own session id. `thread` is the only
                // member - thread-to-pane is unbuilt, and pane-to-pane is
                // what the bare verb already does - so any other value
                // refuses by name rather than resuming something the caller
                // did not ask for.
                let value = match iter.next() {
                    Some(v) if !v.starts_with("--") => v.clone(),
                    _ => {
                        eprintln!("fno-agents: --substrate needs a value (thread)");
                        return Err(2);
                    }
                };
                if value != "thread" {
                    eprintln!(
                        "fno-agents: resume --substrate takes only 'thread' (got {}); \
                         thread-to-pane conversion is not built, and a pane-to-pane \
                         resume is the bare verb",
                        echo_extra(&value)
                    );
                    return Err(2);
                }
                parsed.substrate = Some(value);
            }
            "--account" => {
                // the spawn seam's account picker rides the shared
                // worker-dir seam, so a wake arrives with `--account` appended.
                // The spawn arm parses the flag; this arm refused it at parse,
                // which exited 2 before the name ever resolved and broke the
                // wake ladder three receipts advertise. Parse it here.
                parsed.account = Some(match iter.next() {
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
    if parsed.message_already_queued && parsed.message.is_none() {
        eprintln!("fno-agents: --message-already-queued needs --message");
        return Err(2);
    }
    // The conversion-only flags refuse on a plain resume rather than being
    // dropped: a silently ignored `--allow-new-id` is how a caller learns its
    // authorization was never carried, one forked session too late.
    if parsed.substrate.is_none() {
        for (present, flag) in [
            (parsed.dry_run, "--dry-run"),
            (parsed.allow_new_id, "--allow-new-id"),
        ] {
            if present {
                eprintln!(
                    "fno-agents: resume {flag} applies to a conversion only; \
                     pass --substrate thread, or drop the flag"
                );
                return Err(2);
            }
        }
    }
    match name {
        Some(n) => {
            parsed.name = n;
            Ok(parsed)
        }
        None => {
            eprintln!("fno-agents: resume needs a <name>");
            Err(2)
        }
    }
}

/// Does this argv ask for the pane-to-thread conversion, rather than a
/// re-entry? The FLAG in either spelling, never a value that merely starts
/// with the same letters: `-m "--substrate thread"` is a message, and
/// routing it to `agent.convert` would move a live pane the caller never
/// named.
///
/// This is only the ROUTING question. Whether the substrate is a legal one
/// is [`parse_resume_args`]'s answer, and the convert door asserts it
/// before it builds a request.
pub fn requests_conversion(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--substrate" || arg.starts_with("--substrate="))
}

/// Parse an argv that [`requests_conversion`] routed to the convert door,
/// and refuse it unless the parse really produced `--substrate thread`.
///
/// The assertion is the point. The routing question is asked on the argv,
/// and the answer to "is this a conversion" must come from the PARSER, so a
/// session whose caller asked for a plain resume can never be converted by
/// a door that guessed.
pub fn parse_conversion_args(rest: &[String]) -> Result<ResumeArgs, String> {
    let parsed = parse_resume_args(rest).map_err(|_| {
        "resume --substrate thread takes one NAME, plus --dry-run and --allow-new-id".to_string()
    })?;
    if parsed.substrate.as_deref() != Some("thread") {
        return Err(
            "resume reached the convert door without --substrate thread; this is a \
                    re-entry, not a lifecycle conversion"
                .to_string(),
        );
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_convert_door_refuses_a_parse_that_named_no_thread_substrate() {
        let parsed = parse_conversion_args(&args(&["worker", "--substrate", "thread"]))
            .expect("the flag and the parse agree");
        assert_eq!(parsed.name, "worker");

        // The routing question can be answered yes by an argv the parser
        // then rejects; the door must not trust the router.
        let error = parse_conversion_args(&args(&["worker"])).expect_err("no substrate, no door");
        assert!(error.contains("--substrate thread"), "{error}");
    }

    #[test]
    fn the_convert_door_opens_on_the_flag_and_not_on_a_value_that_looks_like_it() {
        assert!(requests_conversion(&args(&[
            "worker",
            "--substrate",
            "thread"
        ])));
        assert!(requests_conversion(&args(&[
            "worker",
            "--substrate=thread"
        ])));
        // A message that merely contains the flag's spelling is a message.
        assert!(!requests_conversion(&args(&[
            "worker",
            "-m",
            "--substrate thread"
        ])));
        assert!(!requests_conversion(&args(&["worker", "--substrate-ish"])));
        assert!(!requests_conversion(&args(&["worker"])));
    }

    fn args(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| (*t).to_string()).collect()
    }

    #[test]
    fn resume_args_accept_cross_project_and_replacement_cwd_forms() {
        let parsed = parse_resume_args(&args(&[
            "full-session-id",
            "--cross-project",
            "--cwd",
            "/replacement/checkout",
        ]))
        .unwrap();
        assert_eq!(parsed.name, "full-session-id");
        assert!(parsed.cross_project);
        assert_eq!(parsed.cwd.as_deref(), Some("/replacement/checkout"));

        let parsed = parse_resume_args(&args(&[
            "--cwd=/replacement/checkout",
            "--cross-project",
            "full-session-id",
        ]))
        .unwrap();
        assert!(parsed.cross_project);
        assert_eq!(parsed.cwd.as_deref(), Some("/replacement/checkout"));

        assert_eq!(
            parse_resume_args(&args(&["full-session-id", "--cwd"])),
            Err(2)
        );
    }

    #[test]
    fn resume_parses_account_and_reaches_name_resolution() {
        // Regression for the exit-2 trap: the wake seam appends --account, so
        // `resume <name> --account <id>` must PARSE. A miss at parse printed
        // `unknown resume flag: --account` before the name ever resolved,
        // which broke the wake path three runtime receipts advertise.
        let parsed = parse_resume_args(&args(&["zzz-nonexistent-probe", "--account", "probeacct"]))
            .expect("resume with --account parses");
        assert_eq!(parsed.name, "zzz-nonexistent-probe");
        assert_eq!(parsed.account.as_deref(), Some("probeacct"));
        // The control: no account named still parses (the seam only appends
        // when the caller named none, so both shapes arrive here).
        let parsed =
            parse_resume_args(&args(&["zzz-nonexistent-probe"])).expect("bare resume parses");
        assert_eq!(parsed.name, "zzz-nonexistent-probe");
        assert_eq!(parsed.account, None);
        // Equals form rides the same expansion every sibling flag uses.
        let parsed = parse_resume_args(&args(&["zzz-nonexistent-probe", "--account=probeacct"]))
            .expect("resume --account= parses");
        assert_eq!(parsed.account.as_deref(), Some("probeacct"));
        // A value-less --account is a usage error, not an unknown flag.
        assert_eq!(
            parse_resume_args(&args(&["zzz-nonexistent-probe", "--account"])),
            Err(2)
        );
    }

    #[test]
    fn resume_message_and_account_parse_together() {
        // The arms now differ deliberately: ask carries a message and re-execs
        // Python where the overlay resolves; resume binds the row's recorded
        // account. Both accept the flag at parse.
        let parsed = parse_resume_args(&args(&[
            "zzz-nonexistent-probe",
            "--message",
            "hi",
            "--account",
            "probeacct",
        ]))
        .expect("resume with message + account parses");
        assert_eq!(parsed.message.as_deref(), Some("hi"));
        assert_eq!(parsed.account.as_deref(), Some("probeacct"));
    }

    #[test]
    fn resume_args_accept_message_flag_long_and_short() {
        // code-review finding: --message/-m must not die with "unknown resume
        // flag" -- resume auto-routes to this binary by default, so this
        // parser is the only door the claude wake's --message option has.
        let parsed = parse_resume_args(&args(&["alpha", "--message", "continue please"])).unwrap();
        assert_eq!(parsed.name, "alpha");
        assert!(!parsed.print_command);
        assert_eq!(parsed.message.as_deref(), Some("continue please"));
        assert!(!parsed.cross_project);
        assert_eq!(parsed.cwd, None);

        let parsed = parse_resume_args(&args(&["-m", "hi", "beta"])).unwrap();
        assert_eq!(parsed.name, "beta");
        assert_eq!(parsed.message.as_deref(), Some("hi"));
        assert!(!parsed.cross_project);
        assert_eq!(parsed.cwd, None);

        // No --message given: still parses, message is None (unchanged
        // pre-fix behavior for every other flag combination).
        let parsed = parse_resume_args(&args(&["gamma", "--print-command"])).unwrap();
        assert_eq!(parsed.name, "gamma");
        assert!(parsed.print_command);
        assert_eq!(parsed.message, None);
        assert!(!parsed.cross_project);
        assert_eq!(parsed.cwd, None);
    }

    #[test]
    fn resume_args_message_flag_needs_a_value() {
        assert_eq!(parse_resume_args(&args(&["alpha", "--message"])), Err(2));
    }

    #[test]
    fn resume_args_still_rejects_unknown_flags() {
        assert_eq!(parse_resume_args(&args(&["alpha", "--bogus"])), Err(2));
    }

    #[test]
    fn substrate_thread_parses_with_the_conversion_flags() {
        let parsed = parse_resume_args(&args(&[
            "king-delivery",
            "--substrate",
            "thread",
            "--dry-run",
            "--allow-new-id",
        ]))
        .expect("conversion flags parse");
        assert_eq!(parsed.name, "king-delivery");
        assert_eq!(parsed.substrate.as_deref(), Some("thread"));
        assert!(parsed.dry_run);
        assert!(parsed.allow_new_id);

        // The equals form rides the same expansion, and the flags are
        // independent: a bare conversion sets neither boolean.
        let parsed = parse_resume_args(&args(&["king-delivery", "--substrate=thread"]))
            .expect("equals form parses");
        assert_eq!(parsed.substrate.as_deref(), Some("thread"));
        assert!(!parsed.dry_run);
        assert!(!parsed.allow_new_id);

        // A plain resume declares no substrate at all, so the conversion
        // path is never entered by default.
        let parsed = parse_resume_args(&args(&["king-delivery"])).unwrap();
        assert_eq!(parsed.substrate, None);
    }

    #[test]
    fn substrate_takes_only_thread_and_needs_a_value() {
        // pane and headless are SPAWN substrates. Accepting one here would
        // read as a launch selector and resume something nobody asked for.
        for bad in ["pane", "headless", "bg"] {
            assert_eq!(
                parse_resume_args(&args(&["alpha", "--substrate", bad])),
                Err(2),
                "--substrate {bad} must refuse"
            );
        }
        assert_eq!(parse_resume_args(&args(&["alpha", "--substrate"])), Err(2));
        assert_eq!(
            parse_resume_args(&args(&["alpha", "--substrate", "--dry-run"])),
            Err(2)
        );
    }

    #[test]
    fn the_conversion_only_flags_refuse_on_a_plain_resume() {
        // Dropping them silently is how a caller discovers its --allow-new-id
        // was never carried, one forked session too late.
        assert_eq!(parse_resume_args(&args(&["alpha", "--dry-run"])), Err(2));
        assert_eq!(
            parse_resume_args(&args(&["alpha", "--allow-new-id"])),
            Err(2)
        );
        // The control: both are legal beside --substrate thread.
        assert!(parse_resume_args(&args(&["alpha", "--substrate", "thread", "--dry-run"])).is_ok());
    }

    #[test]
    fn resume_accepts_the_internal_already_queued_message_marker() {
        let parsed = parse_resume_args(&args(&[
            "alpha",
            "--message-already-queued",
            "--message",
            "go",
        ]))
        .expect("the internal fallback marker must parse");
        assert_eq!(parsed.message.as_deref(), Some("go"));
        assert!(format!("{parsed:?}").contains("message_already_queued: true"));
        assert_eq!(
            parse_resume_args(&args(&["alpha", "--message-already-queued"])),
            Err(2)
        );
    }
}
