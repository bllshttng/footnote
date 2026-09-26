//! How a pane relaunch carries its identity : the `mux pane run`
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
/// recovery recipe (D3). `identity` rides as an `env(1)` assignment
/// run INSIDE the fence (W1): the server's `agent_self_from_argv`
/// reads exactly this shape to title the pane, and the same assignments set
/// the env the session-start restamp keys on - without it a relaunched pane
/// comes back anonymous, titled from the command basename. `worker` rides as
/// `--worker` BEFORE the fence, matching what spawn passes at
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
        "--server".into(),
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

use std::path::Path;
use std::time::{Duration, Instant};

use crate::client_verbs::shlex_quote;
use crate::pane_stop::{pane_list_via_fno, PaneSighting};
use crate::scrape::mux_pane_read;

/// How long a relaunched keeper gets to prove the worker stayed up (mirrors
/// `_BINDING_WINDOW_S` in mux_spawn.py). It fits inside the watchdog's 180s
/// resume timeout.
pub(crate) const PANE_PROOF_WINDOW: Duration = Duration::from_secs(8);

/// Provider-specific resume argv, mirroring Python `_build_resume_argv`.
/// Returns `None` for an unsupported provider AND for an unreadable capability
/// contract, but the caller only ever sees the second kind through a narrow
/// door. `interactive_resume_supported` also reads the packaged contract and
/// `unwrap_or(false)`s a failure, so an unreadable contract refuses as "not
/// supported" before this function runs. What actually reaches the caller's
/// "resume contract is invalid" message is a contract that LOADS and declares
/// the form, then fails to render it: a malformed token template.
///
/// The grant and the directory pin ride ONE `cwd` here, which is what the CLI
/// verb lane wants (it validates the cwd exists before launching). The mux
/// gesture needs them SPLIT: the grant follows the directory the worker will
/// actually get, while `--cd` must not pin a fallback directory (AC3-GONE),
/// so it calls [`build_resume_argv_split`] directly.
pub(crate) fn build_resume_argv(
    provider: &str,
    session_id: &str,
    cwd: Option<&str>,
) -> Option<Vec<String>> {
    let cwd = cwd.filter(|c| !c.is_empty());
    build_resume_argv_split(provider, session_id, cwd, cwd.is_some())
}

/// The grant/pin split behind [`build_resume_argv`]: `grant_cwd`
/// decides the codex writable-roots grant (None/empty = no grant), `pin_cd`
/// decides `--cd` independently. The mux gesture grants the directory the
/// worker will actually get and pins it only when it is the row's own
/// recorded cwd - pinning a fallback ($HOME, the squad canonical cwd) raises
/// codex's folder-trust screen, an unattended hang (AC3-GONE).
pub(crate) fn build_resume_argv_split(
    provider: &str,
    session_id: &str,
    grant_cwd: Option<&str>,
    pin_cd: bool,
) -> Option<Vec<String>> {
    let argv = build_resume_argv_tokens_split(provider, session_id, grant_cwd, pin_cd)?;
    crate::harness_capabilities::compose_pre_exec(provider, "interactive_resume", argv).ok()
}

/// Raw resume command tokens before the lane's declared `pre_exec` wrapper.
/// Codex paths splice route args before composing the wrapper.
pub(crate) fn build_resume_argv_tokens_split(
    provider: &str,
    session_id: &str,
    grant_cwd: Option<&str>,
    pin_cd: bool,
) -> Option<Vec<String>> {
    // The declared form is the whole identity: cursor-agent's interactive_resume
    // tokens already end in --trust, and a second one is a duplicated flag,
    // never a stronger one. Python's builder renders the same form with no
    // cursor arm, so runtimes stay byte-identical by rendering and nothing else.
    // Raw render, splice, compose last: the composed `sh -c` script would
    // put a spliced grant OUTSIDE the codex command it must precede.
    let mut argv = crate::harness_capabilities::render_session_argv_raw(
        provider,
        "interactive_resume",
        Some(session_id),
    )
    .ok()?;
    // codex's bounded sandbox re-resolves from config on `resume`, so the git +
    // plan grants ride as `-c` tokens spliced right after the `codex` binary
    // token. (`codex resume` does accept --add-dir; `codex exec resume` is the
    // lane that does not. `-c` is kept because one grant builder serves both.)
    if provider == "codex" {
        // The grant follows the directory the worker will actually get; `--cd`
        // rides separately. An empty grant_cwd is absent for both, which is
        // what Python's `if cwd` does and the parity test pins (AC4-EDGE).
        if let Some(cwd) = grant_cwd.filter(|c| !c.is_empty()) {
            // codex 0.156.1 refuses a `sandbox_workspace_write.writable_roots`
            // override paired with `--remote` ("Configure additional workspace
            // roots on the server"), and the declared codex resume/attach forms
            // all ride `--remote unix://`. On a --remote form the `-c` splice
            // is dropped and the roots reach the thread the way delivery
            // carries them: codex_inject::inject probes the resolved sandbox
            // and widens the roots on the turn/start policy, and the daemon
            // thread lanes grant via granted_roots on every turn. A non-remote
            // form keeps the splice.
            let remote_form = argv.iter().any(|token| token == "--remote");
            let grant = if remote_form {
                Vec::new()
            } else {
                crate::provider::codex_writable_config_args(Path::new(cwd))
            };
            let grant_len = grant.len();
            if !grant.is_empty() {
                argv.splice(1..1, grant);
            }
            // Without --cd, codex asks session-directory vs current-directory
            // and defaults to the SESSION directory: the canonical checkout
            // recorded at spawn, not the worktree the row works in. Unattended
            // that prompt is a hang. Attended it is a wrong default a human
            // must catch.
            //
            // Conditional, per codex's own docs: the prompt appears only when
            // the process cwd differs from the session's saved directory. The
            // config key `tui.resume_cwd` answers it globally, and --cd
            // outranks that. This lane wants --cd because it is per
            // invocation and names the directory outright.
            //
            // Spliced BEFORE the subcommand, beside the grant, which is the
            // only global-before-subcommand precedent in this tree. The spawn
            // lanes are not it: they spell the flag `-C`, after `exec` in the
            // headless lane and on a bare `codex` in the pane lane. Both
            // positions parse on codex 0.149.1, so this is a choice about
            // where a reader expects a global, not a fix.
            //
            // NO permission bypass rides here, deliberately. A registry row
            // records no sandbox posture, so this lane cannot tell a bounded
            // worker from a yolo one, and an unconditional bypass would resume
            // every bounded worker with approvals off. See the Python twin.
            // Right after the grant, so the token order matches the Python
            // twin exactly. `test_rust_verb_parity` compares the two argvs
            // element for element, so "both are globals" is not enough here.
            // The split caller (the mux gesture) omits `--cd` when it passes
            // `pin_cd == false`: the worker lands on a fallback directory and
            // codex's own session-directory offer is the one a human can take.
            if pin_cd {
                let at = (1 + grant_len).min(argv.len());
                argv.splice(at..at, ["--cd".to_string(), cwd.to_string()]);
            }
        }
    }
    Some(argv)
}

/// The env(1) assignment tokens for one env pair set, prefixed ahead of the
/// argv - the shape the mux verdict prefix prints and both resume print arms
/// (claude's canonical plan, codex's masked route env) reuse.
pub(crate) fn env_prefixed(env: &[(String, String)], argv: &[String]) -> Vec<String> {
    let mut prefixed: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    prefixed.extend(argv.iter().cloned());
    prefixed
}

/// The `--print-command` tail: the pane form when the row has a mux ref, else
/// the in-terminal exec form. Inspection only - shell-quoted paths and ids;
/// any key-masked env already rides `argv` as tokens.
pub(crate) fn print_relaunch_command(
    session: Option<&str>,
    cwd: &str,
    argv: &[String],
    identity: &[String],
    worker: &str,
) {
    if let Some(session) = session {
        let pane = mux_pane_run_argv(session, cwd, argv, identity, Some(worker));
        let quoted = pane
            .iter()
            .map(|a| shlex_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        println!("fno {quoted}");
    } else {
        let quoted = argv
            .iter()
            .map(|a| shlex_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        println!("cd {} && exec {}", shlex_quote(cwd), quoted);
    }
}

/// The `fno-agents resume-argv` verb: render one harness's
/// interactive-resume argv through the ONE builder the CLI verb lane uses,
/// so the mux gesture consumes the same argv instead of re-deriving the
/// declared form and losing the codex writable-roots grant. `--cwd` supplies
/// the grant (and the `--cd` value); `--cd` pins the directory separately,
/// so a fallback directory can be granted without being pinned (AC3-GONE).
/// `--json` prints `{"argv":[...]}`. A harness the capability table does not
/// name (or one whose declared form cannot render) exits 1: the mux gesture
/// treats any failure as the fail-open signal and renders the declared form
/// itself, never a second argv builder.
pub fn run_resume_argv(rest: &[String]) -> i32 {
    let mut positional: Vec<&str> = Vec::new();
    let mut cwd: Option<String> = None;
    let mut pin_cd = false;
    let mut json = false;
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--cwd" => match it.next() {
                Some(v) => cwd = Some(v.to_string()),
                None => {
                    eprintln!("resume-argv: --cwd needs a path");
                    return 2;
                }
            },
            "--cd" => pin_cd = true,
            "--json" | "-J" => json = true,
            t if t.starts_with('-') => {
                eprintln!("resume-argv: unknown flag {t}");
                return 2;
            }
            t => positional.push(t),
        }
    }
    if positional.len() != 2 {
        eprintln!(
            "usage: fno-agents resume-argv <harness> <session-id> [--cwd <path>] [--cd] [--json|-J]"
        );
        return 2;
    }
    let harness = positional[0];
    let session_id = positional[1];
    // this verb's stdout is a recipe the mux gesture pastes into a
    // pane, and a pane spawn has no secret-free channel for a route's key
    // (the claude verdict prefix puts `env K=V` on the argv, visible in ps).
    // A routed codex row therefore refuses here by name instead of printing
    // the unrouted recipe, and the caller (and the operator) are pointed at
    // the door that restores the route. No row, or an unrouted row, changes
    // nothing.
    if harness == "codex" {
        if let Some(home) = crate::paths::AgentsHome::from_env_opt() {
            let entries = match crate::client_verbs::read_registry_entries(&home.registry_json()) {
                Ok(e) => e,
                Err(_) => Vec::new(),
            };
            let row = entries.iter().find(|e| {
                e.get("harness_session_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(session_id)
            });
            if let Some(row) = row {
                let name = row
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(session_id);
                let identity = crate::codex_route::row_route_identity(
                    row.get("harness").and_then(serde_json::Value::as_str),
                    row.get("route_provider_id")
                        .and_then(serde_json::Value::as_str),
                    row.get("model_name").and_then(serde_json::Value::as_str),
                );
                let refusal = match identity {
                    Err(reason) => Some(format!(
                        "{name} was launched on a codex route and it cannot be carried \
                         through this door ({reason}); resume it with \
                         `fno agents resume {name}`, which restores the route"
                    )),
                    Ok(Some((provider, _))) => Some(format!(
                        "{name} runs on codex route {provider}; resume it with \
                         `fno agents resume {name}`, which restores the route"
                    )),
                    Ok(None) => None,
                };
                if let Some(line) = refusal {
                    eprintln!("resume-argv: {line}");
                    return crate::reentry::REENTRY_REFUSED_EXIT;
                }
            }
        }
    }
    match build_resume_argv_split(harness, session_id, cwd.as_deref(), pin_cd) {
        Some(argv) => {
            if json {
                println!("{}", serde_json::json!({ "argv": argv }).to_string());
            } else {
                let quoted = argv
                    .iter()
                    .map(|a| shlex_quote(a))
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("{quoted}");
            }
            0
        }
        None => {
            eprintln!("resume-argv: harness {harness} declares no renderable resume form");
            1
        }
    }
}
#[cfg(test)]
mod tests {
    use super::{mesh_identity_assignments, mux_pane_run_argv, worker_token};

    #[test]
    fn resume_argv_accepts_both_json_spellings() {
        let sid = "test-resume-session".to_string();
        // -J reaches the JSON branch (0), not the unknown-flag refusal (2).
        assert_eq!(
            super::run_resume_argv(&["claude".into(), sid.clone(), "-J".into()]),
            0
        );
        assert_eq!(
            super::run_resume_argv(&["claude".into(), sid, "--json".into()]),
            0
        );
        assert_eq!(super::run_resume_argv(&["claude".into(), "-J".into()]), 2);
    }

    #[test]
    fn mux_pane_run_argv_fences_the_resumed_command() {
        // D3 + W1: the one-verb form of the manual recovery now
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
            mesh_identity_assignments("x-bbbb-mux-chrome", "claude", Some("x-bbbb")).unwrap();
        assert!(identity.iter().all(|t| t.starts_with("FNO_")));
        let pane = mux_pane_run_argv("main", "/wt", &claude, &identity, Some("x-bbbb-mux-chrome"));
        assert_eq!(
            pane,
            vec![
                "mux".to_string(),
                "pane".into(),
                "run".into(),
                "--server".into(),
                "main".into(),
                "--cwd".into(),
                "/wt".into(),
                "--worker".into(),
                "x-bbbb-mux-chrome".into(),
                "--".into(),
                "env".into(),
                "FNO_AGENT_SELF=x-bbbb-mux-chrome".into(),
                "FNO_AGENT_HARNESS=claude".into(),
                "FNO_NODE=x-bbbb".into(),
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
        assert_eq!(pane[11], "FNO_AGENT_SELF=x-bbbb-mux-chrome");
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
