//! The server-axis unit tests (x-f209): the pure parser and resolver tests
//! over both spellings, moved out under the file-budget gate.
use super::*;

#[test]
fn mux_session_resolution_flag_beats_env_beats_default() {
    // Locked 7: the flag beats the env beats the default (AC3-EDGE).
    assert_eq!(resolve_session(Some("other"), Some("work")), "other");
    assert_eq!(resolve_session(None, Some("work")), "work");
    assert_eq!(resolve_session(None, None), DEFAULT_SESSION);
    // An empty env var reads as unset, not as a session named "".
    assert_eq!(resolve_session(None, Some("")), DEFAULT_SESSION);
}

#[test]
fn server_axis_resolve_session_accepts_both_spellings() {
    // The two spellings name one axis; the resolver is spelling-blind.
    assert_eq!(resolve_session(Some("work"), None), "work");
    assert_eq!(resolve_session(None, Some("work")), "work");
}

#[test]
fn server_axis_pane_run_parser_accepts_server_alias_pair() {
    // AC1/AC3: --server x and --session x parse to the same value, on the
    // run parser and on a non-run pane verb.
    let run_server =
        pane_args(&["run", "--server", "s1", "--", "echo", "hi"]).expect("--server parses on run");
    let run_alias =
        pane_args(&["run", "--session", "s1", "--", "echo", "hi"]).expect("--session still parses");
    assert_eq!(run_server.session, run_alias.session);
    assert_eq!(run_server.session.as_deref(), Some("s1"));

    let kill_server = pane_args(&["kill", "--server", "s2", "main:7"]).expect("must parse");
    let kill_alias = pane_args(&["kill", "--session", "s2", "main:7"]).expect("must parse");
    assert_eq!(kill_server.session, kill_alias.session);
    assert_eq!(kill_server.session.as_deref(), Some("s2"));
}

#[test]
fn server_axis_take_common_flags_accepts_server_alias_pair() {
    // rows/where/view/thread/reseat/retire-session share this prefix.
    let args: Vec<OsString> = ["--server", "s3", "--json"]
        .iter()
        .map(OsString::from)
        .collect();
    let (server, json, rest) = take_common_flags(&args).expect("--server parses");
    assert_eq!(server.as_deref(), Some("s3"));
    assert!(json);
    assert!(rest.is_empty());

    let args: Vec<OsString> = ["--session", "s3", "--json"]
        .iter()
        .map(OsString::from)
        .collect();
    let (server, json, rest) = take_common_flags(&args).expect("--session still parses");
    assert_eq!(server.as_deref(), Some("s3"));
    assert!(json);
    assert!(rest.is_empty());
}

#[test]
fn server_axis_block_parsers_accept_server_alias_pair() {
    // AC1: block pipe and block annotate.
    let pipe_server =
        parse_block_args(&os(&["pipe", "--from", "4", "--to", "2", "--server", "s4"]))
            .expect("must parse");
    let pipe_alias = parse_block_args(&os(&[
        "pipe",
        "--from",
        "4",
        "--to",
        "2",
        "--session",
        "s4",
    ]))
    .expect("must parse");
    assert_eq!(pipe_server.session, pipe_alias.session);
    assert_eq!(pipe_server.session.as_deref(), Some("s4"));

    let ann_server = parse_block_annotate(&os(&[
        "annotate", "--from", "3", "--server", "s5", "--node", "n1", "-m", "hi",
    ]))
    .expect("must parse");
    let ann_alias = parse_block_annotate(&os(&[
        "annotate",
        "--from",
        "3",
        "--session",
        "s5",
        "--node",
        "n1",
        "-m",
        "hi",
    ]))
    .expect("must parse");
    assert_eq!(ann_server.session, ann_alias.session);
    assert_eq!(ann_server.session.as_deref(), Some("s5"));
}
