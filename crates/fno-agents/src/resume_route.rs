use crate::harness_capabilities::HarnessContract;
use serde_json::Value;
use std::path::Path;

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

pub(crate) fn print_resume_command(
    name: &str,
    entry: &Value,
    harness: &str,
    form_session_id: &str,
    cwd_override: Option<&str>,
    home: &crate::paths::AgentsHome,
    contract: &HarnessContract,
    route: &ResumeRoute,
) -> i32 {
    let form_session_id = if harness == "claude" {
        // Claude's short id is a transport key; its resume command needs the UUID.
        entry
            .get("claude_session_uuid")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .or_else(|| {
                entry
                    .get("harness_session_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
            })
            .unwrap_or(form_session_id)
    } else {
        form_session_id
    };
    if form_session_id.is_empty() {
        eprintln!(
            "fno agents resume: agent {} has no recorded session_id for harness {}.",
            crate::client_verbs::py_repr_str(name),
            crate::client_verbs::py_repr_str(harness)
        );
        return 13;
    }
    let row_name = entry
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(name);
    let client_resume = contract
        .conversion(harness)
        .map(|conversion| conversion.strategy.as_str() == "client-resume")
        .unwrap_or(false);
    let recorded_cwd = entry.get("cwd").and_then(Value::as_str).unwrap_or("");
    let cwd = cwd_override.map(str::to_string).unwrap_or_else(|| {
        if client_resume {
            let claude_uuid = entry
                .get("claude_session_uuid")
                .and_then(Value::as_str)
                .unwrap_or("");
            crate::reentry::resolve_resume_cwd(
                &crate::claude_ask::ClaudeHome::from_env(),
                recorded_cwd,
                claude_uuid,
            )
            .to_string_lossy()
            .into_owned()
        } else {
            recorded_cwd.to_string()
        }
    });
    let argv = if harness == "codex" {
        crate::pane_relaunch::build_resume_argv_tokens_split(
            harness,
            form_session_id,
            Some(&cwd),
            true,
        )
    } else {
        contract
            .render_session_argv_raw(harness, "interactive_resume", Some(form_session_id))
            .ok()
    };
    let Some(mut argv) = argv else {
        if let ResumeRoute::Refused(line) = route {
            eprintln!("{line}");
        } else {
            eprintln!(
                "fno agents resume: harness {} resume contract is invalid.",
                crate::client_verbs::py_repr_str(harness)
            );
        }
        return 13;
    };
    let codex_route_outcome =
        crate::codex_route::resume_route(harness, entry, Path::new(&cwd), &mut argv);
    if let Some(code) = crate::codex_route::resume_verdict(&codex_route_outcome, entry, row_name) {
        return code;
    }
    let mut print_env = match &codex_route_outcome {
        Some(Ok(Some(route))) => route.env_masked(),
        _ => Vec::new(),
    };
    if client_resume {
        match crate::reentry::resolve_reentry(
            &home.registry_json(),
            row_name,
            crate::reentry::ReentryTransition::Resume,
            None,
            Some(&cwd),
        ) {
            Ok(plan) => {
                let plan = plan.carry_pins(&mut argv);
                if let Some(path) = plan.route_settings_path.as_deref() {
                    if !path.is_empty() && !argv.iter().any(|token| token == "--settings") {
                        argv.splice(1..1, ["--settings".to_string(), path.to_string()]);
                    }
                }
                print_env.extend(
                    plan.env
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone())),
                );
            }
            Err(reason) => {
                let reason = reason.lines().next().unwrap_or("re-entry plan unavailable");
                eprintln!(
                    "fno agents resume: {reason}; printing the declared contract command only."
                );
            }
        }
    }
    let identity = match crate::pane_relaunch::mesh_identity_assignments(
        entry.get("name").and_then(Value::as_str).unwrap_or(name),
        harness,
        entry.get("node").and_then(Value::as_str),
    ) {
        Ok(identity) => identity,
        Err(reason) => {
            eprintln!("fno agents resume: {reason}; refusing an unattributable resume");
            return 13;
        }
    };
    let mut printable = vec!["env".to_string()];
    printable.extend(identity);
    printable.extend(crate::pane_relaunch::env_prefixed(&print_env, &argv));
    crate::pane_relaunch::print_relaunch_command(None, &cwd, &printable, &[], row_name);
    0
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
