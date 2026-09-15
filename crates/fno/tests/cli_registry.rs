//! The native front door's registry gate (x-861c, AC1-REGISTRY/AC1-FORWARD):
//! the typed root tree carries help on every canonical flag, and the
//! Python-forwarding boundary stays byte-verbatim, non-UTF-8 included.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

use clap::CommandFactory;

use fno::cli_args;

#[test]
fn root_tree_has_help_and_no_duplicates() {
    let cmd = fno::cli_args::FnoRoot::command();
    let mut seen = std::collections::BTreeSet::new();
    for arg in cmd.get_arguments() {
        if let Some(long) = arg.get_long() {
            assert!(seen.insert(long.to_string()), "--{long} declared twice");
            if arg.is_hide_set() {
                continue;
            }
            let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
            assert!(!help.trim().is_empty(), "--{long}: empty help");
        }
    }
}

#[test]
fn the_python_surface_forwards_byte_verbatim() {
    // Non-UTF-8 payload inside a forwarded tail must survive as raw bytes,
    // never a parse error (AC1-FORWARD).
    let argv = vec![
        OsString::from("backlog"),
        OsString::from("list"),
        OsString::from_vec(vec![0xff]),
    ];
    assert_eq!(cli_args::classify(&argv), cli_args::FrontDoor::Forward);
}

#[test]
fn typed_native_verbs_parse_and_refuse() {
    assert_eq!(
        cli_args::classify(&[OsString::from("version"), OsString::from("--json")]),
        cli_args::FrontDoor::Version { json: true }
    );
    // A malformed native shape is usage, never a forward (AC3-ERR): only
    // `mux ...` rides the carry arm.
    assert_eq!(
        cli_args::classify(&[OsString::from("--server")]),
        cli_args::FrontDoor::Usage
    );
}

#[test]
fn pane_run_payload_stays_verbatim() {
    // The payload after `--` (or the first bare token) is the spawned
    // command's argv; a common-flag spelling inside it is never ours to
    // parse. A fence-less MuxCommon::take across the whole argv stole them
    // (round-2 finding), silently trimming the spawned command's flags.
    use fno::mux_cli::{parse_pane_args, PaneCmd};
    let argv: Vec<OsString> = ["run", "--", "true", "--server", "x"]
        .iter()
        .map(OsString::from)
        .collect();
    let parsed = parse_pane_args(&argv).expect("payload parses");
    assert!(parsed.session.is_none());
    assert!(!parsed.json);
    match parsed.cmd {
        PaneCmd::Run { argv, .. } => assert_eq!(
            argv,
            vec!["true".to_string(), "--server".to_string(), "x".to_string()]
        ),
        other => panic!("run expected, got {other:?}"),
    }
    // The flags BEFORE the payload still work, and a payload `--json` does
    // not leak into the pane verb's own output flag.
    let argv: Vec<OsString> = ["run", "--json", "--", "claude", "--json"]
        .iter()
        .map(OsString::from)
        .collect();
    let parsed = parse_pane_args(&argv).expect("pre-payload flags parse");
    assert!(parsed.json);
    assert!(parsed.session.is_none());
    match parsed.cmd {
        PaneCmd::Run { argv, .. } => {
            assert_eq!(argv, vec!["claude".to_string(), "--json".to_string()])
        }
        other => panic!("run expected, got {other:?}"),
    }
}
