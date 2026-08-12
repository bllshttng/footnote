//! `fno mux workspace prune` end-to-end (x-a572 task 1.2): execs the real compiled
//! binary against a crafted store and asserts the dead-origin residue is reaped
//! while named and surviving-origin squads stay. Also proves the build-tree
//! write guard (task 1.1, AC2-HP) refuses the prune's OWN write when
//! FNO_AGENTS_HOME is unset - the exec'd binary is the arm #[cfg(test)] cannot
//! protect.
//!
//! Every case drives the binary as a SUBPROCESS with `env_clear`, so the test
//! process env is never mutated (no cross-test race, no mutex needed). Exit
//! codes come off `Command::status`, never through a pipe.

use std::process::Command;

fn fno() -> Command {
    Command::new(env!("CARGO_BIN_EXE_fno"))
}

/// A scratch agents-home with a `squads.json` seeded from `squads_json`, an
/// empty mux dir (so the pane probe finds nothing), no registry/roster (so the
/// live set is empty - every member is provably dead), and a real
/// `survives-origin` dir substituted into the template for the surviving-origin
/// case.
struct Scratch {
    dir: std::path::PathBuf,
}

impl Scratch {
    fn new(label: &str, squads_json: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("fno-prune-e2e-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("mux")).unwrap();
        std::fs::create_dir_all(dir.join("survives-origin")).unwrap();
        let json = squads_json.replace(
            "__SURVIVES__",
            dir.join("survives-origin")
                .to_str()
                .expect("utf8 scratch path"),
        );
        std::fs::write(dir.join("squads.json"), json).unwrap();
        Scratch { dir }
    }

    /// Run `fno mux workspace ...` with a hermetic env: HOME + FNO_AGENTS_HOME at
    /// the scratch, an empty mux dir, and `unset_agents` controlling
    /// FNO_AGENTS_HOME (true -> the build-tree guard must refuse the write).
    fn run(&self, squad_args: &[&str], unset_agents: bool) -> (bool, String, String) {
        self.run_family("workspace", squad_args, unset_agents)
    }

    /// The same run, with the family spelled explicitly, so the removed `squad`
    /// spelling can be driven through the identical harness and shown to refuse.
    fn run_family(
        &self,
        family: &str,
        squad_args: &[&str],
        unset_agents: bool,
    ) -> (bool, String, String) {
        let mut cmd = fno();
        cmd.args(["mux", family]).args(squad_args);
        cmd.env_clear()
            .env("HOME", &self.dir)
            .env("FNO_MUX_DIR", self.dir.join("mux"));
        if !unset_agents {
            cmd.env("FNO_AGENTS_HOME", &self.dir);
        }
        let out = cmd.output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    fn store(&self) -> String {
        std::fs::read_to_string(self.dir.join("squads.json")).unwrap_or_default()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const ORPHAN_NAMED_SURVIVING: &str = r#"{"version":1,"squads":[
  {"name":"","key":"orphan","origins":["/no/such/prune-e2e"],"members":[{"attach_id":"deadbeef","tombstone":false}],"created_at":""},
  {"name":"work","key":"","origins":["/no/such/prune-e2e"],"members":[{"attach_id":"deadbeef","tombstone":false}],"created_at":""},
  {"name":"","key":"survives","origins":["__SURVIVES__"],"members":[{"attach_id":"deadbeef","tombstone":false}],"created_at":""}
]}"#;

#[test]
fn prune_reaps_orphans_keeps_named_and_surviving_origin() {
    let s = Scratch::new("reap", ORPHAN_NAMED_SURVIVING);

    let (ok, stdout, stderr) = s.run(&["prune"], false);
    assert!(
        ok,
        "prune exited non-zero: {stderr}\n--- stdout ---\n{stdout}"
    );
    assert!(
        stdout.contains("pruned <key:orphan>"),
        "receipt names the pruned orphan: {stdout}"
    );
    assert!(
        stdout.contains("skipped 1 named"),
        "the named squad is counted skip-named: {stdout}"
    );

    let after = s.store();
    assert!(!after.contains("orphan"), "orphan removed: {after}");
    assert!(
        after.contains("\"name\": \"work\""),
        "named squad kept: {after}"
    );
    assert!(
        after.contains("\"key\": \"survives\""),
        "surviving-origin squad kept: {after}"
    );
}

#[test]
fn prune_dry_run_writes_nothing() {
    let s = Scratch::new("dryrun", ORPHAN_NAMED_SURVIVING);
    let before = s.store();

    let (ok, stdout, stderr) = s.run(&["prune", "--dry-run"], false);
    assert!(ok, "dry-run exited non-zero: {stderr}\n{stdout}");
    assert!(
        stdout.contains("would prune <key:orphan>"),
        "dry-run labels lines: {stdout}"
    );
    assert!(
        stdout.contains("dry-run: no changes written"),
        "dry-run trailer: {stdout}"
    );
    assert_eq!(s.store(), before, "dry-run must not change the store");
}

/// The retired `mux squad` spelling is removed, and reaching for it lands on
/// the usage banner rather than a prune.
///
/// Asserted as a POSITIVE marker (exit 2 plus the banner naming the canonical
/// spelling), not as "the prune did not happen": a prune that silently no-ops
/// for an unrelated reason also fails to happen, and the two are
/// indistinguishable from an absence.
#[test]
fn retired_squad_spelling_is_removed() {
    let s = Scratch::new("alias", ORPHAN_NAMED_SURVIVING);
    let before = s.store();

    let (ok, _stdout, stderr) = s.run_family("squad", &["prune", "--json"], false);
    assert!(!ok, "the retired spelling must not succeed: {stderr}");
    assert!(
        stderr.contains("fno mux workspace prune"),
        "usage must name the surviving spelling: {stderr}"
    );
    assert_eq!(s.store(), before, "a usage exit must not touch the store");

    // Positive control: the canonical spelling still prunes, so the assertion
    // above is about the alias and not about a broken fixture.
    let (ok, got, stderr) = s.run_family(
        "workspace",
        &["prune", "--dry-run", "--include-named", "--json"],
        false,
    );
    assert!(ok, "canonical spelling exited non-zero: {stderr}\n{got}");
    assert!(
        got.contains("\"pruned_count\":2"),
        "the fixture must actually prune something: {got}"
    );
}

/// A bare family verb and an unknown verb are both usage (exit 2), and the
/// message names the one verb that exists. The two take DIFFERENT paths and the
/// assertions say which: a bare `mux workspace` fails main.rs's arity guard and
/// lands on the global usage banner, while an unknown verb reaches the family
/// parser. Asserting only "names prune" cannot tell them apart, because the
/// global banner also contains the word.
#[test]
fn workspace_family_usage_names_prune() {
    for (args, label, want) in [
        (
            vec!["mux", "workspace"],
            "bare",
            "fno mux workspace prune [",
        ),
        (
            vec!["mux", "workspace", "bogus"],
            "unknown verb",
            "fno mux workspace: unknown verb",
        ),
    ] {
        let out = fno().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{label} is usage");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(want),
            "{label} should emit {want:?}: {stderr}"
        );
    }
}

/// The removed spelling never returns to the usage banner. Without this,
/// re-adding `squad` is a silent un-deprecation every other test stays green
/// through.
#[test]
fn retired_squad_spelling_is_not_advertised() {
    let out = fno().args(["mux", "workspace"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("fno mux workspace prune"),
        "usage advertises the canonical spelling: {stderr}"
    );
    assert!(
        !stderr.contains("mux squad"),
        "usage must not advertise the retired spelling: {stderr}"
    );
}

/// A dry run whose read already touched the filesystem must say so, on stderr
/// AND in the receipt. Reporting "no changes written" while `load()` had just
/// renamed the store aside is the one lie a dry run cannot tell, and a scripted
/// caller capturing only stdout is the likeliest to miss the correction.
#[test]
fn prune_dry_run_reports_a_quarantined_store() {
    let s = Scratch::new("corrupt", "{not json at all");
    let (ok, stdout, stderr) = s.run(&["prune", "--dry-run", "--json"], false);
    assert!(ok, "dry-run over a corrupt store must not refuse: {stderr}");
    assert!(
        stderr.contains("quarantined corrupt squads.json"),
        "stderr names the quarantine: {stderr}"
    );
    assert!(
        stdout.contains("\"notice\":\"quarantined corrupt squads.json"),
        "the receipt carries the notice, not just stderr: {stdout}"
    );
}

/// Unreadable is not missing. Non-UTF-8 content fails `read_to_string` before
/// the corrupt-JSON quarantine can see it, and collapsing that into "fresh"
/// made prune report an empty store it had never actually read.
#[test]
fn prune_dry_run_reports_an_unreadable_store() {
    let s = Scratch::new("unreadable", r#"{"version":1,"squads":[]}"#);
    std::fs::write(s.dir.join("squads.json"), [0x66, 0x6e, 0xff, 0xfe, 0x6f]).unwrap();
    let (ok, stdout, stderr) = s.run(&["prune", "--dry-run", "--json"], false);
    assert!(
        ok,
        "dry-run over an unreadable store must not refuse: {stderr}"
    );
    assert!(
        stdout.contains("\"notice\":\"could not read squads.json"),
        "an unreadable store is reported, never silently empty: {stdout}"
    );
}

/// A clean store leaves the receipt's notice explicitly null, so a consumer can
/// tell "nothing to report" from "this build predates the field".
#[test]
fn prune_receipt_notice_is_null_on_a_clean_store() {
    let s = Scratch::new("nonotice", ORPHAN_NAMED_SURVIVING);
    let (ok, stdout, stderr) = s.run(&["prune", "--dry-run", "--json"], false);
    assert!(ok, "clean dry-run exited non-zero: {stderr}\n{stdout}");
    assert!(
        stdout.contains("\"notice\":null"),
        "clean store reports a null notice: {stdout}"
    );
}

#[test]
fn prune_empty_store_says_nothing_to_prune() {
    let s = Scratch::new("empty", r#"{"version":1,"squads":[]}"#);
    let (ok, stdout, stderr) = s.run(&["prune"], false);
    assert!(ok, "empty prune exited non-zero: {stderr}\n{stdout}");
    assert!(
        stdout.contains("nothing to prune"),
        "empty store is a clean no-op: {stdout}"
    );
}

#[test]
fn prune_include_named_removes_a_named_orphan() {
    // AC1-EDGE: --include-named reaps a named squad whose origins are all gone.
    let s = Scratch::new(
        "named",
        r#"{"version":1,"squads":[
          {"name":"stale","key":"","origins":["/no/such"],"members":[{"attach_id":"deadbeef","tombstone":false}],"created_at":""}
        ]}"#,
    );
    let (ok, stdout, stderr) = s.run(&["prune", "--include-named"], false);
    assert!(
        ok,
        "include-named prune exited non-zero: {stderr}\n{stdout}"
    );
    assert!(
        stdout.contains("pruned stale"),
        "named orphan removed: {stdout}"
    );
    assert!(!s.store().contains("stale"), "named orphan gone from store");
}

#[test]
fn prune_json_envelope_names_the_removed_set() {
    // AC1-UI: --json emits the removed set machine-readably.
    let s = Scratch::new("json", ORPHAN_NAMED_SURVIVING);
    let (ok, stdout, stderr) = s.run(&["prune", "--json"], false);
    assert!(ok, "json prune exited non-zero: {stderr}\n{stdout}");
    assert!(
        stdout.contains("\"pruned_count\":1"),
        "json count: {stdout}"
    );
    assert!(
        stdout.contains("\"key\":\"orphan\""),
        "json names the removed orphan: {stdout}"
    );
    assert!(
        stdout.contains("\"dry_run\":false"),
        "a real prune reports dry_run false: {stdout}"
    );
}

/// The receipt's `dry_run` flag reports the mode the caller asked for. It was
/// fed the inverse (`applied`), so automation reading it saw every dry run as
/// a real prune and every real prune as a rehearsal.
#[test]
fn prune_json_dry_run_flag_matches_the_mode() {
    let s = Scratch::new("json-dryrun", ORPHAN_NAMED_SURVIVING);
    let (ok, stdout, stderr) = s.run(&["prune", "--dry-run", "--json"], false);
    assert!(ok, "dry-run json exited non-zero: {stderr}\n{stdout}");
    assert!(
        stdout.contains("\"dry_run\":true"),
        "a dry run reports dry_run true: {stdout}"
    );
    assert!(
        stdout.contains("\"pruned_count\":1"),
        "the dry run still names what it would remove: {stdout}"
    );
}

#[test]
fn prune_refuses_from_a_build_tree_binary_without_agents_home() {
    // AC2-HP, the exec'd-binary half: the compiled binary under target/ with
    // FNO_AGENTS_HOME unset must refuse its OWN prune write (exit non-zero,
    // refusal naming FNO_AGENTS_HOME). Exit code read off Command::status.
    let s = Scratch::new("guard", r#"{"version":1,"squads":[]}"#);
    let (ok, _stdout, stderr) = s.run(&["prune"], true);
    assert!(
        !ok,
        "a build-tree binary must refuse a prune without FNO_AGENTS_HOME"
    );
    assert!(
        stderr.contains("FNO_AGENTS_HOME"),
        "the refusal names the remedy: {stderr}"
    );
}

#[test]
fn doctor_reports_orphaned_squads_with_the_prune_remedy() {
    // AC3-UI: doctor's squad-store check renders a warn (exit stays 0) naming the
    // orphan count and the prune remedy. Read-only - it never prunes.
    let s = Scratch::new("doctor", ORPHAN_NAMED_SURVIVING);
    let out = fno()
        .args(["mux", "doctor"])
        .env_clear()
        .env("HOME", &s.dir)
        .env("FNO_AGENTS_HOME", &s.dir)
        .env("FNO_MUX_DIR", s.dir.join("mux"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "doctor exited non-zero (a warn stays 0): {stderr}\n{stdout}"
    );
    assert!(
        stdout.contains("squad store"),
        "doctor runs the squad-store check: {stdout}"
    );
    assert!(
        stdout.contains("orphaned"),
        "the orphan count is a warn: {stdout}"
    );
    assert!(
        stdout.contains("fno mux workspace prune"),
        "the remedy points at prune: {stdout}"
    );
    // The orphan was only DETECTED, not removed - doctor is read-only.
    assert!(
        s.store().contains("orphan"),
        "doctor did not mutate the store"
    );
}
