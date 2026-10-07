//! Contracts for `fno config setup run`, replacing the deleted Python
//! wizard tests (cli/tests/unit/test_setup_wizard.py): the argv claim, the
//! no-prompt decision, the step table's askability, the report shape, the
//! markers, and the recommendation shaping.

use super::*;

fn oss(pieces: &[&str]) -> Vec<OsString> {
    pieces.iter().map(OsString::from).collect()
}

#[test]
fn classify_claims_run_and_leaves_its_siblings() {
    assert!(classify(&oss(&["config", "setup", "run", "--yes"])).is_some());
    assert!(classify(&oss(&["config", "setup", "run"])).is_some());
    // The siblings stay with their owners.
    assert!(classify(&oss(&["config", "setup", "auto-wire"])).is_none());
    assert!(classify(&oss(&["config", "setup", "plan"])).is_none());
    assert!(classify(&oss(&["config", "setup", "wizard"])).is_none());
    assert!(classify(&oss(&["config", "setup"])).is_none());
}

#[test]
fn parse_opts_defaults_take_the_whole_run() {
    let o = parse_opts(&oss(&[])).unwrap();
    assert!(!o.yes);
    assert!(!o.once);
    assert!(!o.list);
    assert!(!o.json);
    assert_eq!(o.scope, Scope::Both);
    assert!(o.only.is_empty());
}

#[test]
fn parse_opts_reads_every_flag() {
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
}

#[test]
fn parse_opts_refuses_unknown_flags_and_scopes() {
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
fn every_step_is_askable_and_uniquely_named() {
    let mut ids: Vec<&str> = STEPS.iter().map(|s| s.id).collect();
    let n = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), n, "duplicate step id");
    for s in STEPS {
        assert!(!s.question.is_empty());
        assert!(!s.effect.is_empty());
        // Report and human steps never mutate: their kind says so up front.
        assert!(matches!(s.kind, Kind::Act | Kind::Report | Kind::Human));
    }
    // Both layers are always offered: the plan forbids a choose-one.
    assert!(STEPS.iter().any(|s| s.layer == Layer::Global));
    assert!(STEPS.iter().any(|s| s.layer == Layer::Project));
}

#[test]
fn only_filter_selects_known_ids_and_refuses_unknown_ones() {
    let mut opts = Opts::default();
    opts.only = vec!["gh-auth".into(), "backlog-prefix".into()];
    let steps = select_steps(&opts, Path::new(".")).unwrap();
    assert_eq!(steps.len(), 2);
    let mut bad = Opts::default();
    bad.only = vec!["no-such-step".into()];
    let err = select_steps(&bad, Path::new(".")).unwrap_err();
    assert!(err.contains("no-such-step"));
}

#[test]
fn scope_filters_layers() {
    let mut global = Opts::default();
    global.scope = Scope::Global;
    assert!(select_steps(&global, Path::new("."))
        .unwrap()
        .iter()
        .all(|s| s.layer == Layer::Global));
}

#[test]
fn the_report_carries_exactly_the_five_fields() {
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
    assert_eq!(v["done"].as_array().unwrap().len(), 1);
}

#[test]
fn wire_lines_fold_by_their_status_words() {
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
    assert!(rep.restart_needed, "an install must ask for a restart");
}

#[test]
fn marker_names_live_inside_owned_dirs() {
    // The global marker rides the already-inventoried sidecar/ subfolder:
    // the state root itself grows no new row (the freeze gate).
    assert_eq!(global_marker().file_name().unwrap(), "setup-done");
    assert_eq!(
        global_marker().parent().unwrap().file_name().unwrap(),
        "sidecar"
    );
    // The project marker sits beside the repo's config, not in the state root.
    let root = Path::new("/tmp/some-repo");
    assert_eq!(
        project_marker(root),
        Path::new("/tmp/some-repo/.fno/setup.done")
    );
}

#[test]
fn the_marker_line_carries_version_and_utc_stamp() {
    let line = marker_line();
    assert!(line.starts_with("fno "));
    assert!(line.contains(env!("CARGO_PKG_VERSION")));
    // YYYY-MM-DDTHH:MM:SSZ
    assert_eq!(line.len(), 4 + env!("CARGO_PKG_VERSION").len() + 1 + 20);
    assert_eq!(&line[line.len() - 20..line.len() - 18], "20");
    assert!(line.ends_with('Z'));
    assert_eq!(
        line.as_bytes()[4 + env!("CARGO_PKG_VERSION").len() + 11],
        b'T'
    );
}

#[test]
fn the_utc_stamp_is_iso_shaped() {
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
fn slug_prefix_shapes_the_repo_name() {
    assert_eq!(slug_prefix(Path::new("/tmp/My-Repo_X")), "myrepox");
    // Capped at 7: footnote itself yields footnot.
    assert_eq!(slug_prefix(Path::new("/home/x/footnote")), "footnot");
    assert_eq!(slug_prefix(Path::new("/")), "");
}

#[test]
fn in_source_checkout_needs_the_fno_crate_manifest() {
    let dir = std::env::temp_dir().join(format!("fno-setup-run-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("crates").join("fno")).unwrap();
    assert!(!in_source_checkout(&dir), "no crate manifest yet");
    std::fs::write(
        dir.join("crates").join("fno").join("Cargo.toml"),
        "[package]\n",
    )
    .unwrap();
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
