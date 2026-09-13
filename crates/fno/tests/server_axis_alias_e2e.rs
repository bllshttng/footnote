//! x-f209: the mux server axis is spelled `server`. The verbs with no pure
//! parser (tab, layout, workspace restore, rows, where, `mux server`,
//! `serve --web`) accept `--server`, keep `--session` working with exactly
//! one deprecation line, and the env precedence is flag > FNO_SERVER >
//! FNO_SESSION > main. Runs the real binary against a scratch FNO_MUX_DIR,
//! so the shared mux is never touched.

mod common;

use std::process::Command;

use common::{ClientHarness, Scratch};

const SESSION_LINE: &str =
    "warning: FNO_SESSION is deprecated; use FNO_SERVER instead. The alias will be removed in a future release.";
const FLAG_LINE: &str =
    "warning: --session is deprecated; use --server instead. The alias will be removed in a future release.";

fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// Run the real `fno` in the scratch mux dir with extra env pinned per call.
/// Every FNO_* var the caller does not name is stripped, so each case decides
/// the whole env matrix.
fn fno_env(scratch: &Scratch, envs: &[(&str, &str)], args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fno"));
    scratch.isolate_command(&mut cmd);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.args(args).output().unwrap()
}

#[test]
fn server_axis_verbs_accept_server_and_warn_only_on_session() {
    // AC1/AC2 on the verbs with no pure parser. Every verb here has no live
    // server under its name, so the parse-level proof is: stderr names the
    // server value, never `unknown argument`, and the --session spelling
    // carries exactly one deprecation line while --server carries none.
    let scratch = Scratch::new("axis-verbs");

    // rows (take_common_flags -> run_on_existing_server refuses naming it).
    let out = fno_env(&scratch, &[], &["mux", "rows", "--server", "s-rows"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("unknown argument"),
        "rows --server parses: {err}"
    );
    assert!(err.contains("s-rows"), "rows names the server: {err}");
    assert_eq!(
        count(&err, FLAG_LINE),
        0,
        "no deprecation for --server: {err}"
    );

    let out = fno_env(&scratch, &[], &["mux", "rows", "--session", "s-rows"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("unknown argument"),
        "rows --session parses: {err}"
    );
    assert!(err.contains("s-rows"), "rows names the server: {err}");
    assert_eq!(
        count(&err, FLAG_LINE),
        1,
        "exactly one line for --session: {err}"
    );

    // where (same common prefix).
    let out = fno_env(&scratch, &[], &["mux", "where", "x", "--server", "s-where"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("unknown argument"),
        "where --server parses: {err}"
    );

    let out = fno_env(
        &scratch,
        &[],
        &["mux", "where", "x", "--session", "s-where"],
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("unknown argument"),
        "where -- spellings parse: {err}"
    );
    assert_eq!(count(&err, FLAG_LINE), 1, "one line: {err}");

    // tab ls (per-verb parser).
    let out = fno_env(&scratch, &[], &["mux", "tab", "ls", "--server", "s-tab"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("unknown argument"),
        "tab --server parses: {err}"
    );
    assert_eq!(count(&err, FLAG_LINE), 0, "no line for --server: {err}");

    let out = fno_env(&scratch, &[], &["mux", "tab", "ls", "--session", "s-tab"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(count(&err, FLAG_LINE), 1, "one line: {err}");

    // layout get (the skip shape).
    let out = fno_env(
        &scratch,
        &[],
        &["mux", "layout", "get", "--server", "s-lay"],
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("unknown argument"),
        "layout --server parses: {err}"
    );
    assert_eq!(count(&err, FLAG_LINE), 0);

    let out = fno_env(
        &scratch,
        &[],
        &["mux", "layout", "get", "--session", "s-lay"],
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(count(&err, FLAG_LINE), 1, "one line: {err}");

    // workspace restore.
    let out = fno_env(
        &scratch,
        &[],
        &["mux", "workspace", "restore", "--server", "s-ws"],
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("unknown argument"),
        "restore --server parses: {err}"
    );
    assert_eq!(count(&err, FLAG_LINE), 0);

    let out = fno_env(
        &scratch,
        &[],
        &["mux", "workspace", "restore", "--session", "s-ws"],
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(count(&err, FLAG_LINE), 1, "one line: {err}");

    // serve --web: the web bridge refuses on a dead server, naming it.
    let out = fno_env(
        &scratch,
        &[],
        &["mux", "serve", "--web", "--server", "s-web", "--port", "1"],
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("unknown argument"),
        "serve --web --server parses: {err}"
    );
    assert_eq!(count(&err, FLAG_LINE), 0, "no line for --server: {err}");
}

/// Mint a server for `name` via the public verb, wait for its socket, then
/// kill it. `alias` picks the spelling under test. Returns the child's stderr
/// (empty when not captured).
fn mint_and_kill(scratch: &Scratch, name: &str, alias: &str, capture_stderr: bool) -> String {
    let sock = scratch.0.join(format!("{name}.sock"));
    let mut c = Command::new(env!("CARGO_BIN_EXE_fno"));
    scratch.isolate_command(&mut c);
    c.args(["mux", "server", alias, name]);
    let mut child = c
        .stdout(std::process::Stdio::null())
        .stderr(if capture_stderr {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .spawn()
        .expect("server child spawns");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !sock.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "mux server {alias} {name} never created its socket"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = child.kill();
    match child.wait_with_output() {
        Ok(out) => String::from_utf8_lossy(&out.stderr).into_owned(),
        Err(_) => String::new(),
    }
}

#[test]
fn server_axis_mux_server_role_takes_server_flag() {
    // AC1/AC2: `mux server --server <name>` mints the server; the --session
    // alias keeps minting with exactly one deprecation line. Distinct names,
    // so the second mint never inherits the first's socket.
    let scratch = Scratch::new("axis-server");
    let via_server = mint_and_kill(&scratch, "s-ax", "--server", false);
    assert_eq!(count(&via_server, FLAG_LINE), 0);
    let via_session = mint_and_kill(&scratch, "s-ax2", "--session", true);
    assert_eq!(
        count(&via_session, FLAG_LINE),
        1,
        "exactly one line: {via_session}"
    );
}

#[test]
fn server_axis_env_precedence_matrix() {
    // AC5-AC8: flag > FNO_SERVER > FNO_SESSION > main; the FNO_SESSION line
    // prints only when FNO_SESSION decided. Each case is a fresh child
    // process, so the once-per-process note applies per invocation.
    let scratch = Scratch::new("axis-env");

    // AC5: FNO_SERVER beats FNO_SESSION silently.
    let out = fno_env(
        &scratch,
        &[("FNO_SERVER", "env-a"), ("FNO_SESSION", "env-b")],
        &["mux", "rows"],
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("\"env-a\""), "FNO_SERVER decided: {err}");
    assert_eq!(
        count(&err, SESSION_LINE),
        0,
        "silent when FNO_SERVER wins: {err}"
    );

    // AC6: only FNO_SESSION -> one line, and it decided.
    let out = fno_env(&scratch, &[("FNO_SESSION", "env-b")], &["mux", "rows"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("\"env-b\""), "FNO_SESSION decided: {err}");
    assert_eq!(count(&err, SESSION_LINE), 1, "one line: {err}");

    // AC7: explicit flag beats FNO_SESSION silently.
    let out = fno_env(
        &scratch,
        &[("FNO_SESSION", "env-b")],
        &["mux", "rows", "--server", "env-a"],
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("\"env-a\""), "flag decided: {err}");
    assert_eq!(
        count(&err, SESSION_LINE),
        0,
        "flag silences the line: {err}"
    );

    // AC8: roles that resolve no server never print the line.
    let out = fno_env(
        &scratch,
        &[("FNO_SESSION", "env-b")],
        &["version", "--json"],
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(count(&err, SESSION_LINE), 0, "version stays silent: {err}");
    let out = fno_env(&scratch, &[("FNO_SESSION", "env-b")], &["mux", "ls"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(count(&err, SESSION_LINE), 0, "mux ls stays silent: {err}");

    // The bare top-level attach spelling: no line for --server.
    let out = fno_env(&scratch, &[], &["--server", "nope", "--json"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(count(&err, FLAG_LINE), 0, "--server never warns: {err}");
}

#[test]
fn server_axis_nested_attach_guard_names_server() {
    // AC10-ERR: inside a pane of server `work` (FNO_SERVER=work, or only the
    // legacy FNO_SESSION=work), `fno --server work` refuses and names both
    // remedies. Needs a real PTY: the guard lives in the client, pre-alt-screen.
    let scratch = Scratch::new("axis-guard");
    for env in [[("FNO_SERVER", "work")], [("FNO_SESSION", "work")]] {
        let mut h = ClientHarness::spawn_with(&scratch, &env);
        let status = h.wait_exit(15);
        assert!(!status.success(), "nested attach must refuse: {env:?}");
        let out = h.raw_output();
        assert!(out.contains("work"), "names the server: {out}");
        assert!(
            out.contains("--server") && out.contains("unset FNO_SERVER FNO_SESSION"),
            "names both remedies: {out}"
        );
    }
}
