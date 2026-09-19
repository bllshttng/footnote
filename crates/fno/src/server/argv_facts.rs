//! The argv-fact helpers: what a pane's own argv proves about it.
//!
//! Provenance tokens, the attach/peek argv builders and their test
//! seams, the known-workers and restore-policy overrides, and the
//! isolated attach-context map. Pure helpers over argv and
//! thread-locals; the server reads them through the re-export.

use super::*;

/// Extract the `FNO_NODE` value from a pane-run `argv`. The `_mesh_env_wrapper`
/// (mux_spawn.py) prefixes the command with `env FNO_NODE=<id> ...`, so the id
/// is already in the argv the server receives - no new IPC from the pane. An
/// ad-hoc pane (no such token) yields `None`.
///
/// Anchored to the `env(1)` wrapper to avoid false positives: only a leading
/// `env` followed by its `NAME=VALUE` assignment run is scanned, stopping at
/// the actual command. So a real command that merely mentions `FNO_NODE=` in
/// its own args (e.g. `grep FNO_NODE=x file`) is never mistaken for provenance.
pub(super) fn node_from_argv(argv: &[String]) -> Option<String> {
    env_token_from_argv(argv, "FNO_NODE=")
}

/// The refused worker a restore placeholder was minted for, from the
/// same `env(1)` wrapper (`FNO_REFUSED_WORKER=<name>`). The refusal marker then
/// travels in the pane's own argv like every other durable pane fact, so a
/// later server re-derives it on keeper re-adoption and can sweep a
/// placeholder it did not mint - no stored state to go stale.
pub(super) fn refused_worker_from_argv(argv: &[String]) -> Option<String> {
    env_token_from_argv(argv, "FNO_REFUSED_WORKER=")
}

/// The pane's `FNO_ACCOUNT` birth account, parsed from the same
/// `env(1)` wrapper prefix as `FNO_NODE` (`_mesh_env_wrapper` stamps it when a
/// spawn was routed with `--account`). `None` for a default-account or ad-hoc
/// pane. This is the mux-spawned-pane source for the sideline account glyph
/// (managed accounts share `~/.claude`, so the roster can't distinguish them -
/// the pane's own birth env can).
pub(super) fn account_from_argv(argv: &[String]) -> Option<String> {
    env_token_from_argv(argv, "FNO_ACCOUNT=")
}

/// The pane's `FNO_AGENT_SELF` registered worker name, parsed from the
/// same `env(1)` wrapper prefix as `FNO_NODE`. `_mesh_env_wrapper` stamps it for
/// every mux-spawned agent pane - the unique identity the sideline row already
/// shows. The tab/pane title reads this, not the process table, so a QoS
/// wrapper (taskpolicy/nice) that rewrites argv[0] cannot collapse the title.
pub(super) fn agent_self_from_argv(argv: &[String]) -> Option<String> {
    env_token_from_argv(argv, "FNO_AGENT_SELF=")
}

/// The argv index where the `env(1)` `NAME=VALUE` assignment run begins:
/// past `env` itself and its option run. `_mesh_env_wrapper` emits an auth-var
/// scrub (`-u VAR`) BEFORE the assignments on an `--account` spawn, so
/// a naive "first token after env" scan would stop on `-u` and miss every
/// assignment (dropping both `FNO_NODE` and `FNO_ACCOUNT`). Skip `-u VAR` (and
/// `--unset VAR`) pairs, other `-flags`, and a `--` terminator. `None` when
/// argv doesn't start with `env`.
pub(super) fn env_assignments_start(argv: &[String]) -> Option<usize> {
    if argv.first().map(String::as_str) != Some("env") {
        return None;
    }
    let mut i = 1;
    while let Some(tok) = argv.get(i).map(String::as_str) {
        if tok == "--" {
            i += 1;
            break;
        }
        if tok.starts_with('-') {
            // `-u`/`--unset` consumes the next token (the var name to unset).
            i += if tok == "-u" || tok == "--unset" {
                2
            } else {
                1
            };
        } else {
            break; // the assignment run (or the command) starts here
        }
    }
    // A trailing bare `-u` / `--unset` advances past the end, and every caller
    // slices `argv[start..]`, which panics for start > len. Clamp here (one
    // fix, four call sites) rather than guarding each slice.
    Some(i.min(argv.len()))
}

/// Shared scan for a `NAME=` token in the leading `env(1)` assignment run of a
/// pane-run argv (anchored to `env` so a command that merely mentions the token
/// in its own args is never mistaken for provenance).
pub(super) fn env_token_from_argv(argv: &[String], prefix: &str) -> Option<String> {
    let start = env_assignments_start(argv)?;
    argv[start..]
        .iter()
        .take_while(|a| a.contains('='))
        .find_map(|a| a.strip_prefix(prefix))
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// The spawned command's basename for the tab-label chain: the first
/// argv token past an optional leading `env` + its `NAME=VALUE` run (the same
/// scan shape as [`node_from_argv`]). `None` when the scan finds no command -
/// spawn never fails on labeling.
pub(super) fn cmd_from_argv(argv: &[String]) -> Option<String> {
    let cmd = match env_assignments_start(argv) {
        // Past the assignment run (skip `NAME=VALUE`s) is the command; the
        // option run was already skipped, so `-u` never masquerades as the cmd.
        Some(start) => argv[start..].iter().find(|a| !a.contains('='))?,
        None => argv.first()?,
    };
    let base = cmd.rsplit('/').next().unwrap_or(cmd);
    (!base.is_empty()).then(|| base.to_string())
}

#[cfg(test)]
thread_local! {
    /// Test override for the attach program (see [`attach_argv`]): points unit
    /// tests at a benign binary so the attach spawn+swap path runs without claude.
    pub(super) static ATTACH_PROGRAM: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
    /// Test override for the Follow viewer program (see
    /// [`peek_argv`]): the real argv boots the deployed `fno` CLI, which under
    /// a loaded full-suite run can outlive any sane test budget. Same benign-
    /// binary pattern as `ATTACH_PROGRAM`.
    pub(super) static PEEK_PROGRAM: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

/// The base argv attaching bg session `id`: `claude attach <id>`. `id` is always
/// a positional arg (never a shell string), so an 8-hex id can only name a
/// session. Tests override the program via `set_attach_program`.
pub(super) fn attach_base(id: &str) -> Vec<String> {
    #[cfg(test)]
    if let Some(mut argv) = ATTACH_PROGRAM.with(|p| p.borrow().clone()) {
        argv.push(id.to_string());
        return argv;
    }
    vec!["claude".to_string(), "attach".to_string(), id.to_string()]
}

/// The argv attaching bg session `id`, routed to the right claude daemon. For an
/// isolated-account row (`config_dir` set), wrap with `env CLAUDE_CONFIG_DIR=<dir>`
/// so the attach hits that account's daemon instead of the ambient `~/.claude`
/// (codex P1: a bare `claude attach` under the default dir fails, or worse
/// targets a colliding default-account session); `FNO_ACCOUNT` rides along so the
/// re-attached pane keeps its account glyph. A default-account row passes `None`
/// and is byte-identical to the pre-feature attach.
pub(super) fn attach_argv(
    id: &str,
    account: Option<&str>,
    config_dir: Option<&std::path::Path>,
) -> Vec<String> {
    attach_argv_for(Some("claude"), id, account, config_dir)
}

/// One attach gesture's argv, rendered from the harness's DECLARED
/// attach form. Each harness gets its own interface, because the viewport execs
/// that interface rather than rendering one:
///
/// - a harness whose contract row (or `[harness.<name>.attach]` config
///   override) declares a form: that form, rendered - `claude attach <id>`,
///   `sh -c 'codex app-server daemon start; exec codex resume <id> --remote
///   unix://'`, and whatever a harness declares next. No account wrapper: a
///   declared non-claude form is addressed by its own endpoint (codex's
///   control socket via `CODEX_HOME`), and the wrapper is claude's routing.
/// - pi, whose argv carries env-dependent `--provider`/`--model` no static
///   form can name: its own builder.
///
/// cursor-agent declares no attach form at all (a second `--resume` is a
/// rival TUI on the same remote chat, not a join), so its rows never reach
/// here: no attach id resolves for them in the viewport.
///
/// Any other harness falls through to the claude shape, which is what every
/// caller did before a harness was passed at all; only rows that resolved an
/// attach id ever reach here.
pub(super) fn attach_argv_for(
    harness: Option<&str>,
    id: &str,
    account: Option<&str>,
    config_dir: Option<&std::path::Path>,
) -> Vec<String> {
    if harness == Some("pi") {
        // No account wrapper and no socket: a pi session is addressed
        // by the pair (cwd, session id), and the attach pane already spawns in
        // the row's cwd. That pairing is what makes this a JOIN onto the
        // session pi's rpc lane is driving rather than a second session under
        // the same id.
        return agents_view::pi_attach_argv(id);
    }
    // The declaration renders the argv, EXCEPT under a test program stub:
    // the stub pins the claude-shaped base, and pre-declaration it
    // reached every non-codex harness through attach_base. A declared
    // non-claude form renders exactly as production; claude and the
    // no-harness fallback route through attach_base, where the stub lives.
    // Without this, a stub naming anything but claude's own shape would
    // silently spawn the real claude in tests.
    let declared = harness.and_then(agents_view::attach_form);
    #[cfg(test)]
    let declared = {
        let stubbed = ATTACH_PROGRAM.with(|p| p.borrow().is_some());
        if stubbed && !matches!(harness, Some(h) if h != "claude") {
            None
        } else {
            declared
        }
    };
    let base = match declared {
        Some(form) => form.render(id),
        None => attach_base(id),
    };
    // The account wrapper is claude's routing, not a general one: it points a
    // claude attach at the right ~/.claude daemon. A harness carrying a
    // config_dir must not inherit a CLAUDE_CONFIG_DIR prefix.
    let dir = config_dir.filter(|_| harness.is_none_or(|h| h == "claude"));
    let Some(dir) = dir else {
        return base;
    };
    let mut wrapped = vec![
        "env".to_string(),
        format!("CLAUDE_CONFIG_DIR={}", dir.display()),
    ];
    if let Some(a) = account {
        wrapped.push(format!("FNO_ACCOUNT={a}"));
    }
    wrapped.extend(base);
    wrapped
}

#[cfg(test)]
pub(super) fn set_attach_program(argv: &[&str]) {
    ATTACH_PROGRAM.with(|p| *p.borrow_mut() = Some(argv.iter().map(|s| s.to_string()).collect()));
}

/// The Follow tier's viewer argv: `fno agents peek <name> --follow`,
/// tailing the row's transcript in the dedicated pane. Read-only by
/// construction (peek never writes the observed). Tests override the program
/// via `set_peek_program` so the spawn path runs without the deployed CLI.
pub(super) fn peek_argv(name: &str) -> Vec<String> {
    #[cfg(test)]
    if let Some(mut argv) = PEEK_PROGRAM.with(|p| p.borrow().clone()) {
        argv.push(name.to_string());
        return argv;
    }
    vec![
        "fno".to_string(),
        "agents".to_string(),
        "peek".to_string(),
        name.to_string(),
        "--follow".to_string(),
    ]
}

#[cfg(test)]
pub(super) fn set_peek_program(argv: &[&str]) {
    PEEK_PROGRAM.with(|p| *p.borrow_mut() = Some(argv.iter().map(|s| s.to_string()).collect()));
}

/// The Locate tier's one-screen explanation: the row's facts, the
/// single sentence saying why no viewport exists for it, and the route that
/// DOES reach it. Rendered through `sh -c 'printf ...; exec cat'` with every
/// line as an ARGV (`"$@"`), never interpolated into the script, so a row
/// field can never parse as shell. `exec cat` holds the PTY open until the
/// pane is closed - the screen is not an empty pane and not a dead one; it
/// stays up, self-teaching, exactly until the operator closes it.
pub(super) fn locate_argv(row: &RegistryAgent) -> Vec<String> {
    let harness = row.harness.as_deref().unwrap_or("(none recorded)");
    let cwd = if row.cwd.is_empty() {
        "(none recorded)"
    } else {
        row.cwd.as_str()
    };
    let lines = [
        "thread view - no live viewport exists for this row".to_string(),
        String::new(),
        format!("name:      {}", row.name),
        format!("harness:   {harness}"),
        // The registry records no substrate field; a paneless live row is by
        // construction daemon-hosted, and that is the true thing to say.
        "substrate: daemon-hosted (no pane; the daemon owns the session)".to_string(),
        format!("cwd:       {cwd}"),
        String::new(),
        format!(
            "why: the {harness} harness owns no interactive attach form and no \
             transcript reader, so no live viewport can be opened for it here."
        ),
        String::new(),
        format!("what reaches it: fno agents mail send {}", row.name),
    ];
    let mut argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        "printf '%s\\n' \"$@\"; exec cat".to_string(),
        "fno-locate".to_string(),
    ];
    argv.extend(lines);
    argv
}
#[cfg(test)]
thread_local! {
    /// Test override for the restore-time registry name set (see
    /// [`Core::known_worker_names`]): restore reads the REAL registry once,
    /// which unit tests cannot reach deterministically. The override is
    /// itself Option-layered: `None` (the default) reads the file,
    /// `Some(None)` simulates an unreadable registry, `Some(Some(set))` pins
    /// the name set.
    pub(super) static KNOWN_WORKERS: std::cell::RefCell<Option<Option<std::collections::HashSet<String>>>> =
        const { std::cell::RefCell::new(None) };
    pub(super) static HOLD_WORKERS_OVERRIDE: std::cell::RefCell<Option<bool>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn set_known_workers(names: &[&str]) {
    KNOWN_WORKERS
        .with(|p| *p.borrow_mut() = Some(Some(names.iter().map(|s| s.to_string()).collect())));
}

#[cfg(test)]
pub(super) fn set_known_workers_unreadable() {
    KNOWN_WORKERS.with(|p| *p.borrow_mut() = Some(None));
}

#[cfg(test)]
pub(super) fn set_hold_workers(value: bool) {
    HOLD_WORKERS_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(value));
}

#[cfg(test)]
thread_local! {
    /// Test override for the three-state startup restore policy,
    /// outranking the legacy `set_hold_workers` bool. `None` (the default)
    /// falls through to the bool override, then to the real config ladder.
    pub(super) static RESTORE_POLICY_OVERRIDE:
        std::cell::RefCell<Option<crate::digest_overlay::MuxRestorePolicy>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn set_restore_policy(policy: crate::digest_overlay::MuxRestorePolicy) {
    RESTORE_POLICY_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(policy));
}

#[cfg(test)]
pub(super) struct RestorePolicyGuard;

#[cfg(test)]
impl Drop for RestorePolicyGuard {
    fn drop(&mut self) {
        RESTORE_POLICY_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    }
}

/// The startup restore policy read from the config ladder. Free fn so the
/// test override arm and the production arm share one defaulting path.
pub(super) fn restore_policy_now() -> crate::digest_overlay::MuxRestorePolicy {
    std::env::current_dir()
        .ok()
        .as_deref()
        .map(crate::digest_overlay::mux_restore_policy)
        .unwrap_or(crate::digest_overlay::MuxRestorePolicy::Hold)
}

#[cfg(test)]
pub(super) struct HoldWorkersGuard;

#[cfg(test)]
impl Drop for HoldWorkersGuard {
    fn drop(&mut self) {
        HOLD_WORKERS_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    }
}

#[cfg(test)]
pub(super) fn clear_known_workers() {
    KNOWN_WORKERS.with(|p| *p.borrow_mut() = None);
}

#[cfg(test)]
pub(super) struct KnownWorkersGuard;

#[cfg(test)]
impl Drop for KnownWorkersGuard {
    fn drop(&mut self) {
        clear_known_workers();
    }
}

/// `short_id -> (account, config_dir)` for every isolated-account roster
/// worker, so restore can route a persisted isolated member's `claude attach` at
/// the right daemon (codex P1): at restore time `self.agents` is empty and the
/// stored member carries no account, so `attach_account_ctx` cannot resolve it -
/// this reverse lookup reads the isolated rosters directly. One-shot, read-only,
/// fail-open to empty.
pub(super) fn isolated_attach_ctx() -> HashMap<String, (String, std::path::PathBuf)> {
    let mut map = HashMap::new();
    for (account, roster_path) in agents_view::isolated_roster_paths() {
        let Some(dir) = agents_view::account_config_dir(&account) else {
            continue;
        };
        if let Ok(raw) = std::fs::read_to_string(&roster_path) {
            for w in agents_view::parse_roster(&raw).into_iter().flatten() {
                map.insert(w.short_id, (account.clone(), dir.clone()));
            }
        }
    }
    map
}

#[cfg(test)]
#[path = "argv_facts_tests.rs"]
mod argv_facts_tests;
