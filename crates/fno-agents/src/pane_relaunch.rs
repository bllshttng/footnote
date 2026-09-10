//! How a pane relaunch carries its identity (x-0345): the `mux pane run`
//! argv builder and the `env(1)` assignment run that names the relaunched
//! worker. Extracted from client_verbs (over the file budget, shrink-only):
//! the code the identity change touched moved here with the change.

/// The `env(1)` assignment run that carries a row's mesh identity into a
/// relaunched pane: the same pairs `_mesh_env_wrapper` writes at spawn
/// (mux_spawn.py), identity tokens ONLY. The account and model ride
/// `--settings <path>` / the job's saved launch, so no value from inside a
/// route file can reach a printed command (re-pins #830 AC5). Err when a
/// token cannot ride an assignment: spawn validates names at mint, but the
/// resume path reads them back from the registry, so the wrap re-validates
/// and the caller refuses the relaunch rather than emit a shape
/// `agent_self_from_argv` cannot parse.
pub(crate) fn mesh_identity_assignments(
    name: &str,
    harness: &str,
    node: Option<&str>,
) -> Result<Vec<String>, String> {
    // An empty harness or node is OPTIONAL provenance (a degenerate row
    // can carry neither field) and is omitted, not written as an empty
    // assignment; an empty NAME is the one hard error - the wrapper exists
    // to carry it. `node` is the backlog node id (ReentryPlan::node /
    // RegistryEntry::node) -- a distinct axis from fno_id, the thread/session
    // identity. A caller that passes fno_id here stamps a session id into
    // FNO_NODE, which is what happened at both call sites before this fix.
    let mut pairs: Vec<(&str, &str)> = vec![("FNO_AGENT_SELF", name)];
    if !harness.is_empty() {
        pairs.push(("FNO_AGENT_HARNESS", harness));
    }
    if let Some(id) = node.filter(|id| !id.is_empty()) {
        pairs.push(("FNO_NODE", id));
    }
    for (key, value) in &pairs {
        if value.is_empty() || value.contains('=') || value.contains('\n') {
            return Err(format!(
                "row identity token {key}={value:?} cannot ride an env(1) assignment"
            ));
        }
    }
    Ok(pairs.iter().map(|(k, v)| format!("{k}={v}")).collect())
}

/// A row relaunches onto its mux pane when it HAS one and a session id to
/// relaunch with. Keyed on evidence the registry row carries, never on a
/// harness name (law d-dbf83820): `mux_spawn` writes the mux ref for every
/// `--substrate pane` row, and the empty-`session_id` refusal for a harness
/// whose resume needs one already ran before this predicate is consulted
/// (x-eb79: this replaces the `harness == "claude"` narrowing that sent every
/// non-claude pane row to the in-terminal exec).
pub(crate) fn pane_relaunch_target<'a>(
    mux_session: Option<&'a str>,
    resume_id: &str,
) -> Option<&'a str> {
    mux_session.filter(|_| !resume_id.is_empty())
}

/// The row name a relaunched pane may carry as `--worker`, or `None` when the
/// name cannot ride the flag: the mux server validates it with the same
/// registry charset (`[A-Za-z0-9._-]`, <= 64 chars) and refuses the WHOLE
/// `pane run` on a bad token, so omitting the token keeps the relaunch alive
/// (unjoined) instead of failing it. Mirrors `squad_store::valid_worker_name`,
/// which lives across the crate boundary fno-agents cannot link.
fn worker_token(name: &str) -> Option<&str> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    ok.then_some(name)
}

/// Build the `mux pane run` argv (everything after the `fno` binary) that
/// relaunches `claude_argv` on a new pane in `session` at `cwd`. The `--` fence
/// keeps a `--resume <uuid>` (or any flag-shaped inner arg) out of the mux
/// parser, so the resumed command is transported verbatim - the one-verb form
/// of the manual `fno mux pane run 'cd <wt> && exec claude --resume <uuid>'`
/// recovery recipe (x-b84f D3). `identity` rides as an `env(1)` assignment
/// run INSIDE the fence (x-0345 W1): the server's `agent_self_from_argv`
/// reads exactly this shape to title the pane, and the same assignments set
/// the env the session-start restamp keys on - without it a relaunched pane
/// comes back anonymous, titled from the command basename. `worker` rides as
/// `--worker` BEFORE the fence (x-eb79), matching what spawn passes at
/// `mux_spawn.py`: the server records the pane as a squad member joined to
/// that row, so the relaunch survives a mux restart as an idle resumable row.
/// Callers keep any `which_on_path` check on the UNWRAPPED harness argv; the
/// wrap happens here.
pub(crate) fn mux_pane_run_argv(
    session: &str,
    cwd: &str,
    claude_argv: &[String],
    identity: &[String],
    worker: Option<&str>,
) -> Vec<String> {
    let mut v: Vec<String> = vec![
        "mux".into(),
        "pane".into(),
        "run".into(),
        "--session".into(),
        session.into(),
        "--cwd".into(),
        cwd.into(),
    ];
    if let Some(name) = worker.and_then(worker_token) {
        v.push("--worker".into());
        v.push(name.into());
    }
    v.push("--".into());
    if !identity.is_empty() {
        v.push("env".into());
        v.extend(identity.iter().cloned());
    }
    v.extend(claude_argv.iter().cloned());
    v
}

#[cfg(test)]
mod tests {
    use super::{mesh_identity_assignments, mux_pane_run_argv, pane_relaunch_target, worker_token};

    #[test]
    fn mux_pane_run_argv_fences_the_resumed_command() {
        // x-b84f D3 + x-0345 W1: the one-verb form of the manual recovery now
        // carries the row's identity past the fence, in the same `env(1)`
        // assignment-run shape `_mesh_env_wrapper` writes at spawn and
        // `agent_self_from_argv` reads for the pane title. The `--` fence
        // keeps the inner `--resume <uuid>` (and any flag-shaped arg) out of
        // the mux parser, so the resumed command is transported verbatim.
        // AC5: only a path appears, never a value from inside the file.
        let claude = vec![
            "claude".to_string(),
            "--settings".into(),
            "/route/path.json".into(),
            "--resume".into(),
            "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9".into(),
        ];
        let identity =
            mesh_identity_assignments("x-f75e-mux-chrome", "claude", Some("x-f75e")).unwrap();
        assert!(identity.iter().all(|t| t.starts_with("FNO_")));
        let pane = mux_pane_run_argv("main", "/wt", &claude, &identity, Some("x-f75e-mux-chrome"));
        assert_eq!(
            pane,
            vec![
                "mux".to_string(),
                "pane".into(),
                "run".into(),
                "--session".into(),
                "main".into(),
                "--cwd".into(),
                "/wt".into(),
                "--worker".into(),
                "x-f75e-mux-chrome".into(),
                "--".into(),
                "env".into(),
                "FNO_AGENT_SELF=x-f75e-mux-chrome".into(),
                "FNO_AGENT_HARNESS=claude".into(),
                "FNO_NODE=x-f75e".into(),
                "claude".into(),
                "--settings".into(),
                "/route/path.json".into(),
                "--resume".into(),
                "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9".into(),
            ]
        );
        // The fence sits exactly between the mux transport and the command,
        // and the wrapper follows it: `agent_self_from_argv` answers only an
        // argv that STARTS with `env` (env_assignments_start), so assert the
        // token itself, never the absence of a basename.
        assert_eq!(pane.iter().position(|a| a == "--"), Some(9));
        assert_eq!(pane[10], "env");
        assert_eq!(pane[11], "FNO_AGENT_SELF=x-f75e-mux-chrome");
        assert_eq!(pane[12], "FNO_AGENT_HARNESS=claude");
        // Route values never enter the wrapper; the path rides `--settings`
        // (re-pins #830 AC5 against the identity wrap).
        let joined = pane.join(" ");
        assert!(!joined.contains("FNO_ROUTE=") && !joined.contains("token"));
    }

    #[test]
    fn mux_pane_run_argv_omits_an_unrecordable_worker_name() {
        // AC2-EDGE: a name the mux server's `valid_worker_name` rejects would
        // refuse the whole `pane run`, so the builder omits `--worker` and the
        // rest of the argv is unchanged - the relaunch proceeds unjoined
        // rather than failing on a token the store would drop anyway.
        let argv = vec!["codex".to_string(), "resume".into(), "s-1".into()];
        let pane = mux_pane_run_argv("main", "/wt", &argv, &[], Some("bad name"));
        assert!(!pane.contains(&"--worker".to_string()));
        assert_eq!(pane.iter().position(|a| a == "--"), Some(7));
        // The happy-path token sits before the fence, where spawn puts it.
        let joined = mux_pane_run_argv("main", "/wt", &argv, &[], Some("t-ok_1.2")).join(" ");
        assert!(joined.contains("--worker t-ok_1.2 --"));
        assert_eq!(worker_token(""), None);
        assert_eq!(worker_token(&"x".repeat(65)), None);
        assert_eq!(worker_token("x"), Some("x"));
    }

    #[test]
    fn pane_relaunch_target_keys_on_evidence_not_harness() {
        // AC1: a row with a mux ref and a session id takes the pane; the same
        // row without an id (the empty-session-id guard already refused for
        // harnesses that need one, so this is defense in depth) and a row
        // with no mux ref (thread lane) both answer None - keyed on the pair
        // the branch consults, never on a harness name (AC1-OC).
        assert_eq!(pane_relaunch_target(Some("main"), "sid-1"), Some("main"));
        assert_eq!(pane_relaunch_target(Some("main"), ""), None);
        assert_eq!(pane_relaunch_target(None, "sid-1"), None);
        assert_eq!(pane_relaunch_target(None, ""), None);
    }

    #[test]
    fn mesh_identity_assignments_refuse_tokens_that_cannot_ride_env() {
        // A token carrying '=' or a newline cannot ride an env(1) assignment;
        // `agent_self_from_argv` would mis-parse or miss it. Registry names
        // are validated at mint; resume re-validates at the wrap and refuses.
        assert!(mesh_identity_assignments("bad=name", "claude", None).is_err());
        assert!(mesh_identity_assignments("bad\nname", "claude", None).is_err());
        assert!(mesh_identity_assignments("", "claude", None).is_err());
        assert!(mesh_identity_assignments("ok", "c=l", None).is_err());
        // An empty harness or fno_id is optional provenance, not an error:
        // it is omitted (a degenerate row must still print/launch carrying
        // its name), never written as an empty assignment.
        let a = mesh_identity_assignments("ok", "claude", Some("")).unwrap();
        assert_eq!(a, vec!["FNO_AGENT_SELF=ok", "FNO_AGENT_HARNESS=claude"]);
        let b = mesh_identity_assignments("ok", "", None).unwrap();
        assert_eq!(b, vec!["FNO_AGENT_SELF=ok"]);
    }
}
