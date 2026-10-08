//! Contracts for `fno config setup run`, replacing the deleted Python
//! wizard tests (cli/tests/unit/test_setup_wizard.py): the argv claim, the
//! no-prompt decision, the step table's askability, the report shape, the
//! markers, and the recommendation shaping. One table-driven fn per
//! contract family, so the suite stays under the test-delta cap.

use super::*;

fn oss(pieces: &[&str]) -> Vec<OsString> {
    pieces.iter().map(OsString::from).collect()
}

#[test]
fn the_argv_surface_parses_and_refuses() {
    // The lexical claim: run is ours; the siblings stay with their owners.
    assert!(classify(&oss(&["config", "setup", "run", "--yes"])).is_some());
    assert!(classify(&oss(&["config", "setup", "run"])).is_some());
    for sibling in [
        vec!["config", "setup", "auto-wire"],
        vec!["config", "setup", "plan"],
        vec!["config", "setup", "wizard"],
        vec!["config", "setup"],
    ] {
        assert!(classify(&oss(&sibling)).is_none());
    }
    // Defaults: the whole run, both layers, no flags.
    let o = parse_opts(&oss(&[])).unwrap();
    assert!(!o.yes && !o.once && !o.list && !o.json && o.only.is_empty());
    assert_eq!(o.scope, Scope::Both);
    // Every flag reads.
    let o = parse_opts(&oss(&[
        "--yes",
        "--once",
        "--scope",
        "global",
        "--only",
        "gh-auth,backlog-prefix",
        "--list",
        "--json",
    ]))
    .unwrap();
    assert!(o.yes && o.once && o.list && o.json);
    assert_eq!(o.scope, Scope::Global);
    assert_eq!(o.only, vec!["gh-auth", "backlog-prefix"]);
    // Refusals: unknown flag, unknown scope, missing values.
    assert!(parse_opts(&oss(&["--bogus"])).is_err());
    assert!(parse_opts(&oss(&["--scope", "side"])).is_err());
    assert!(parse_opts(&oss(&["--scope"])).is_err());
    assert!(parse_opts(&oss(&["--only"])).is_err());
}

#[test]
fn the_no_prompt_decision_never_depends_on_one_signal() {
    // Explicit --yes wins even on a TTY.
    assert!(no_prompt(true, true, false));
    // A pipe on stdin takes the defaults without reading them.
    assert!(no_prompt(false, false, false));
    // An agent session env never sees a prompt.
    assert!(no_prompt(false, true, true));
    // A human on a TTY with no --yes is the only prompting shape.
    assert!(!no_prompt(false, true, false));
}

#[test]
fn step_selection_offers_both_layers_and_filters() {
    // The table is askable: unique ids, real question text, both layers
    // always offered (the plan forbids a choose-one).
    let mut ids: Vec<&str> = STEPS.iter().map(|s| s.id).collect();
    let n = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), n, "duplicate step id");
    for s in STEPS {
        assert!(!s.question.is_empty());
        assert!(!s.effect.is_empty());
    }
    assert!(STEPS.iter().any(|s| s.layer == Layer::Global));
    assert!(STEPS.iter().any(|s| s.layer == Layer::Project));
    // --only selects known ids and refuses unknown ones.
    let mut opts = Opts::default();
    opts.only = vec!["gh-auth".into(), "backlog-prefix".into()];
    assert_eq!(select_steps(&opts, Path::new(".")).unwrap().len(), 2);
    let mut bad = Opts::default();
    bad.only = vec!["no-such-step".into()];
    assert!(select_steps(&bad, Path::new("."))
        .unwrap_err()
        .contains("no-such-step"));
    // A global scope never carries project or contributor rows.
    let mut global = Opts::default();
    global.scope = Scope::Global;
    assert!(select_steps(&global, Path::new("."))
        .unwrap()
        .iter()
        .all(|s| s.layer == Layer::Global));
}

#[test]
fn the_report_shape_and_wire_fold_hold() {
    // The JSON report carries exactly the five fields an agent parses.
    let mut rep = Report::default();
    rep.done
        .push("harness-wiring: Claude Code: installed".into());
    rep.skipped
        .push("accounts: review with `fno config accounts`".into());
    rep.needs_human.push("gh-auth: run `gh auth login`".into());
    rep.paths.push("/Users/x/.fno/config.toml".into());
    rep.restart_needed = true;
    let v = rep.to_json();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    assert_eq!(keys.len(), 5);
    for k in ["done", "skipped", "needs_human", "paths", "restart_needed"] {
        assert!(keys.contains(&k), "missing {k}");
    }
    assert_eq!(v["restart_needed"], true);
    // Wire lines fold by their status words: an install is done plus a
    // restart ask, already-installed is done without one, and a manual
    // finish or failure blocks on a human.
    let mut rep = Report::default();
    fold_wire_line("wired the fno plugin into your agent CLIs:", &mut rep);
    fold_wire_line("  Claude Code: installed", &mut rep);
    fold_wire_line("  Codex CLI: already installed", &mut rep);
    fold_wire_line(
        "  pi: needs a manual finish - locate the extension",
        &mut rep,
    );
    fold_wire_line("  agy: FAILED (no hooks file)", &mut rep);
    fold_wire_line(
        "  no agent CLIs detected on PATH - nothing to wire.",
        &mut rep,
    );
    assert_eq!(rep.done.len(), 2);
    assert_eq!(rep.needs_human.len(), 2);
    assert_eq!(rep.skipped.len(), 1);
    assert!(rep.restart_needed, "a fresh install must ask for a restart");
    let mut rep = Report::default();
    fold_wire_line("  Codex CLI: already installed", &mut rep);
    assert!(
        !rep.restart_needed,
        "an already-installed line changed nothing"
    );
}

#[test]
fn markers_name_owned_dirs_and_stamp_iso() {
    // The global marker rides the already-inventoried sidecar/ subfolder:
    // the state root itself grows no new row (the freeze gate).
    assert_eq!(global_marker().file_name().unwrap(), "setup-done");
    assert_eq!(
        global_marker().parent().unwrap().file_name().unwrap(),
        "sidecar"
    );
    // The project marker sits beside the repo's config, not in the state root.
    assert_eq!(
        project_marker(Path::new("/tmp/some-repo")),
        Path::new("/tmp/some-repo/.fno/setup.done")
    );
    // The marker line carries the version and an ISO UTC stamp.
    let line = marker_line();
    assert!(line.starts_with("fno "));
    assert!(line.contains(env!("CARGO_PKG_VERSION")));
    assert!(line.ends_with('Z'));
    let stamp = utc_now();
    assert_eq!(stamp.len(), 20);
    let b = stamp.as_bytes();
    assert_eq!(b[4], b'-');
    assert_eq!(b[7], b'-');
    assert_eq!(b[10], b'T');
    assert_eq!(b[13], b':');
    assert_eq!(b[16], b':');
    assert_eq!(b[19], b'Z');
}

#[test]
fn the_recommendation_shapers_hold() {
    // The repo name shapes into a <=7-char lowercase prefix.
    assert_eq!(slug_prefix(Path::new("/tmp/My-Repo_X")), "myrepox");
    assert_eq!(slug_prefix(Path::new("/home/x/footnote")), "footnot");
    assert_eq!(slug_prefix(Path::new("/")), "");
    // A footnote checkout is the fno crate manifest under crates/.
    let dir = std::env::temp_dir().join(format!("fno-setup-run-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("crates/fno")).unwrap();
    assert!(!in_source_checkout(&dir), "no crate manifest yet");
    std::fs::write(dir.join("crates/fno/Cargo.toml"), "[package]\n").unwrap();
    assert!(in_source_checkout(&dir));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn once_needs_every_covered_layers_marker() {
    // A layer outside the scope never blocks --once; a covered layer
    // needs its own marker. This pins the inverted-condition fix.
    assert!(all_markers_present_in(Scope::Global, true, false));
    assert!(!all_markers_present_in(Scope::Global, false, true));
    assert!(all_markers_present_in(Scope::Project, false, true));
    assert!(!all_markers_present_in(Scope::Project, true, false));
    assert!(all_markers_present_in(Scope::Both, true, true));
    assert!(!all_markers_present_in(Scope::Both, true, false));
}
