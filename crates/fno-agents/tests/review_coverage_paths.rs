//! Path-uniqueness enumeration for the `review_coverage` producer (x-3a3f).
//!
//! The AGENTS.md pitfalls entry says a guard on one of N reachable paths is
//! decorative; this node's defect was the inversion - the PRODUCER sat on one
//! of N paths, so the merge gate was unsatisfiable rather than bypassable for
//! every session shape that had no stop hook. This test enumerates every path
//! that reaches a coverage verdict and asserts, per row, either that running
//! the path produces a coverage event, or that the row is on an explicit
//! safe-direction allowlist with a reason. A new call site that starts reading
//! `review_coverage` without joining the table fails the source scan.

mod common;

use common::make_script;
use fno_agents::loopcheck::{run_loop_check_capture, run_review_coverage_capture};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const HEAD: &str = "deadbeefdeadbeefdeadbeefdeadbeef00000001";

/// One row of the reachability table (mirrors the table in
/// docs/architecture/review-lanes.md).
enum ProducerReach {
    /// Running the path emits a `review_coverage` event; asserted at runtime
    /// below (or, for the Python gates, by their own unit suites - the source
    /// scan pins that the call site exists).
    Reachable,
    /// The path deliberately does NOT get a producer, for the stated reason.
    SafeDirection(&'static str),
}

fn path_table() -> Vec<(&'static str, &'static str, ProducerReach)> {
    vec![
        (
            "stop hook decide() -> run_done",
            "streak-gated; unchanged",
            ProducerReach::Reachable,
        ),
        (
            "fno do pr merge <n>",
            "recompute-once in _review_coverage_for_pr",
            ProducerReach::Reachable,
        ),
        (
            "fno do pr status <n>",
            "read through the shared recompute helper",
            ProducerReach::Reachable,
        ),
        (
            "finalize auto-merge arm (coverage_satisfied_in_latest_event)",
            "reached only from a terminal-allow, which implies run_done already \
             ran this fire; a failed arm leaves a green reviewed PR for a human, \
             which is the safe direction",
            ProducerReach::SafeDirection("cannot be starved: terminal-allow implies run_done ran"),
        ),
        (
            "fno do pr coverage-check <n> without --recompute (and the git-protection hook through it)",
            "default is the no-recompute read: a recompute shells the Rust producer \
             at minutes while a PreToolUse hook has a 60s budget, and a killed hook \
             emits no verdict at all. --recompute is producer-reachable, same as \
             `fno do pr merge`",
            ProducerReach::SafeDirection(
                "one-directional by design: on a missing or stale row this DENIES \
                 where `fno do pr merge` may yet allow after recomputing. An empty \
                 read is an answer and refuses. Recovery from a wrong deny is one \
                 command; from a wrong allow it is a revert",
            ),
        ),
        (
            "a human running the verb by hand",
            "fno-agents review-coverage",
            ProducerReach::Reachable,
        ),
    ]
}

fn writer_return_table() -> Vec<(&'static str, &'static str)> {
    vec![
        ("invalid status target", "not posting for invalid target"),
        ("no configured review lane", "no review lane configured"),
        ("coverage row describes a different head", "event describes"),
        (
            "live override label posts its own verdict",
            "coverage-override label applied on the PR",
        ),
        (
            "unreadable label protects an existing override status",
            "protected existing coverage-override marker",
        ),
        (
            "covered predicate has no counted coverage variant",
            "covered predicate had no Covered count",
        ),
    ]
}

#[test]
fn every_coverage_status_return_has_a_positive_marker() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(manifest_dir.join("src/loopcheck.rs")).unwrap();
    let start = source.find("fn publish_coverage_status(").unwrap();
    let end = source[start..]
        .find("/// The give-up line")
        .map(|offset| start + offset)
        .unwrap();
    let publisher = &source[start..end];
    let table = writer_return_table();

    assert_eq!(
        publisher.matches("return;").count(),
        table.len(),
        "a publisher return was added or removed without updating the writer-path table"
    );
    for (path, marker) in table {
        assert!(
            !marker.is_empty(),
            "writer path '{path}' has no positive marker"
        );
        assert!(
            publisher.contains(marker),
            "writer path '{path}' does not emit or post its marker '{marker}'"
        );
    }
}

fn coverage_exists(path: &Path) -> bool {
    fs::read_to_string(path)
        .map(|t| {
            t.lines()
                .filter(|l| l.contains("review_coverage"))
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .any(|ev| ev.get("type").and_then(|t| t.as_str()) == Some("review_coverage"))
        })
        .unwrap_or(false)
}

fn fixture(parent: &Path, name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
    let cwd = parent.join(name);
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    fs::write(
        cwd.join(".fno/config.toml"),
        "[review]\nrequired_bots = [\"chatgpt-codex-connector\"]\n",
    )
    .unwrap();
    // NOT a TempDir: the guard must outlive this function, or the stubs
    // vanish and the availability probe reads NotFound -> advisory mode.
    let bins = parent.join(format!("{name}-bins"));
    fs::create_dir_all(&bins).unwrap();
    let gh = make_script(
        &bins,
        "gh",
        &format!(
            r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{{"state":"OPEN","number":1,"headRefName":"main","headRefOid":"{HEAD}","mergeable":"MERGEABLE","baseRefName":"main"}}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then echo '[{{"name":"ci","state":"SUCCESS","bucket":"pass"}}]'; exit 0; fi
if echo "$*" | grep -q "pulls/"; then echo '[]'; exit 0; fi
if echo "$*" | grep -q "reviews"; then echo '{{"reviews":[],"comments":[]}}'; exit 0; fi
exit 1"#
        ),
    );
    let git = make_script(
        &bins,
        "git",
        &format!(
            r#"case "$*" in
  *--raw*) exit 1 ;;
  *) echo "{HEAD}" ;;
esac"#
        ),
    );
    let project = cwd.join(".fno/events.jsonl");
    let global = parent.join(format!("{name}-global-events.jsonl"));
    (cwd, project, global, gh, git)
}

/// Every runtime-assertable reachable row produces a coverage event when its
/// path runs. The Python rows (merge/status) are exercised end-to-end by
/// cli/tests/unit/test_pr_merge.py and test_pr_status.py; the source scan
/// below pins their call sites to this table.
#[test]
fn every_reachable_path_produces_an_event() {
    let table = path_table();
    let mut proven = 0;
    for (path_name, _how, reach) in &table {
        match reach {
            ProducerReach::SafeDirection(why) => {
                assert!(
                    !why.is_empty(),
                    "safe-direction row {path_name} must carry a reason"
                );
            }
            ProducerReach::Reachable => {
                // Both Rust-drivable rows run here; the Python rows are
                // counted via the source-scan test so this loop stays honest
                // about which rows were RUN.
                match *path_name {
                    "stop hook decide() -> run_done" => {
                        let parent = TempDir::new().unwrap();
                        let (cwd, project, global, gh, git) = fixture(parent.path(), "stop-hook");
                        let manifest = cwd.join("target-state.md");
                        fs::write(
                            &manifest,
                            "---\nsession_id: s1\ncreated_at: 2026-08-14T00:00:00Z\nattended: true\n---\n",
                        )
                        .unwrap();
                        let transcript = cwd.join("transcript.jsonl");
                        fs::write(
                            &transcript,
                            serde_json::json!({"message": {"role": "assistant",
                                "content": "<promise>MISSION COMPLETE</promise>"}})
                            .to_string()
                                + "\n",
                        )
                        .unwrap();
                        std::env::set_var("FNO_NUDGE_DISABLED", "1");
                        std::env::set_var("FNO_LOOPCHECK_MIN_FIRE_GAP_SECS", "0");
                        let args: Vec<String> = vec![
                            "loop-check".into(),
                            format!("--state={}", manifest.display()),
                            format!("--transcript={}", transcript.display()),
                            format!("--cwd={}", cwd.display()),
                            format!("--gh-bin={}", gh.display()),
                            format!("--git-bin={}", git.display()),
                            format!("--events={}", project.display()),
                            format!("--global-events={}", global.display()),
                            "--global-settings".into(),
                            "/nonexistent/global-settings.yaml".into(),
                            "--author-harness".into(),
                            "none".into(),
                        ];
                        let (code, json) = run_loop_check_capture(&args);
                        assert_eq!(code, 0, "{json}");
                        assert!(
                            coverage_exists(&project) || coverage_exists(&global),
                            "stop-hook path emitted no coverage event; decision: {json}"
                        );
                        proven += 1;
                    }
                    "a human running the verb by hand" => {
                        let parent = TempDir::new().unwrap();
                        let (cwd, project, global, gh, git) = fixture(parent.path(), "verb");
                        let args: Vec<String> = vec![
                            "review-coverage".into(),
                            format!("--cwd={}", cwd.display()),
                            format!("--events={}", project.display()),
                            format!("--global-events={}", global.display()),
                            format!("--gh-bin={}", gh.display()),
                            format!("--git-bin={}", git.display()),
                            "--global-settings".into(),
                            "/nonexistent/global-settings.yaml".into(),
                            "--author-harness".into(),
                            "none".into(),
                        ];
                        let (code, json) = run_review_coverage_capture(&args);
                        assert_eq!(code, 0, "{json}");
                        assert!(
                            coverage_exists(&project) || coverage_exists(&global),
                            "verb path emitted no coverage event"
                        );
                        proven += 1;
                    }
                    python_row => {
                        // fno do pr merge / fno do pr status: their end-to-end proof
                        // lives in the Python suites; here we only acknowledge
                        // the row so adding a table row without a runtime arm
                        // in THIS test fails the coverage counter below.
                        assert!(
                            python_row.starts_with("fno do pr "),
                            "new reachable row {python_row} has no runtime arm"
                        );
                    }
                }
            }
        }
    }
    assert!(proven >= 2, "expected both Rust-drivable rows to run");
}

/// Every call site that READS the review_coverage event appears in the table
/// (or on the allowlist), and the Python gates actually route through the
/// recompute helper rather than around it. A new reader that joins without a
/// table row fails here.
#[test]
fn new_reader_sites_must_join_the_table() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .ancestors()
        .nth(2)
        .expect("crate sits at <repo>/crates/fno-agents");

    let table = path_table();
    let table_names: Vec<&str> = table.iter().map(|(n, _, _)| *n).collect();

    // Rust readers: scan the crate sources for reads of the event.
    for src in ["src/finalize.rs", "src/loopcheck.rs"] {
        let path = manifest_dir.join(src);
        let text = fs::read_to_string(&path).unwrap();
        if src == "src/finalize.rs" {
            // The allowlisted arm: it reads review_coverage and has no
            // producer, by decision (see the table's reason).
            assert!(
                text.contains("coverage_satisfied_in_latest_event"),
                "finalize's coverage arm moved; update the table"
            );
            assert!(
                table_names.iter().any(|n| n.contains("finalize")),
                "finalize's read has no table row"
            );
        }
    }

    // Python readers: the pr package is the consumer surface. Every file that
    // reads the event must be a table row, and the two gate rows must route
    // through the shared recompute helper (the verb invocation), not around it.
    let pr_dir = repo_root.join("cli/src/fno/pr");
    let mut reader_files: Vec<String> = Vec::new();
    for entry in fs::read_dir(&pr_dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".py") {
            continue;
        }
        let text = fs::read_to_string(entry.path()).unwrap();
        if text.contains("review_coverage_for_gate")
            || text.contains("review_coverage_for_head(")
            || text.contains("latest_review_coverage(")
            || text.contains("read_review_coverage(")
            || text.contains("_scan_coverage(")
        {
            reader_files.push(name);
        }
    }
    // _reviews.py owns the reader itself; _merge.py and _status.py are the
    // gate rows. Anything else reading the event is a NEW path: it must join
    // the table here (and get a producer or a safe-direction reason).
    for name in &reader_files {
        let known = match name.as_str() {
            "_reviews.py" => true, // the shared reader/helper itself
            // The merge guard's predicate, lifted out of `run_merge` unchanged so
            // `fno do pr merge` and `fno do pr coverage-check` ask it through one copy.
            // Whitelisted for the same reason `_reviews.py` is: it is the shared
            // body, not a new path. Unlike `_reviews.py`, its row is enforced:
            // exact equality, so no renamed or split sibling row satisfies the arm.
            "_coverage_gate.py" => table_names.iter().any(|n| {
                *n == "fno do pr coverage-check <n> without --recompute (and the git-protection hook through it)"
            }),
            "_merge.py" => table_names.iter().any(|n| *n == "fno do pr merge <n>"),
            "_status.py" => table_names.iter().any(|n| *n == "fno do pr status <n>"),
            _ => false,
        };
        assert!(
            known,
            "{name} reads review_coverage but has no row in the x-3a3f path table"
        );
    }
    assert!(
        reader_files.contains(&"_merge.py".to_string())
            && reader_files.contains(&"_status.py".to_string())
            && reader_files.contains(&"_coverage_gate.py".to_string()),
        "a gate row stopped reading coverage"
    );

    // And the gates route through the recompute: both files name the shared
    // helper, which is the only path to the verb. The status row passes
    // `recompute=review_lane` (not a bare True): the producer spawn is gated
    // on the same no-lane boundary merge reads, while the read itself still
    // goes through this helper.
    for (file, needle) in [
        ("_merge.py", "review_coverage_for_gate"),
        ("_status.py", "recompute=review_lane"),
    ] {
        let text = fs::read_to_string(pr_dir.join(file)).unwrap();
        assert!(
            text.contains(needle),
            "{file} stopped routing through the shared recompute helper"
        );
    }
    // The helper itself fires the verb - the producer's one Python doorway.
    let reviews = fs::read_to_string(pr_dir.join("_reviews.py")).unwrap();
    assert!(
        reviews.contains("\"review-coverage\""),
        "the recompute helper no longer invokes the review-coverage verb"
    );
}

/// Row-to-path: a row may not name a path that does not exist. The other two
/// tests prove reachable rows produce events and readers join the table; this
/// closes the third direction - a SafeDirection row naming a fictitious verb
/// or symbol passed both, because nothing checked the row's own path exists
/// (the PR 917 dual-review finding: a row named a verb that existed on main
/// but not on the branch the row was written for, and the drift-catcher could
/// never fire).
#[test]
fn every_table_row_names_a_path_that_exists() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .ancestors()
        .nth(2)
        .expect("crate sits at <repo>/crates/fno-agents");
    let cli = fs::read_to_string(repo_root.join("cli/src/fno/pr/cli.py")).unwrap();
    let rust_srcs: Vec<String> = ["src/loopcheck.rs", "src/finalize.rs"]
        .iter()
        .map(|s| fs::read_to_string(manifest_dir.join(s)).unwrap())
        .collect();

    for (path_name, _, _) in path_table() {
        if let Some(rest) = path_name.strip_prefix("fno do pr ") {
            let verb = rest.split_whitespace().next().unwrap_or("");
            assert!(
                !verb.is_empty() && cli.contains(&format!("\"{verb}\"")),
                "row '{path_name}' names verb '{verb}' that cli/src/fno/pr/cli.py does not register"
            );
            continue;
        }
        if let Some(open) = path_name.find('(') {
            let close = path_name.find(')').unwrap_or(path_name.len());
            let inner = &path_name[open + 1..close];
            // A named argument sits inside the parens ("(...symbol...)");
            // call syntax leaves them empty ("decide()"), so fall back to the
            // token immediately before the '(' - the callee's own name.
            let symbol = inner
                .split_whitespace()
                .next_back()
                .filter(|s| !s.is_empty())
                .or_else(|| path_name[..open].split_whitespace().next_back())
                .unwrap_or("");
            assert!(
                !symbol.is_empty() && rust_srcs.iter().any(|t| t.contains(symbol)),
                "row '{path_name}' names symbol '{symbol}' that no crate source defines"
            );
        }
    }
}
