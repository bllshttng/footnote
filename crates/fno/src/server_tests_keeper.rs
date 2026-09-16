//! The `fno mux pane keeper list` parser tests, moved out of the parent
//! under the file-budget gate's shrink-only rule for over-budget files.
#[test]
fn keeper_list_parses_as_a_hidden_pane_verb() {
    let parsed = crate::mux_cli::parse_pane_args(
        &crate::cli_args::PaneOp::Keeper {
            op: crate::cli_args::KeeperOp::List(crate::cli_args::MuxTail { tail: Vec::new() }),
        },
        &["--json".into()],
    )
    .expect("parses");
    assert_eq!(
        parsed.cmd,
        crate::mux_cli::PaneCmd::KeeperList {
            json: true,
            stale_after: None
        }
    );
    let parsed = crate::mux_cli::parse_pane_args(
        &crate::cli_args::PaneOp::Keeper {
            op: crate::cli_args::KeeperOp::List(crate::cli_args::MuxTail { tail: Vec::new() }),
        },
        &["--stale-after".into(), "24h".into()],
    )
    .expect("parses");
    assert_eq!(
        parsed.cmd,
        crate::mux_cli::PaneCmd::KeeperList {
            json: false,
            stale_after: Some(std::time::Duration::from_secs(86_400)),
        }
    );
    // Declared in the typed tree (the root menu stays the one advertisement
    // surface; the inventory artifact lists keeper as hidden).
    let pane_cmd = <crate::cli_args::FnoRoot as clap::CommandFactory>::command()
        .get_subcommands()
        .find(|c| c.get_name() == "mux")
        .and_then(|m| m.get_subcommands().find(|c| c.get_name() == "pane"))
        .expect("pane declared in the tree")
        .clone();
    assert!(pane_cmd.get_subcommands().any(|c| c.get_name() == "keeper"));
}
