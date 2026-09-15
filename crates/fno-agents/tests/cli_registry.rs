//! The structural registry gate (x-861c, AC5-RATCHET): the migrated verbs'
//! flags stay declared once with nonblank help, and the hand-rolled scanners
//! they replaced stay gone. A regression that reintroduces a `flag_value`
//! helper, drops a help string, or breaks the Python adapter's exact argv
//! fails here before CI's heavy suites run.

use clap::{CommandFactory, Parser};

use fno_agents::cli_args::{RestartArgs, ReviewSummaryArgs, ScratchArgs};

/// The files whose scanners retired with this port; the strings below are
/// the definitions themselves.
const SCANNED_SOURCES: &[(&str, &str)] = &[
    ("scratch.rs", include_str!("../src/scratch.rs")),
    (
        "review_summary.rs",
        include_str!("../src/review_summary.rs"),
    ),
    ("spawn_overlay.rs", include_str!("../src/spawn_overlay.rs")),
];

#[test]
fn registry_trees_have_help_and_no_duplicates() {
    for (path, cmd) in [
        ("fno-agents restart", RestartArgs::command()),
        ("fno-agents scratch", ScratchArgs::command()),
        ("fno-agents review-summary", ReviewSummaryArgs::command()),
    ] {
        let mut seen = std::collections::BTreeSet::new();
        for arg in cmd.get_arguments() {
            if let Some(long) = arg.get_long() {
                assert!(
                    seen.insert(long.to_string()),
                    "{path} declares --{long} twice"
                );
                if arg.is_hide_set() {
                    continue;
                }
                let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
                assert!(
                    !help.trim().is_empty(),
                    "{path} --{long}: empty help; the registry help is the only copy"
                );
            }
        }
        for sub in cmd.get_subcommands() {
            assert!(
                sub.get_about()
                    .map(|a| !a.to_string().trim().is_empty())
                    .unwrap_or(false),
                "{path} {}: empty about",
                sub.get_name()
            );
        }
    }
}

#[test]
fn the_retired_scanners_stay_gone() {
    for (file, source) in SCANNED_SOURCES {
        for line in source.lines() {
            let trimmed = line.trim_start();
            let is_def = trimmed.starts_with("fn flag_value(")
                || trimmed.starts_with("pub fn flag_value(")
                || trimmed.starts_with("fn has_flag(")
                || trimmed.starts_with("macro_rules! flag_value");
            assert!(!is_def, "{file} redefines a retired scanner: {trimmed}");
        }
    }
}

#[test]
fn the_python_adapters_restart_argv_parses() {
    // x-67b8: this exact argv (post-verb) is what cli/src/fno/restart.py
    // spawns; a parser that refuses it breaks every daemon swap.
    let a = RestartArgs::try_parse_from(["--json", "--force"]).expect("adapter argv parses");
    assert!(a.json.json);
    assert!(a.force);
}

#[test]
fn the_real_binary_refuses_unknown_restart_flags_before_any_daemon_contact() {
    // Side-effect-free real-binary proof that the typed parser owns restart:
    // an unknown flag refuses at exit 2, one command-qualified line, before
    // any daemon contact, so the probe cannot restart anything.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .args(["restart", "--json", "--definitely-not-a-flag"])
        .envs(fno_agents::test_run::self_owner_env())
        .output()
        .expect("binary spawns");
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.starts_with("fno-agents restart: "), "{err}");
    assert!(err.contains("--definitely-not-a-flag"), "{err}");
}
