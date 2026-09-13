//! How a resume argv is built and delivered (x-7b5e, x-eb79): the declared
//! `interactive_resume` form reader, the fail-open render the mux gesture
//! falls back to, the OFF-LOOP resolution that carries the codex
//! writable-roots grant, and the replay vocabulary the gesture re-enters
//! through. Extracted from server.rs (over the file budget, shrink-only):
//! the code this module owns moved here with the x-eb79 change that touched
//! it, answering one question - what argv resumes a session, and how does it
//! reach the pane spawn without blocking the core loop.

#[cfg(test)]
thread_local! {
    /// Test override for the resume program (see [`resume_argv_for`]): points
    /// unit tests at a benign binary so a resume spawn runs without launching
    /// a real claude/codex, mirroring `ATTACH_PROGRAM` for the attach path.
    static RESUME_PROGRAM: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
    /// Test override for the declared resume forms (see
    /// [`declared_resume_form`]). `None` (the default) reads the same bundled
    /// contract the production reader parses, so tests and prod walk one code
    /// path and an override is only needed for the negative arms.
    static DECLARED_RESUME_FORMS:
        std::cell::RefCell<Option<std::collections::HashMap<String, Option<DeclaredResumeForm>>>> =
        const { std::cell::RefCell::new(None) };
}

/// (x-7b5e) One harness's declared `interactive_resume` form: the tokens the
/// capability table carries, with the `{session_id}` placeholder intact.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct DeclaredResumeForm {
    tokens: Vec<String>,
}

/// (x-7b5e) The `interactive_resume` form `harness` declares, or `None` when
/// it declares none. A thin view over [`crate::agents_view::resume_form`] -
/// the ONE reader of the declared capability table, shared with the attach
/// lane by a form-kind parameter as its doc comment promised - so a seventh
/// harness, and an operator override (`[harness.<name>.resume]` in config),
/// needs no Rust change here. An unknown harness, an `unsupported` form, and
/// a malformed one all read as "cannot resume", the safe direction.
pub(super) fn declared_resume_form(harness: &str) -> Option<DeclaredResumeForm> {
    #[cfg(test)]
    if let Some(map) = DECLARED_RESUME_FORMS.with(|p| p.borrow().clone()) {
        return map.get(harness).cloned().flatten();
    }
    crate::agents_view::resume_form(harness).map(|form| DeclaredResumeForm {
        tokens: form.tokens,
    })
}

/// (x-7b5e) Test override pinning one harness's declared resume availability
/// directly, so the negative arms (a harness the table gives no form) stay
/// assertable after the table itself declares a form for every harness.
#[cfg(test)]
pub(super) fn set_declared_resume_form(harness: &str, form: Option<Vec<String>>) {
    DECLARED_RESUME_FORMS.with(|p| {
        p.borrow_mut()
            .get_or_insert_with(std::collections::HashMap::new)
            .insert(
                harness.to_string(),
                form.map(|tokens| DeclaredResumeForm { tokens }),
            );
    });
}

#[cfg(test)]
pub(super) fn set_resume_program(argv: &[&str]) {
    RESUME_PROGRAM.with(|p| *p.borrow_mut() = Some(argv.iter().map(|s| s.to_string()).collect()));
}

#[cfg(test)]
pub(super) fn clear_resume_program() {
    RESUME_PROGRAM.with(|p| *p.borrow_mut() = None);
}

#[cfg(test)]
pub(super) struct DeclaredResumeFormsGuard;

#[cfg(test)]
impl Drop for DeclaredResumeFormsGuard {
    fn drop(&mut self) {
        DECLARED_RESUME_FORMS.with(|p| *p.borrow_mut() = None);
    }
}

/// Test-only guard clearing the [`RESUME_PROGRAM`] override on scope exit, so
/// a test that installs the override cannot leak it into a later test on the
/// same thread (`cargo test -- --test-threads=1`).
#[cfg(test)]
pub(super) struct ResumeProgramGuard;

#[cfg(test)]
impl Drop for ResumeProgramGuard {
    fn drop(&mut self) {
        clear_resume_program();
    }
}

/// (x-d401) The session id a pane-run argv resumes: the token after
/// The session id a pane argv is resuming, derived from the SAME declared
/// resume form the resume spawn builds: the argv's command names a harness
/// the capability table gives a form, the argv carries that form's literal
/// tokens in order (extra flags tolerated between them), and the token in
/// the `{session_id}` slot is the target. Anchored past the `env(1)`
/// wrapper, so an argument that merely mentions the token (`grep --resume
/// file`) never parses as a resume target, and a harness the table gives no
/// form never parses at all. `None` for a shell pane or a run with no
/// resume form. The row-to-pane join: `row_resume_disposition_in_session`
/// reads it so a pane visibly running a session makes `BackendNotLive`
/// unreachable for that session's row - for EVERY declared harness, not
/// just the two this function once hardcoded.
pub(super) fn resume_target_from_argv(argv: &[String]) -> Option<String> {
    let start = super::env_assignments_start(argv).unwrap_or(0);
    let rest = &argv[start..];
    // The command is the first non-assignment token (same scan as
    // `cmd_from_argv`); only a harness whose declared form parses.
    let cmd_idx = rest.iter().position(|a| !a.contains('='))?;
    let base = rest[cmd_idx].rsplit('/').next().unwrap_or(&rest[cmd_idx]);
    let form = declared_resume_form(base)?;
    let placeholder = form.tokens.iter().position(|t| t == "{session_id}")?;
    if placeholder < 2 {
        // Without a literal anchor between the command and the id slot
        // (`foo --resume <id>`-shaped), any first argument would read as a
        // session id. Refusing to guess is the safe direction.
        return None;
    }
    // Walk the form's pre-placeholder literals against the argv in order,
    // tolerating extra flags between them. tokens[0] is the harness command
    // itself, already matched by `base`; when the LAST literal lands on an
    // argv token, the next argv token sits in the placeholder slot.
    let mut arg = cmd_idx + 1;
    for literal in &form.tokens[1..placeholder] {
        loop {
            let candidate = rest.get(arg)?;
            if candidate == literal {
                break;
            }
            arg += 1;
        }
        arg += 1;
    }
    rest.get(arg)
        // A FLAG is not a session id. `codex resume --last` resumes the most
        // recent session without naming it, so the token in the placeholder
        // slot is `--last` and storing it yields a join key matching no row.
        // The junk value is harmless; the MISS is not. `pane_resumes_session`
        // is what keeps a row non-resumable while a pane runs that session,
        // so a pane started this way leaves its row still offered as
        // resumable, and one tap opens a SECOND WRITER on a live rollout.
        // That is not theoretical: it happened in this branch's own review
        // round.
        .filter(|sid| !sid.is_empty() && !sid.contains('=') && !sid.starts_with('-'))
        .cloned()
}

/// The argv resuming `session_id` through its harness's own form (x-5f7f).
/// The session id is always a positional arg (never a shell string), and it
/// arrives from a registry row the catalog gate matched, so it can only name
/// a session. Tests override the program via [`set_resume_program`],
/// mirroring `set_attach_program`.
///
/// This is the third INTERACTIVE codex resume argv builder, beside the Python
/// `_build_resume_argv` and its Rust twin in fno-agents. A fix applied to two
/// of three reads as done, so a change to codex's resume argv belongs here too.
///
/// The word interactive is load-bearing: two MORE builders render the headless
/// `codex exec resume` form, `harnesses/codex.py`'s `resume` and
/// `codex_ask.rs`'s `build_argv_resume`. Five in all. Those two take neither
/// `--cd` nor `--add-dir` (that subcommand accepts neither) and pin the
/// directory through the subprocess cwd instead, so they are a separate
/// question, not more copies of this one. A reader trusting a bare count of
/// three would skip them.
///
/// For codex this renders the BARE declared form, no grant and no `--cd`.
/// The gesture does not run it for codex except as the fail-open fallback:
/// x-eb79 routes the gesture's argv through `fno-agents resume-argv` (the
/// builder the CLI verb lane uses) via [`Core::resolve_resume_argv`], and
/// this render is what survives when that shell-out fails - flagged so a
/// pane notice names the grant loss. `--cd` itself is refused on this lane
/// even in the fallback, and the reason is narrower than it first looks.
///
/// This lane already spawns the pane AT the row's directory, via
/// `spawn_pane_cmd(&argv, .., &spawn_cwd)`. Codex raises its directory
/// prompt only when the process cwd differs from the session's saved
/// directory. So for a worker whose saved directory is the tree it is being
/// restored into, the two agree, no prompt appears, and `--cd` would be a
/// no-op naming the path codex already picked.
///
/// The prompt DOES appear on the other branch, where `restore_member_cwd`
/// fell back because the row's directory is gone. There `--cd` is exactly
/// the wrong answer: it pins the fallback, the squad canonical cwd or
/// `$HOME`, when codex left alone still offers the recorded session
/// directory, which usually survives and a human can take. `$HOME` is also
/// not a trusted codex project, so pinning it can raise the folder-trust
/// screen instead, an unattended hang of the kind `--cd` exists to remove.
///
/// A SEPARATE, PRE-EXISTING problem lives here and is not caused by any of
/// the above. A restored bounded worker in a linked worktree is already
/// rooted where `.git` is a FILE pointing at `<repo>/.git/worktrees/<name>`,
/// outside the writable workspace, so its next commit already fails. The
/// other two builders splice a `-c sandbox_workspace_write.writable_roots=`
/// grant that fixes this. This lane's fallback has none - which is exactly
/// why the failure must never be silent.
///
/// The grant does not travel here. `codex_writable_config_args` shells
/// `fno do plan path`, folds in the state dirs, and carries the invariant
/// that omitting the state root leaves a resumed worker unable to write its
/// claim lockfile. Copying it is a fourth divergent implementation of subtle
/// logic. Depending on fno-agents inverts a boundary its own Cargo.toml
/// records: fno never links it, it shells the binary at runtime. The open
/// candidate is TAKEN as of x-7b5e/x-eb79: the tokens come from the declared
/// `interactive_resume` form via `fno-agents resume-argv` (see
/// [`declared_resume_form`]), and this fn only fills the session id.
pub(super) fn resume_argv_for(harness: &str, session_id: &str) -> Result<Vec<String>, String> {
    #[cfg(test)]
    if let Some(mut argv) = RESUME_PROGRAM.with(|p| p.borrow().clone()) {
        argv.push(session_id.to_string());
        return Ok(argv);
    }
    let Some(form) = declared_resume_form(harness) else {
        return Err(crate::restore_liveness::no_resume_form_reason(
            harness, session_id,
        ));
    };
    let mut argv = form.tokens;
    let mut filled = false;
    for token in argv.iter_mut() {
        if token == "{session_id}" {
            *token = session_id.to_string();
            filled = true;
        } else if token.starts_with('{') && token.ends_with('}') {
            return Err(format!(
                "{harness} resume form names {token}; only {{session_id}} can be filled"
            ));
        }
    }
    if !filled {
        return Err(format!("{harness} resume form fills no session id"));
    }
    Ok(argv)
}

/// (x-eb79) What the core loop re-enters once a non-claude row's resume argv
/// lands. Same idempotent-gates contract as `ReentrySpawnRequest`: the
/// replay re-runs the gesture and the staged argv is consumed at argv
/// construction, so the second pass spawns exactly the pane the first pass
/// would have - only the argv construction moved off-loop.
pub(super) enum ResumeReplay {
    /// Re-enter `resume_one` for the row (`Command::ResumeAgent`).
    Gesture { name: String },
    /// Re-enter the held-worker resume behind `Command::FocusPane(pid)`.
    Held { pid: u64 },
}

impl super::Core {
    /// (x-eb79) Resolve one non-claude row's resume argv OFF the core loop
    /// (`fno-agents resume-argv`), mirroring `resolve_reentry`: the codex
    /// grant decision needs a Python-booting shell-out, so the gesture yields
    /// `PlanPending` and replays through [`super::CoreMsg::ResumeArgvReady`].
    /// Fail-open by construction: when the verb fails the task renders the
    /// declared form itself and flags `degraded`, so the handler notices the
    /// operator - a silent fallback is refused (AC3-FALLBACK).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn resolve_resume_argv(
        &self,
        client_id: u64,
        harness: &str,
        session_id: &str,
        grant_cwd: &str,
        pin_cd: bool,
        replay: ResumeReplay,
    ) {
        let core_tx = self.self_tx.clone();
        let harness = harness.to_string();
        let session_id = session_id.to_string();
        let grant_cwd = grant_cwd.to_string();
        tokio::spawn(async move {
            let outcome = match super::agent_actions::run_resume_argv(
                &harness,
                &session_id,
                &grant_cwd,
                pin_cd,
            )
            .await
            {
                Ok(argv) => Ok((argv, false)),
                // Fail-open: the fallback IS today's declared-form render,
                // never a second divergent builder. The degradation is named
                // to the operator, never silent.
                Err(reason) => match resume_argv_for(&harness, &session_id) {
                    Ok(argv) => Ok((argv, true)),
                    Err(_) => Err(reason),
                },
            };
            let _ = core_tx
                .send(super::CoreMsg::ResumeArgvReady {
                    id: client_id,
                    argv: outcome,
                    replay: Box::new(replay),
                })
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{clear_resume_program, resume_argv_for};

    #[test]
    fn resume_argv_matches_the_harness_capability_tokens() {
        // The mirror is checked against the TOML that owns the tokens, not
        // against this crate's own literals (Rust checked against Rust proves
        // nothing). EVERY harness the table declares must render, because a
        // bulk restore built on a partial match silently skips the rest
        // (x-7b5e: the old two-arm match left gemini/agy/opencode/pi with no
        // Resume at all). The headless form is reserved for one-shot and
        // stream-json workers, while `claude attach` is the live-row gesture.
        // No override is installed, so the REAL argv is asserted.
        clear_resume_program();
        let toml_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../cli/src/fno/agents/harness_capabilities.toml");
        let raw = std::fs::read_to_string(&toml_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", toml_path.display()));
        let caps: toml::Value = toml::from_str(&raw).expect("parse harness_capabilities.toml");
        let token = |form: &str| -> Vec<String> {
            let mut node = caps.get("harness").expect("harness table");
            for key in form.split('/') {
                node = node
                    .get(key)
                    .unwrap_or_else(|| panic!("missing {key} in {form}"));
            }
            let arr = node
                .get("tokens")
                .and_then(|t| t.as_array())
                .unwrap_or_else(|| panic!("missing tokens for {form}"));
            arr.iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect()
        };
        let declared: Vec<String> = caps
            .get("harness")
            .and_then(|h| h.as_table())
            .expect("harness table")
            .keys()
            .cloned()
            .collect();
        assert!(
            declared.len() >= 6,
            "the table declares every harness under test: {declared:?}"
        );
        for harness in &declared {
            // x-eb79: codex now resumes through the off-loop `resume-argv`
            // resolution (grant + --cd, asserted on the staging tests in
            // server.rs); its declared form is the FAILOPEN render, not the
            // gesture's.
            if harness == "codex" {
                continue;
            }
            let form = token(&format!(
                "{harness}/resume_strategy/forms/interactive_resume"
            ));
            assert!(
                super::super::Core::resume_form(harness),
                "{harness} is resumable with no Rust change (AC3-HP)"
            );
            let sid = format!("{harness}-0a1b2c3d");
            let expected: Vec<String> = form
                .iter()
                .map(|t| t.replace("{session_id}", &sid))
                .collect();
            assert_eq!(
                resume_argv_for(harness, &sid).unwrap(),
                expected,
                "{harness} argv comes from the declared tokens"
            );
        }
        // A harness the table does not name answers with a reason that names
        // it, never an argv (AC5-ERR).
        let err = resume_argv_for("iambad", "sid").unwrap_err();
        assert!(err.contains("iambad"), "{err}");
        // Codex's declared form IS the fail-open render (x-eb79): what a
        // gesture runs when `fno-agents resume-argv` is unavailable. The
        // grant-bearing gesture argv is asserted on the staging tests.
        assert_eq!(
            resume_argv_for("codex", "01a027ad").unwrap(),
            vec!["codex".to_string(), "resume".to_string(), "01a027ad".into()],
        );
    }
}
