use crate::harness_capabilities::HarnessContract;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResumeRoute {
    ClientResume,
    ServerResume,
    TerminalExec,
    Refused(String),
}

pub(crate) fn resume_route(
    harness: &str,
    substrate: Option<&str>,
    has_mux_ref: bool,
    caller_has_terminal: bool,
    name: &str,
    session_id: &str,
    contract: &HarnessContract,
) -> ResumeRoute {
    let conversion = match contract.conversion(harness) {
        Ok(conversion) => conversion,
        Err(_) => {
            return ResumeRoute::Refused(format!(
                "fno agents resume: harness {} resume not supported by this fno version.",
                crate::client_verbs::py_repr_str(harness)
            ));
        }
    };

    if substrate == Some("thread") {
        return match conversion.strategy.as_str() {
            "client-resume" => ResumeRoute::ClientResume,
            "server-resume" => ResumeRoute::ServerResume,
            "unsupported" => {
                ResumeRoute::Refused(format!("fno agents resume: {name}: {}", conversion.refusal))
            }
            "keeper-rebind" => {
                let form = contract
                    .render_session_argv_raw(harness, "interactive_resume", Some(session_id))
                    .map(|argv| argv.join(" "))
                    .unwrap_or_else(|_| format!("{harness} interactive_resume"));
                ResumeRoute::Refused(format!(
                    "fno agents resume: {name}: the {harness} keeper lane has no revival for an exited thread yet; from a terminal, fno agents resume {name} runs {form}"
                ))
            }
            strategy => ResumeRoute::Refused(format!(
                "fno agents resume: {name}: unknown {harness} resume strategy {strategy:?}"
            )),
        };
    }

    if caller_has_terminal {
        ResumeRoute::TerminalExec
    } else if has_mux_ref {
        ResumeRoute::Refused(format!(
            "fno agents resume: {name}: this exited row still records a mux pane; run fno agents resume {name} from a terminal"
        ))
    } else {
        ResumeRoute::Refused(format!(
            "fno agents resume: {name}: the {harness} resume form is a terminal program and this caller has no terminal; run fno agents resume {name} from a terminal"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract() -> HarnessContract {
        HarnessContract::packaged().expect("packaged harness contract")
    }

    #[test]
    fn routes_by_conversion_strategy_and_terminal_fact() {
        let contract = contract();
        assert_eq!(
            resume_route(
                "claude",
                Some("thread"),
                false,
                false,
                "c",
                "c-session",
                &contract
            ),
            ResumeRoute::ClientResume
        );
        assert_eq!(
            resume_route(
                "codex",
                Some("thread"),
                false,
                false,
                "c",
                "c-session",
                &contract
            ),
            ResumeRoute::ServerResume
        );
        assert_eq!(
            resume_route(
                "opencode",
                Some("pane"),
                false,
                true,
                "o",
                "o-session",
                &contract
            ),
            ResumeRoute::TerminalExec
        );
    }

    #[test]
    fn refuses_unsupported_and_keeper_thread_routes_with_contract_details() {
        let contract = contract();
        let refusal = contract.conversion("opencode").unwrap().refusal;
        assert_eq!(
            resume_route(
                "opencode",
                Some("thread"),
                false,
                false,
                "o",
                "o-session",
                &contract
            ),
            ResumeRoute::Refused(format!("fno agents resume: o: {refusal}"))
        );

        let ResumeRoute::Refused(keeper_refusal) = resume_route(
            "agy",
            Some("thread"),
            false,
            false,
            "a",
            "actual-cv-session",
            &contract,
        ) else {
            panic!("keeper thread must refuse");
        };
        assert!(keeper_refusal.contains("the agy keeper lane has no revival"));
        for token in contract
            .render_session_argv_raw("agy", "interactive_resume", Some("actual-cv-session"))
            .unwrap()
        {
            assert!(keeper_refusal.contains(&token));
        }
    }

    #[test]
    fn mux_pane_rows_refuse_headless_relaunch_but_allow_terminal_exec() {
        let contract = contract();
        let ResumeRoute::Refused(line) =
            resume_route("pi", Some("pane"), true, false, "p", "p-session", &contract)
        else {
            panic!("headless mux pane must refuse");
        };
        assert_eq!(
            line,
            "fno agents resume: p: this exited row still records a mux pane; run fno agents resume p from a terminal"
        );
        assert_eq!(
            resume_route("pi", Some("pane"), true, true, "p", "p-session", &contract),
            ResumeRoute::TerminalExec
        );
    }

    #[test]
    fn missing_nonthread_terminal_and_unknown_harness_refuse_with_actionable_lines() {
        let contract = contract();
        assert_eq!(
            resume_route(
                "opencode",
                None,
                false,
                false,
                "o",
                "o-session",
                &contract
            ),
            ResumeRoute::Refused("fno agents resume: o: the opencode resume form is a terminal program and this caller has no terminal; run fno agents resume o from a terminal".to_string())
        );
        assert_eq!(
            resume_route("unknown", None, false, false, "u", "u-session", &contract),
            ResumeRoute::Refused(
                "fno agents resume: harness 'unknown' resume not supported by this fno version."
                    .to_string()
            )
        );
    }
}
