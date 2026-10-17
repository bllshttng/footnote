//! Tests for the heal drive loop, in their own module: `heal.rs` sat pinned
//! at the file-budget cap and the starvation fix had no room to land. The
//! move is pure test motion; raw-string stub contents stay byte-exact and
//! the code lines shed the one indent level the brace nesting paid.

use super::*;
use crate::pr_push::{any_pending, rest_bucket};
use serde_json::json;
use std::path::Path;

/// A real `cargo fmt --check (pinned)` failure, timestamps and all.
const FMT_LOG: &str = concat!(
    "2026-09-03T03:06:02.1431957Z ##[group]Run cargo \"+$RUSTFMT_TOOLCHAIN\" fmt --all --check\n",
    "2026-09-03T03:06:03.2159034Z Diff in /home/runner/work/footnote/footnote/crates/fno/src/server.rs:17553:\n",
    "2026-09-03T03:06:03.2234373Z -            core.pane_send(\n",
    "2026-09-03T03:06:04.8929731Z ##[error]Process completed with exit code 1.\n",
);

fn ctx<'a>(check: &'a str, log: &'a str) -> Ctx<'a> {
    Ctx {
        check,
        log,
        bucket: "fail",
        link: "",
    }
}

fn cancelled_ctx<'a>(check: &'a str, log: &'a str) -> Ctx<'a> {
    Ctx {
        check,
        log,
        bucket: "cancel",
        link: "",
    }
}

fn cancelled_run_ctx<'a>(check: &'a str, link: &'a str) -> Ctx<'a> {
    Ctx {
        check,
        log: "",
        bucket: "cancel",
        link,
    }
}

#[test]
fn classify_rows() {
    let stripped = strip_timestamps(FMT_LOG);
    assert!(
        stripped.contains("Diff in /home/runner/work/footnote/footnote/crates/fno/src/"),
        "got: {stripped}"
    );
    assert!(!stripped.contains("2026-09-03T03:06"), "got: {stripped}");

    let log = strip_timestamps(FMT_LOG);
    let f = classify(&ctx("cargo fmt --check (pinned)", &log), false);
    assert_eq!(f.signature, "rustfmt-drift");
    match f.remedy {
        Remedy::Auto { run, verify } => {
            assert_eq!(run.len(), 1, "one crate drifted: {run:?}");
            assert_eq!(run[0].cwd, "crates/fno");
            assert_eq!(run[0].argv, vec!["cargo", "+1.94.1", "fmt", "--all"]);
            assert_eq!(verify[0].argv.last().unwrap(), "--check");
        }
        other => panic!("expected Auto, got {other:?}"),
    }

    let log = strip_timestamps(&format!(
        "{FMT_LOG}2026-09-03T03:06:03Z Diff in /home/runner/work/footnote/footnote/crates/fno-agents/src/heal.rs:1:\n"
    ));
    let f = classify(&ctx("cargo fmt --check (pinned)", &log), false);
    match f.remedy {
        Remedy::Auto { run, .. } => {
            let dirs: Vec<&str> = run.iter().map(|c| c.cwd.as_str()).collect();
            assert_eq!(dirs, vec!["crates/fno", "crates/fno-agents"]);
        }
        other => panic!("expected Auto, got {other:?}"),
    }

    let f = classify(&ctx("cargo fmt --check (pinned)", "log unavailable"), false);
    assert_eq!(f.signature, "rustfmt-drift");
    match f.remedy {
        Remedy::Auto { run, .. } => assert_eq!(run.len(), 2, "{run:?}"),
        other => panic!("expected Auto, got {other:?}"),
    }

    // The shape ruff actually prints: code first, location beneath.
    let log = strip_timestamps(concat!(
        "2026-09-03T07:15:29.7310174Z F821 Undefined name `Callable`\n",
        "2026-09-03T07:15:29.7310931Z    --> src/fno/lead/board.py:886:21\n",
        "2026-09-03T07:15:29.7322711Z Found 1 error.\n",
    ));
    let f = classify(&ctx("Python static correctness (495 sources)", &log), false);
    assert_eq!(f.signature, "ruff-lint");
    match f.remedy {
        Remedy::Auto { run, verify } => {
            assert_eq!(run.len(), 1, "one command, the gate's own: {run:?}");
            assert_eq!(run[0].cwd, "cli");
            assert!(run[0].argv.contains(&"--fix".to_string()), "{run:?}");
            // The gate reads src/ only and runs no formatter; a remedy
            // that reached wider would rewrite files nothing checks.
            assert!(!run[0].argv.iter().any(|a| a == "tests"), "{run:?}");
            assert!(!run[0].argv.iter().any(|a| a == "format"), "{run:?}");
            assert_eq!(verify[0].argv.last().unwrap(), "src/");
        }
        other => panic!("expected Auto, got {other:?}"),
    }

    let log = "src/fno/x.py:12: error: Incompatible return value type";
    let f = classify(&ctx("Python static correctness", log), false);
    assert_eq!(f.signature, "mypy");
    assert!(
        matches!(&f.remedy, Remedy::Escalate { repro } if repro.contains("mypy")),
        "{:?}",
        f.remedy
    );
}

#[test]
fn heal_write_rows() {
    let log = concat!(
        "check-pr-node-closure: HEAD ref 'feature/x-aaaa' names x-aaaa, ",
        "and the exact trailer claims none of them.\n",
        "  Add a line reading:\n",
    );
    let f = classify(&ctx("check-pr-node-closure", log), false);
    assert_eq!(f.signature, "closure-trailer");
    assert_eq!(
        f.remedy,
        Remedy::EditBody {
            nodes: vec!["x-aaaa".to_string()]
        }
    );

    // Two branch nodes and a body whose two stale trailer lines name two
    // more ids: one invocation, four ids, one line, no stale line left.
    let mk = |ids: &str| {
        let log = format!(
            "check-pr-node-closure: HEAD ref 'feature/{ids}' names {ids}, \
             and the exact trailer claims none of them.\n"
        );
        classify(&ctx("check-pr-node-closure", &log), false)
    };
    let findings = vec![mk("x-aaaa"), mk("x-cccc")];
    let body =
        "Fixes the thing.\n\nBacklog-Closure: x-dddd\nSecond line.\nBacklog-Closure: x-eeee\n";
    let union = closure_union(&findings, body);
    assert_eq!(
        union,
        vec![
            "x-aaaa".to_string(),
            "x-cccc".to_string(),
            "x-dddd".to_string(),
            "x-eeee".to_string()
        ]
    );
    assert_eq!(
        edit_body_cmd(&union),
        "fno do pr closure-trailer x-aaaa --extra x-cccc --extra x-dddd --extra x-eeee"
    );
    let stripped = body_without_trailer_lines(body);
    assert!(!stripped.contains("Backlog-Closure:"));
    assert!(stripped.starts_with("Fixes the thing."));
    assert!(stripped.contains("Second line."));

    let mk = |ids: &str| {
        let log = format!(
            "check-pr-node-closure: HEAD ref 'feature/{ids}' names {ids}, \
             and the exact trailer claims none of them.\n"
        );
        classify(&ctx("check-pr-node-closure", &log), false)
    };
    let findings = vec![mk("x-aaaa")];
    let body = "Fixes the thing.\n\nBacklog-Closure: x-dddd\n";
    let union = closure_union(&findings, body);
    let line = format!("Backlog-Closure: {}", union.join(" "));
    let first = format!(
        "{}\n\n{}\n",
        body_without_trailer_lines(body).trim_end(),
        line
    );
    // The second heal reads the union line it wrote, takes the same ids,
    // and writes the identical body.
    let union2 = closure_union(&findings, &first);
    assert_eq!(union2, union);
    let line2 = format!("Backlog-Closure: {}", union2.join(" "));
    let second = format!(
        "{}\n\n{}\n",
        body_without_trailer_lines(&first).trim_end(),
        line2
    );
    assert_eq!(first, second);
}

#[test]
fn pytest_rows() {
    let log = concat!(
        "FAILED tests/unit/test_mail_force.py::test_force_writes_a_row - AssertionError\n",
        "FAILED tests/unit/test_agents_cli_fold.py::test_paths - assert\n",
        "3 failed, 19455 passed, 301 skipped\n",
    );
    let f = classify(&ctx("smoke-pytest", log), false);
    assert_eq!(f.signature, "pytest");
    match f.remedy {
        Remedy::Escalate { repro } => {
            assert!(repro.starts_with("cd cli && uv run pytest "), "{repro}");
            assert!(repro.contains("tests/unit/test_mail_force.py::test_force_writes_a_row"));
            assert!(repro.contains("tests/unit/test_agents_cli_fold.py::test_paths"));
        }
        other => panic!("expected Escalate, got {other:?}"),
    }

    // Two real shards, two spellings of the same path. Both repros have
    // to run from `cli`, so the `cli/` prefix is dropped rather than
    // producing a path that does not exist there.
    let log = concat!(
        "FAILED tests/unit/test_a.py::test_one - x\n",
        "FAILED cli/tests/unit/test_b.py::test_two - y\n",
    );
    let f = classify(&ctx("smoke-pytest", log), false);
    match f.remedy {
        Remedy::Escalate { repro } => {
            assert!(repro.contains("tests/unit/test_b.py::test_two"), "{repro}");
            assert!(!repro.contains("cli/tests/"), "{repro}");
        }
        other => panic!("expected Escalate, got {other:?}"),
    }

    let log: String = (1..=9)
        .map(|n| format!("FAILED tests/unit/t.py::test_{n} - x"))
        .collect::<Vec<_>>()
        .join("\n");
    let f = classify(&ctx("smoke-pytest", &log), false);
    match f.remedy {
        Remedy::Escalate { repro } => {
            assert!(repro.contains("test_5"), "{repro}");
            assert!(!repro.contains("test_6"), "{repro}");
            assert!(repro.contains("and 4 more"), "{repro}");
        }
        other => panic!("expected Escalate, got {other:?}"),
    }
}

#[test]
fn rollup_rows() {
    // The `smoke` job's whole log, near enough: one echo and an exit on
    // its shards' results. Classified `unknown` it printed 38 lines of
    // runner boilerplate on every red PR and pointed at nothing.
    let log = concat!(
        "##[group]Run echo \"smoke-pytest=failure smoke-rest=failure\"\n",
        "smoke-pytest=failure smoke-rest=failure\n",
        "##[error]Process completed with exit code 1.\n",
    );
    let f = classify(&ctx("smoke", log), false);
    assert_eq!(f.signature, "shard-rollup");
    match f.remedy {
        Remedy::Escalate { repro } => {
            assert!(repro.contains("smoke-pytest"), "{repro}");
            assert!(repro.contains("smoke-rest"), "{repro}");
        }
        other => panic!("expected Escalate, got {other:?}"),
    }

    // The gate only classifies when a shard did not pass; an all-green
    // echo in some other job's log must not capture that job.
    assert_eq!(shard_rollup_shards("a=success b=success"), None);
    assert_eq!(shard_rollup_shards("a=success b=skipped"), None);

    // A real rollup off PR 1413. `cancelled` was outside the accepted
    // vocabulary, so the gate read `unknown` and printed 38 lines of
    // runner boilerplate. A cancelled shard is common precisely because a
    // push over a run in flight cancels one.
    let log = concat!(
        "##[group]Run echo \"smoke-pytest=cancelled smoke-rest=success\"\n",
        "smoke-pytest=cancelled smoke-rest=success\n",
        "##[error]Process completed with exit code 1.\n",
    );
    let f = classify(&ctx("smoke", log), false);
    assert_eq!(f.signature, "shard-rollup");
    match f.remedy {
        Remedy::Escalate { repro } => {
            assert!(repro.contains("smoke-pytest (cancelled)"), "{repro}");
            assert!(
                !repro.contains("smoke-rest"),
                "a passing shard is not named"
            );
        }
        other => panic!("expected Escalate, got {other:?}"),
    }
}

#[test]
fn report_rows() {
    // 13 unrecognized checks across every open PR buried the --all report
    // in 40-line log tails. Terse keeps the first line only.
    let findings = vec![classify(
        &ctx("mystery", "line one\nline two\nline three"),
        false,
    )];
    assert_eq!(findings[0].signature, "unknown");
    assert!(
        findings[0].detail().lines().count() > 1,
        "the full detail is multi-line"
    );
    assert_eq!(report(&findings, true, true), EXIT_ESCALATIONS);

    let log = "test stream_worker::tests::mid_turn_silence ... FAILED\n\
               test result: FAILED. 1948 passed; 1 failed;\n";
    let f = classify(&ctx("cargo test + schema parity", log), false);
    assert_eq!(f.signature, "cargo-test");
    match f.remedy {
        Remedy::Escalate { repro } => {
            assert!(repro.contains("cargo test --lib --bins"), "{repro}");
            assert!(
                repro.contains("stream_worker::tests::mid_turn_silence"),
                "{repro}"
            );
        }
        other => panic!("expected Escalate, got {other:?}"),
    }

    let log = "check-file-budget: cli/src/fno/mail/cli.py is 6105 lines (budget 5000)";
    let f = classify(&ctx("guards", log), false);
    assert_eq!(f.signature, "guard-script");
    assert_eq!(
        f.remedy,
        Remedy::Escalate {
            repro: "bash scripts/ci/check-file-budget.sh".to_string()
        }
    );

    // The real log that misdiagnosed: dozens of passing guards announcing
    // themselves, then a failure from a step that prints no `check-`
    // prefix at all. Prefix-matching named `check-pitfalls`, which passes.
    let log = concat!(
        "check-pitfalls: 4/10 entries, all valid\n",
        "smoke: pass     2s  In-N-Out menu-cap ratchet\n",
        "verb-ratchet: collapsed action inventory drifted from the map\n",
        "smoke: step failed, stopping (fail-fast): Verb-surface ratchet (real count)\n",
        "##[error]Process completed with exit code 1.\n",
    );
    let f = classify(&ctx("smoke-rest", log), false);
    assert_eq!(f.signature, "smoke-step");
    match f.remedy {
        Remedy::Escalate { repro } => {
            assert!(repro.contains("Verb-surface ratchet"), "{repro}");
            assert!(!repro.contains("check-pitfalls"), "{repro}");
        }
        other => panic!("expected Escalate, got {other:?}"),
    }

    // Every guard announces itself on success too, so a `guards` log is
    // mostly passing prefixes. The repro has to name the last one before
    // the error marker; the first one exits 0 and diagnoses nothing.
    let log = concat!(
        "check-retired-command-strings: OK: inspected 21 site(s)\n",
        "check-reachable-paths self-test: OK (canaries fired)\n",
        "check-reachable-paths: findings:\n",
        "  A new twin literal (in both .py and .rs): --allow-escape-sequences\n",
        "##[error]Process completed with exit code 1.\n",
        "check-package-path-escapes: OK\n",
    );
    let f = classify(&ctx("guards", log), false);
    assert_eq!(f.signature, "guard-script");
    assert_eq!(
        f.remedy,
        Remedy::Escalate {
            repro: "bash scripts/ci/check-reachable-paths.sh".to_string()
        }
    );

    let f = classify(&ctx("fno/review-coverage", ""), false);
    assert_eq!(f.signature, "review-gate");
    assert!(
        matches!(&f.remedy, Remedy::Escalate { repro } if repro.contains("not a CI failure")),
        "{:?}",
        f.remedy
    );

    // Measured on three open PRs: every `unknown` heal reported was a
    // cancelled check whose log carried one line. An empty log has three
    // explanations, and "the run concluded nothing" is the one that was
    // true. Deciding this on the log instead of the bucket produced seven
    // "add a signature to heal.rs" rows for a superseded run.
    let f = classify(&cancelled_ctx("guards", "Current runner version"), false);
    assert_eq!(f.signature, "cancelled");
    match f.remedy {
        Remedy::Escalate { repro } => {
            assert!(repro.contains("reached no verdict"), "{repro}");
            assert!(!repro.contains("add a signature"), "{repro}");
        }
        other => panic!("expected Escalate, got {other:?}"),
    }
}

#[test]
fn inherited_rows() {
    // inherited is checked before the table, so main's problem never
    // becomes this PR's whatever the bucket says.
    let f = classify(&cancelled_ctx("guards", ""), true);
    assert_eq!(f.signature, "inherited");

    let log = strip_timestamps(FMT_LOG);
    let f = classify(&ctx("cargo fmt --check (pinned)", &log), true);
    assert_eq!(f.signature, "inherited");
    assert_eq!(f.remedy, Remedy::Inherited);
    assert!(!f.counts_against_pr());

    let log: String = (1..=60)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let f = classify(&ctx("mystery", &log), false);
    assert_eq!(f.signature, "unknown");
    match f.remedy {
        Remedy::Escalate { repro } => {
            assert!(repro.contains("add a signature to heal.rs"), "{repro}");
            assert!(repro.contains("line 60"), "the tail ends at the last line");
            assert!(repro.contains("line 21"), "the tail is 40 lines deep");
            assert!(!repro.contains("line 20"), "and no deeper");
        }
        other => panic!("expected Escalate, got {other:?}"),
    }

    let f = classify(&ctx("some-check", ""), false);
    assert_eq!(f.signature, "unknown");
    assert!(f.counts_against_pr());
}

#[test]
fn table_rows() {
    let text = playbook();
    for sig in SIGNATURES {
        assert!(
            text.contains(sig.name),
            "{} missing from playbook",
            sig.name
        );
    }
    assert!(text.contains("inherited"));
    assert!(text.contains("unknown"));

    let checks = json!([
        {"name": "a", "bucket": "fail", "link": "", "workflow": "w"},
        {"name": "b", "bucket": "pass", "link": "", "workflow": "w"},
        {"name": "c", "bucket": "cancel", "link": "", "workflow": "w"},
        {"name": "d", "bucket": "pending", "link": "", "workflow": "w"},
    ]);
    let names: Vec<String> = failing_rows(&checks)
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["a", "c"]);

    let settled = json!([{"name": "a", "bucket": "fail"}, {"name": "b", "bucket": "pass"}]);
    assert!(!any_pending(&settled));
    let running = json!([{"name": "a", "bucket": "pass"}, {"name": "b", "bucket": "pending"}]);
    assert!(any_pending(&running));

    assert_eq!(
        job_id("https://github.com/o/r/actions/runs/123/job/456"),
        Some("456".to_string())
    );
    assert_eq!(job_id("https://example.test/build/7"), None);
}

// ── the verb: push discipline ───────────────────────────────────────────
// Stub `gh` and `git`, not a real remote: exactly one push, never a push
// over a run in flight, provable rather than argued.

use crate::write_exec_stub as write_exec;

/// A stub `gh` answering the four reads heal makes. `pending` decides
/// whether the SECOND check-runs read (the pre-push one) reports a run in
/// flight, which is how the in-flight guard is exercised.
fn stub_gh(dir: &Path, pending_on_second_read: bool) -> std::path::PathBuf {
    let flip = if pending_on_second_read {
        r#"if [ -f "$D/seen" ]; then B=in_progress; else touch "$D/seen"; fi"#
    } else {
        ""
    };
    write_exec(
        dir,
        "gh",
        &format!(
            r#"#!/bin/sh
D="$(dirname "$0")"
echo "gh $*" >> "$D/gh.log"
for a in "$@"; do case "$a" in
  */pulls/*) echo '{{"head":{{"sha":"deadbeef","ref":"feature/x","repo":{{"full_name":"o/r"}}}},"base":{{"ref":"main","repo":{{"full_name":"o/r"}}}},"author_association":"OWNER","body":"b"}}'; exit 0 ;;
  */check-runs) B=completed; {flip}
     if [ "$B" = in_progress ]; then
       echo '{{"check_runs":[{{"name":"cargo fmt --check (pinned)","status":"in_progress","conclusion":null,"html_url":"https://github.com/o/r/actions/runs/1/job/9"}}]}}'
     else
       echo '{{"check_runs":[{{"name":"cargo fmt --check (pinned)","status":"completed","conclusion":"failure","html_url":"https://github.com/o/r/actions/runs/1/job/9"}}]}}'
     fi
     exit 0 ;;
  */logs) echo "Diff in /w/w/crates/fno-agents/src/x.rs:1:"; exit 0 ;;
  */status) echo '{{"statuses":[]}}'; exit 0 ;;
esac; done
echo '[]'
"#
        ),
    )
}

/// A stub `git` recording every invocation. `dirty` decides what
/// `status --porcelain` answers.
fn stub_git(dir: &Path, branch: &str, dirty: bool) -> std::path::PathBuf {
    let porcelain = if dirty { "echo ' M src/x.rs'" } else { ":" };
    write_exec(
        dir,
        "git",
        &format!(
            r#"#!/bin/sh
D="$(dirname "$0")"
echo "git $*" >> "$D/git.log"
case "$1 $2" in
  "rev-parse --abbrev-ref") echo {branch}; exit 0 ;;
  "status --porcelain") if [ -f "$D/fixed" ]; then echo ' M src/x.rs'; else {porcelain}; fi; exit 0 ;;
esac
exit 0
"#
        ),
    )
}

/// A stub `cargo` on PATH that "fixes" the drift: the first run dirties
/// the tree, and the verify then passes. The crate directory is created
/// because a remedy runs IN it, and a missing cwd fails the spawn -- which
/// heal correctly reads as "the fix did not succeed".
fn stub_cargo(dir: &Path) {
    std::fs::create_dir_all(dir.join("crates/fno-agents")).unwrap();
    write_exec(
        dir,
        "cargo",
        r#"#!/bin/sh
D="$(dirname "$0")"
echo "cargo $*" >> "$D/cargo.log"
for a in "$@"; do [ "$a" = "--check" ] && exit 0; done
touch "$D/fixed"
exit 0
"#,
    );
}

/// A stub `cargo` that always succeeds and never touches the tree. It
/// stands for a remedy whose red does not reproduce locally.
fn stub_cargo_noop(dir: &Path) {
    std::fs::create_dir_all(dir.join("crates/fno-agents")).unwrap();
    std::fs::create_dir_all(dir.join("crates/fno")).unwrap();
    write_exec(
        dir,
        "cargo",
        r#"#!/bin/sh
D="$(dirname "$0")"
echo "cargo $*" >> "$D/cargo.log"
exit 0
"#,
    );
}

/// A stub `git` whose `commit` always fails, standing for a rejecting
/// pre-commit hook or a signing failure.
fn stub_git_commit_fails(dir: &Path, branch: &str) -> std::path::PathBuf {
    write_exec(
        dir,
        "git",
        &format!(
            r#"#!/bin/sh
D="$(dirname "$0")"
echo "git $*" >> "$D/git.log"
case "$1 $2" in
  "rev-parse --abbrev-ref") echo {branch}; exit 0 ;;
  "status --porcelain") if [ -f "$D/fixed" ]; then echo ' M src/x.rs'; fi; exit 0 ;;
esac
case "$1" in
  commit) echo "pre-commit hook refused" >&2; exit 1 ;;
esac
exit 0
"#
        ),
    )
}

fn log_of(dir: &Path, name: &str) -> String {
    // The events journal is store-committed: read committed rows, not
    // journal bytes. Other stub logs (gh.log, fno.log) stay raw files.
    if name == "events.jsonl" {
        return crate::events::committed_journal_text(&dir.join(name));
    }
    std::fs::read_to_string(dir.join(name)).unwrap_or_default()
}

fn args_for(dir: &Path, extra: &[&str]) -> Vec<String> {
    let mut v = vec![
        "1".to_string(),
        "--gh-bin".to_string(),
        dir.join("gh").to_string_lossy().into_owned(),
        "--git-bin".to_string(),
        dir.join("git").to_string_lossy().into_owned(),
        "--cwd".to_string(),
        dir.to_string_lossy().into_owned(),
    ];
    v.push("--bin-dir".to_string());
    v.push(dir.to_string_lossy().into_owned());
    // The journal rides the same test seam as drive_args below: without it
    // the default resolves through $HOME and the hermetic guard panics.
    v.push("--events-file".to_string());
    v.push(dir.join("events.jsonl").to_string_lossy().into_owned());
    v.extend(extra.iter().map(|s| s.to_string()));
    v
}

#[test]
fn remedy_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh(d, false);
    stub_git(d, "feature/x", false);
    stub_cargo(d);
    let code = run_heal(&args_for(d, &[]));
    assert_eq!(code, EXIT_ESCALATIONS.max(EXIT_CLEAN), "dry run reports");
    assert_eq!(log_of(d, "cargo.log"), "", "no remedy ran");
    assert!(!log_of(d, "git.log").contains("push"), "no push");

    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh(d, false);
    stub_git(d, "feature/x", true);
    stub_cargo(d);
    let code = run_heal(&args_for(d, &["--apply"]));
    assert_eq!(code, EXIT_CWD_REFUSAL);
    assert_eq!(log_of(d, "cargo.log"), "", "no remedy ran");
    assert!(!log_of(d, "git.log").contains("push"));

    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh(d, false);
    stub_git(d, "main", false);
    stub_cargo(d);
    assert_eq!(run_heal(&args_for(d, &["--apply"])), EXIT_CWD_REFUSAL);
    assert_eq!(log_of(d, "cargo.log"), "");

    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh(d, false);
    stub_git(d, "feature/x", false);
    stub_cargo(d);
    run_heal(&args_for(d, &["--apply"]));
    let git = log_of(d, "git.log");
    assert_eq!(git.matches("git commit").count(), 1, "{git}");
    assert_eq!(git.matches("git push").count(), 1, "{git}");

    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    // The pre-push re-read reports a check still running.
    stub_gh(d, true);
    stub_git(d, "feature/x", false);
    stub_cargo(d);
    let code = run_heal(&args_for(d, &["--apply"]));
    assert_eq!(code, EXIT_IN_FLIGHT);
    let git = log_of(d, "git.log");
    assert_eq!(git.matches("git commit").count(), 1, "the fix is kept");
    assert!(!git.contains("git push"), "but never pushed: {git}");

    // `dirty` is a WHOLE-WORKTREE question and the remedies share one
    // worktree, so an earlier remedy's uncommitted edit made a later
    // no-op remedy read as dirty and take credit for work it did not do.
    // Each remedy is measured against its own before/after now.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh(d, false);
    stub_git(d, "feature/x", false);
    stub_cargo_noop(d);

    let mut findings = vec![classify(
        &ctx("cargo fmt --check (pinned)", "log unavailable"),
        false,
    )];
    let args = parse_args(&args_for(d, &["--apply"])).unwrap();
    // The tree is already dirty when the remedy runs, exactly as it would
    // be after a previous finding's fix.
    std::fs::write(d.join("fixed"), "").unwrap();

    let healed = apply_auto(&args, &mut findings);
    assert!(
        healed.is_empty(),
        "a no-op remedy is never healed: {healed:?}"
    );
    assert!(
        matches!(&findings[0].remedy, Remedy::Escalate { repro } if repro.contains("changed nothing")),
        "{:?}",
        findings[0].remedy
    );

    // The fix is in the worktree but no commit and no push happened, so
    // the check is still red on the remote. Exiting 0 there is the false
    // green the no-op arm was written to close, left open on this path.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh(d, false);
    stub_git_commit_fails(d, "feature/x");
    stub_cargo(d);

    let code = run_heal(&args_for(d, &["--apply"]));
    assert_ne!(code, EXIT_CLEAN, "a failed commit is not a clean PR");
    assert!(
        !log_of(d, "git.log").contains("git push"),
        "and nothing is pushed"
    );

    // --dry-run rehearses --all --apply; on the single-PR path it would
    // silently mean "the report you already get without --apply".
    assert_eq!(
        run_heal(&[
            "1".to_string(),
            "--apply".to_string(),
            "--dry-run".to_string()
        ]),
        EXIT_READ_ERROR
    );
}

// ── the drive loop ─────────────────────────────────────────────────────
//
// Driven against stub gh/git/cargo/fno executables, so the four refusals
// and the tick row are provable rather than argued.

/// A stub `gh` answering the drive loop's reads: an open-PR listing with
/// PR 1 (branch feature/x-1111, free) and PR 2 (feature/x-2222, whose
/// node the test holds a live claim on), each with one rustfmt-drift
/// failure. `mystery` swaps PR 1's check for one no signature matches.
fn stub_gh_drive(dir: &Path, mystery: bool) -> std::path::PathBuf {
    let check = if mystery {
        r#"{"name":"mystery-check","status":"completed","conclusion":"failure","html_url":"https://github.com/o/r/actions/runs/1/job/9"}"#
    } else {
        r#"{"name":"cargo fmt --check (pinned)","status":"completed","conclusion":"failure","html_url":"https://github.com/o/r/actions/runs/1/job/9"}"#
    };
    // A mystery PR's log must match nothing in the table either: the
    // rustfmt Diff line would classify it rustfmt-drift however the check
    // is named, and the escalation under test would never fire.
    let log_line = if mystery {
        "totally novel failure output"
    } else {
        "Diff in /w/w/crates/fno-agents/src/x.rs:1:"
    };
    write_exec(
        dir,
        "gh",
        &format!(
            r#"#!/bin/sh
D="$(dirname "$0")"
echo "gh $*" >> "$D/gh.log"
for a in "$@"; do case "$a" in
  *'pulls?state=open'*)
     echo '[{{"number":1,"head":{{"sha":"aaa1","ref":"feature/x-1111"}},"body":"b"}},{{"number":2,"head":{{"sha":"bbb2","ref":"feature/x-2222"}},"body":"b"}},{{"number":3,"head":{{"sha":"ccc3","ref":"patch-1","repo":{{"full_name":"stranger/footnote"}}}},"base":{{"ref":"main","repo":{{"full_name":"o/r"}}}},"author_association":"FIRST_TIMER","body":"b"}}]'
     exit 0 ;;
  *pulls/1*) echo '{{"head":{{"sha":"aaa1","ref":"feature/x-1111","repo":{{"full_name":"o/r"}}}},"base":{{"ref":"main","repo":{{"full_name":"o/r"}}}},"author_association":"OWNER","body":"b"}}'; exit 0 ;;
  *pulls/2*) echo '{{"head":{{"sha":"bbb2","ref":"feature/x-2222","repo":{{"full_name":"o/r"}}}},"base":{{"ref":"main","repo":{{"full_name":"o/r"}}}},"author_association":"OWNER","body":"b"}}'; exit 0 ;;
  *pulls/3*) echo '{{"head":{{"sha":"ccc3","ref":"patch-1","repo":{{"full_name":"stranger/footnote"}}}},"base":{{"ref":"main","repo":{{"full_name":"o/r"}}}},"author_association":"FIRST_TIMER","body":"b"}}'; exit 0 ;;
  *check-runs) echo '{{"check_runs":[{check}]}}'; exit 0 ;;
  */logs) echo "{log_line}"; exit 0 ;;
  */status) echo '{{"statuses":[]}}'; exit 0 ;;
esac; done
echo '[]'
"#
        ),
    )
}

/// A stub `git` that both lists one worktree (on feature/x-1111) and
/// answers run_one's branch/porcelain/commit/push questions inside it.
fn stub_git_drive(dir: &Path) -> std::path::PathBuf {
    let wt = dir.join("wt");
    let wt = wt.to_string_lossy().into_owned();
    write_exec(
        dir,
        "git",
        &format!(
            r#"#!/bin/sh
D="$(dirname "$0")"
echo "git $*" >> "$D/git.log"
case "$1 $2" in
  "worktree list") printf 'worktree {wt}\nHEAD aaa\nbranch refs/heads/feature/x-1111\n\n'; exit 0 ;;
  "rev-parse --abbrev-ref") echo feature/x-1111; exit 0 ;;
  "status --porcelain") if [ -f "$D/fixed" ]; then echo ' M src/x.rs'; fi; exit 0 ;;
esac
exit 0
"#
        ),
    )
}

/// The drive-loop stub gh with PR 1's one red check a CANCELLED run
/// (Actions run 777): the class the rerun remedy exists for.
fn stub_gh_drive_rerun(dir: &Path) -> std::path::PathBuf {
    write_exec(
        dir,
        "gh",
        r#"#!/bin/sh
D="$(dirname "$0")"
echo "gh $*" >> "$D/gh.log"
for a in "$@"; do case "$a" in
  *'pulls?state=open'*)
     echo '[{"number":1,"head":{"sha":"aaa1","ref":"feature/x-1111"},"body":"b"},{"number":2,"head":{"sha":"bbb2","ref":"feature/x-2222"},"body":"b"}]'
     exit 0 ;;
  *pulls/1*) echo '{"head":{"sha":"aaa1","ref":"feature/x-1111","repo":{"full_name":"o/r"}},"base":{"ref":"main","repo":{"full_name":"o/r"}},"author_association":"OWNER","body":"b"}'; exit 0 ;;
  *pulls/2*) echo '{"head":{"sha":"bbb2","ref":"feature/x-2222","repo":{"full_name":"o/r"}},"base":{"ref":"main","repo":{"full_name":"o/r"}},"author_association":"OWNER","body":"b"}'; exit 0 ;;
  *check-runs) echo '{"check_runs":[{"name":"ci","status":"completed","conclusion":"cancelled","html_url":"https://github.com/o/r/actions/runs/777/job/9"}]}'; exit 0 ;;
  */logs) echo "the run was cancelled before any step ran"; exit 0 ;;
  */status) echo '{"statuses":[]}'; exit 0 ;;
esac; done
echo '[]'
"#,
    )
}

/// A stub `fno` logging every invocation. Heal files fleet tasks by
/// writing the store directly, so the inbox lanes are gone; the stub
/// exists for the verbs the seams still shell (`do pr push`, `backlog`).
fn stub_fno(dir: &Path) {
    write_exec(
        dir,
        "fno",
        r#"#!/bin/sh
D="$(dirname "$0")"
echo "fno $*" >> "$D/fno.log"
exit 0
"#,
    );
}

/// Hold a live claim on `x-2222` under the given root, as a worker would.
fn hold_claim(dir: &Path) {
    let opts = crate::claims::AcquireOpts {
        pid: Some(std::process::id()),
        root: Some(dir.to_path_buf()),
        events_dir: Some(dir.to_path_buf()),
        ..Default::default()
    };
    match crate::claims::acquire("node:x-2222", "worker:t-x", opts) {
        crate::claims::AcquireOutcome::Acquired(_) => {}
        other => panic!("claim setup failed: {other:?}"),
    }
}

/// Drive-loop argv with the claim root and the events file pinned to the
/// test dir, so the two side effects never touch the real `~/.fno`.
fn drive_args(dir: &Path, extra: &[&str]) -> Vec<String> {
    let mut v = args_for(dir, &["--all", "--apply"]);
    v.push("--claims-root".to_string());
    v.push(dir.to_string_lossy().into_owned());
    v.push("--events-file".to_string());
    v.push(dir.join("events.jsonl").to_string_lossy().into_owned());
    v.extend(extra.iter().map(|s| s.to_string()));
    v
}

#[test]
fn drive_rows() {
    // Parity cases from the Python half and the CI gate: a plain branch
    // names nothing, a node branch names its node, and a trailing segment
    // never re-glues into a second, bogus candidate.
    assert_eq!(branch_node_ids("main"), Vec::<String>::new());
    assert_eq!(branch_node_ids("fix/respawn-race"), Vec::<String>::new());
    assert_eq!(
        branch_node_ids("feature/x-bbbb"),
        vec!["x-bbbb".to_string()]
    );
    assert_eq!(
        branch_node_ids("feature/x-cccc-1234"),
        vec!["x-cccc".to_string()],
        "the all-hex suffix must not re-glue into cdef-1234"
    );
    // Fixed-width hex makes a prefix of x-5b667: only the
    // delimiter-bounded one counts.
    assert_eq!(
        branch_node_ids("feature/x-5b667"),
        vec!["x-5b667".to_string()]
    );

    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive(d, false);
    stub_git_drive(d);
    stub_cargo(d);
    stub_fno(d);
    hold_claim(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    let code = run_heal(&drive_args(d, &["--dry-run"]));
    assert_eq!(
        code, EXIT_ESCALATIONS,
        "a rehearsal over a would-heal PR reports the work, exactly like the report-only --all"
    );
    let out = log_of(d, "gh.log");
    assert!(out.contains("pulls/2"), "the claimed PR was read: {out}");
    assert_eq!(log_of(d, "cargo.log"), "", "a dry run runs no remedy");
    assert!(!log_of(d, "git.log").contains("push"), "and pushes nothing");
    // The tick row lands with the dry_run marker and both verdicts.
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("pr_heal_tick"), "{events}");
    assert!(events.contains("\"dry_run\":true"), "{events}");
    assert!(events.contains("\"skip_claim_held\":1"), "{events}");
    assert!(events.contains("\"skip_outside\":1"), "{events}");
    assert!(events.contains("\"would_heal\":1"), "{events}");

    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive(d, false);
    stub_git_drive(d);
    stub_cargo(d);
    stub_fno(d);
    hold_claim(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    let code = run_heal(&drive_args(d, &[]));
    assert_eq!(code, EXIT_CLEAN, "the free PR healed: {code}");
    let git = log_of(d, "git.log");
    assert_eq!(git.matches("git push").count(), 1, "one push: {git}");
    assert_ne!(log_of(d, "cargo.log"), "", "the remedy ran");
    let git_fork = log_of(d, "git.log");
    assert!(
        !git_fork.contains("patch-1"),
        "no git work for the fork: {git_fork}"
    );
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("\"healed\":1"), "{events}");
    assert!(events.contains("\"steward_off_no_law\":1"), "{events}");
    assert!(events.contains("\"skip_claim_held\":1"), "{events}");
    assert!(events.contains("\"skip_outside\":1"), "{events}");

    // The single-PR path skips an outside PR before any worktree
    // read: exit clean, no git call.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive(d, false);
    stub_git_drive(d);
    stub_cargo(d);
    stub_fno(d);
    let code = run_heal(&args_for(d, &["3", "--apply"]));
    assert_eq!(code, EXIT_CLEAN, "the fork PR skips clean: {code}");
    assert_eq!(log_of(d, "git.log"), "", "no git call for the fork PR");

    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive(d, false);
    // No stub_git_drive: `git worktree list` fails, so no worktree is
    // found for the free PR's branch.
    stub_git(d, "feature/x-1111", false);
    stub_cargo(d);
    stub_fno(d);
    hold_claim(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    let code = run_heal(&drive_args(d, &[]));
    assert_ne!(code, EXIT_CLEAN, "the red PR is still red");
    assert_eq!(log_of(d, "cargo.log"), "", "no remedy ran anywhere");
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("skip_no_worktree"), "{events}");

    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh(d, false);
    stub_git(d, "feature/x", false);
    assert_eq!(run_heal(&args_for(d, &["--playbook"])), EXIT_CLEAN);
    assert_eq!(log_of(d, "gh.log"), "", "no read");

    assert_eq!(run_heal(&["--nope".to_string()]), EXIT_READ_ERROR);

    // --slurp hands back an array of pages, so nothing has to find a
    // boundary. The old hand-rolled `split("][")` cut inside any PR body
    // carrying a markdown reference link and dropped those PRs silently.
    let one: Vec<Value> = vec![json!([{"number": 7}, {"number": 9}])];
    assert_eq!(open_pr_numbers(&one), vec!["7", "9"]);

    let many: Vec<Value> = vec![json!([{"number": 7}]), json!([{"number": 9}])];
    assert_eq!(open_pr_numbers(&many), vec!["7", "9"]);

    let with_bracket_pair: Vec<Value> = vec![json!([{"number": 7, "body": "see [the doc][ref]"}])];
    assert_eq!(open_pr_numbers(&with_bracket_pair), vec!["7"]);

    // Keeping the LAST non-clean code let a later escalation mask an
    // earlier failure to read the world at all.
    assert_eq!(worse_of(EXIT_READ_ERROR, EXIT_ESCALATIONS), EXIT_READ_ERROR);
    assert_eq!(worse_of(EXIT_ESCALATIONS, EXIT_IN_FLIGHT), EXIT_ESCALATIONS);
    assert_eq!(worse_of(EXIT_CLEAN, EXIT_IN_FLIGHT), EXIT_IN_FLIGHT);
    assert_eq!(worse_of(EXIT_CLEAN, EXIT_CLEAN), EXIT_CLEAN);

    // The guard joins candidates with commas and NO space, so a capture
    // that cannot cross a comma matched nothing and the remedy became a
    // silent no-op on exactly the branches that needed it most.
    let log = concat!(
        "check-pr-node-closure: HEAD ref 'feature/x-a1-x-b2' names x-a1,x-b2, ",
        "and the exact trailer claims none of them.\n",
    );
    let f = classify(&ctx("check-pr-node-closure", log), false);
    assert_eq!(
        f.remedy,
        Remedy::EditBody {
            nodes: vec!["x-a1".to_string(), "x-b2".to_string()]
        }
    );

    let unknown = json!({"status": "completed", "conclusion": "action_required"});
    assert_eq!(rest_bucket(&unknown), "fail");
    assert_eq!(
        rest_bucket(&json!({"status": "completed", "conclusion": "success"})),
        "pass"
    );
    assert_eq!(
        rest_bucket(&json!({"status": "queued", "conclusion": null})),
        "pending"
    );
}

#[test]
fn rerun_rows() {
    // The rerun comes first, exactly once per (sha, run id); only a
    // second red on the same pair is the verdict. The escalation names
    // the double failure, so the operator reads a verdict, not a guess.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive(d, true);
    stub_git_drive(d);
    stub_fno(d);
    hold_claim(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    let code1 = run_heal(&drive_args(d, &[]));
    assert_eq!(
        code1, EXIT_CLEAN,
        "the rerun acted: first sighting exits clean"
    );
    let gh = log_of(d, "gh.log");
    assert_eq!(gh.matches("run rerun 1 --failed").count(), 1, "{gh}");
    let store_before =
        crate::event_store::journal_text(&d.join("questions.jsonl"), &["fleet_task"]);
    assert!(
        !store_before.contains("fleet_task"),
        "no task before the rerun answers: {store_before}"
    );
    run_heal(&drive_args(d, &[]));
    let store = crate::event_store::journal_text(&d.join("questions.jsonl"), &["fleet_task"]);
    assert_eq!(
        store.matches(r#""type":"fleet_task""#).count(),
        1,
        "exactly one task: {store}"
    );
    assert!(
        store.contains("no playbook signature") && store.contains("mystery-check"),
        "{store}"
    );
    assert!(store.contains(r#""run":"fno do pr heal 1""#), "{store}");
    // The next tick re-files nothing: the open task dedups on its key.
    run_heal(&drive_args(d, &[]));
    let store = crate::event_store::journal_text(&d.join("questions.jsonl"), &["fleet_task"]);
    assert_eq!(
        store.matches(r#""type":"fleet_task""#).count(),
        1,
        "the open task is never re-filed: {store}"
    );
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("\"unknown\":1"), "{events}");

    let f = classify(
        &cancelled_run_ctx("ci", "https://github.com/o/r/actions/runs/777/job/9"),
        false,
    );
    assert_eq!(f.signature, "cancelled");
    assert_eq!(
        f.remedy,
        Remedy::Rerun {
            run_id: "777".to_string()
        }
    );
    assert_eq!(f.action(), "rerun");
    assert_eq!(f.detail(), "gh run rerun 777");
    assert!(f.counts_against_pr());

    // A link that names no run (a StatusContext, or no link at all)
    // leaves nothing to rerun; the escalation is the honest answer.
    let f = classify(&cancelled_ctx("ci", ""), false);
    assert_eq!(f.signature, "cancelled");
    assert!(
        matches!(&f.remedy, Remedy::Escalate { repro } if repro.contains("cancelled")),
        "{:?}",
        f.remedy
    );

    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive_rerun(d);
    stub_git_drive(d);
    stub_fno(d);
    hold_claim(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    let code = run_heal(&drive_args(d, &[]));
    assert_eq!(code, EXIT_CLEAN, "the rerun acted: the PR exits clean");
    let gh = log_of(d, "gh.log");
    assert_eq!(
        gh.matches("run rerun 777").count(),
        1,
        "exactly one rerun: {gh}"
    );
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("\"reran\":1"), "{events}");
    assert!(events.contains("\"rerun_keys\":[\"aaa1:777\"]"), "{events}");

    // The guard reads rerun_shas off pr_heal_tick rows; the drive loop
    // writes its own. A manual `pr-heal <n> --apply` writes one too, so
    // a hand rerun is never issued a second time by the next cycle.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive_rerun(d);
    stub_git_drive(d);
    stub_fno(d);
    hold_claim(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    let mut args = args_for(d, &["--apply"]);
    args.push("--claims-root".to_string());
    args.push(d.to_string_lossy().into_owned());
    args.push("--events-file".to_string());
    args.push(d.join("events.jsonl").to_string_lossy().into_owned());
    run_heal(&args);
    run_heal(&args);
    let gh = log_of(d, "gh.log");
    assert_eq!(
        gh.matches("run rerun 777").count(),
        1,
        "the second manual apply never re-runs the sha: {gh}"
    );

    // The journal is the once-guard: a prior pr_heal_tick row naming the
    // sha in rerun_shas means the rerun was already issued; a second
    // cancelled verdict on the same sha reached a real result.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive_rerun(d);
    stub_git_drive(d);
    stub_fno(d);
    hold_claim(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    run_heal(&drive_args(d, &[]));
    run_heal(&drive_args(d, &[]));
    let gh = log_of(d, "gh.log");
    assert_eq!(
        gh.matches("run rerun 777").count(),
        1,
        "the second cycle never re-runs the sha: {gh}"
    );
    let events = log_of(d, "events.jsonl");
    assert!(
        events.contains("\"still_red\":1"),
        "the demoted rerun is still red, never healed: {events}"
    );
}

#[test]
fn unchanged_skip_rows() {
    // Sweep 2 over an unchanged world: every PR skips on (head, digest),
    // no per-PR pulls read, no job log, no remedy, and the lazy
    // main-HEAD read never fires. The tick row records the world the
    // next sweep compares against.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive(d, false);
    stub_git_drive(d);
    stub_cargo(d);
    stub_fno(d);
    hold_claim(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    let code1 = run_heal(&drive_args(d, &[]));
    assert_eq!(code1, EXIT_CLEAN, "sweep 1 heals the free PR: {code1}");
    let pulls_sweep1 = log_of(d, "gh.log").matches("pulls/").count();
    let logs_sweep1 = log_of(d, "gh.log").matches("/logs").count();
    let cargo_sweep1 = log_of(d, "cargo.log");

    let code2 = run_heal(&drive_args(d, &[]));
    assert_eq!(
        code2, EXIT_CLEAN,
        "an all-skipped sweep exits clean: {code2}"
    );
    let gh_sweep2 = log_of(d, "gh.log");
    assert_eq!(
        gh_sweep2.matches("pulls/").count(),
        pulls_sweep1,
        "a skipped PR is never pulls-read: {gh_sweep2}"
    );
    assert_eq!(
        gh_sweep2.matches("/logs").count(),
        logs_sweep1,
        "a skipped PR reads no job log: {gh_sweep2}"
    );
    assert_eq!(
        log_of(d, "cargo.log"),
        cargo_sweep1,
        "a skipped PR runs no remedy"
    );
    // The main-HEAD read is lazy: an all-skipped sweep never pays it.
    let run_list_before = gh_sweep2.matches("run list").count();
    let code2b = run_heal(&drive_args(d, &[]));
    assert_eq!(code2b, EXIT_CLEAN);
    assert_eq!(
        log_of(d, "gh.log").matches("run list").count(),
        run_list_before,
        "an all-skipped sweep never reads main's HEAD"
    );
    // The rehearsal never takes the skip: over the same unchanged world
    // its exit still means "red PRs exist", never "the rehearsal ran".
    let code_dry = run_heal(&drive_args(d, &["--dry-run"]));
    assert_eq!(
        code_dry, EXIT_ESCALATIONS,
        "a rehearsal reports the unchanged red: {code_dry}"
    );
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("\"skip_unchanged\":3"), "{events}");
    // The recorded world is the next sweep's baseline.
    assert!(events.contains("\"pr_states\""), "{events}");
    for sha in ["aaa1", "bbb2", "ccc3"] {
        assert!(
            events.contains(&format!("\"head\":\"{sha}\"")),
            "state for {sha}: {events}"
        );
    }

    // Sweep 3: PR 1 gains a commit, so it is processed (and healed
    // again) while the untouched PRs keep skipping. The stub's `fixed`
    // marker stands for the dirt sweep 1's remedy left before its commit
    // carried it; the marker goes the way of the pushed fix.
    let _ = std::fs::remove_file(d.join("fixed"));
    let body = r#"#!/bin/sh
D="$(dirname "$0")"
echo "gh $*" >> "$D/gh.log"
for a in "$@"; do case "$a" in
  *'pulls?state=open'*)
     echo '[{"number":1,"head":{"sha":"aaa2","ref":"feature/x-1111"},"body":"b"},{"number":2,"head":{"sha":"bbb2","ref":"feature/x-2222"},"body":"b"},{"number":3,"head":{"sha":"ccc3","ref":"patch-1","repo":{"full_name":"stranger/footnote"}},"base":{"ref":"main","repo":{"full_name":"o/r"}},"author_association":"FIRST_TIMER","body":"b"}]'
     exit 0 ;;
  *pulls/1*) echo '{"head":{"sha":"aaa2","ref":"feature/x-1111","repo":{"full_name":"o/r"}},"base":{"ref":"main","repo":{"full_name":"o/r"}},"author_association":"OWNER","body":"b"}'; exit 0 ;;
  *pulls/2*) echo '{"head":{"sha":"bbb2","ref":"feature/x-2222","repo":{"full_name":"o/r"}},"base":{"ref":"main","repo":{"full_name":"o/r"}},"author_association":"OWNER","body":"b"}'; exit 0 ;;
  *pulls/3*) echo '{"head":{"sha":"ccc3","ref":"patch-1","repo":{"full_name":"stranger/footnote"}},"base":{"ref":"main","repo":{"full_name":"o/r"}},"author_association":"FIRST_TIMER","body":"b"}'; exit 0 ;;
  *check-runs) echo '{"check_runs":[{"name":"cargo fmt --check (pinned)","status":"completed","conclusion":"failure","html_url":"https://github.com/o/r/actions/runs/1/job/9"}]}'; exit 0 ;;
  */logs) echo "Diff in /w/w/crates/fno-agents/src/x.rs:1:"; exit 0 ;;
  */status) echo '{"statuses":[]}'; exit 0 ;;
esac; done
echo '[]'
"#;
    write_exec(d, "gh", body);
    let pulls_before_sweep3 = log_of(d, "gh.log").matches("pulls/1").count();
    let code3 = run_heal(&drive_args(d, &[]));
    assert_eq!(code3, EXIT_CLEAN, "the pushed PR heals again: {code3}");
    let gh_sweep3 = log_of(d, "gh.log");
    assert!(
        gh_sweep3.matches("pulls/1").count() > pulls_before_sweep3,
        "the changed PR is pulls-read: {gh_sweep3}"
    );
    // The last tick row folds this sweep's counts.
    let events3 = log_of(d, "events.jsonl");
    let last = events3
        .lines()
        .filter(|l| l.contains("pr_heal_tick"))
        .last()
        .expect("a tick row exists");
    assert!(last.contains("\"healed\":1"), "{last}");
    assert!(last.contains("\"skip_unchanged\":2"), "{last}");
    assert!(last.contains("\"aaa2\""), "the new head recorded: {last}");
}

// ── the rerun remedy ──────────────────────────────────────────────────

// ── the rebase triggers ───────────────────────────────────────────────

/// A stub `gh` for the rebase tests: PR 1 carries `mergeable` (the test
/// names its value) and a `base` of main. Its one red check is
/// rustfmt-drift, so if the loop ever reached the heal path the remedy
/// would run and cargo.log would name it.
fn stub_gh_drive_rebase(dir: &Path, pr1_mergeable: &str) {
    let body = r#"#!/bin/sh
D="$(dirname "$0")"
echo "gh $*" >> "$D/gh.log"
for a in "$@"; do case "$a" in
  *'pulls?state=open'*)
     echo '[{"number":1,"head":{"sha":"aaa1","ref":"feature/x-1111"},"base":{"ref":"main"},"mergeable":MERGEABLE,"body":"b"},{"number":2,"head":{"sha":"bbb2","ref":"feature/x-2222"},"base":{"ref":"main"},"mergeable":null,"body":"b"}]'
     exit 0 ;;
  *pulls/1*) echo '{"head":{"sha":"aaa1","ref":"feature/x-1111","repo":{"full_name":"o/r"}},"base":{"ref":"main","repo":{"full_name":"o/r"}},"author_association":"OWNER","mergeable":MERGEABLE,"body":"b"}'; exit 0 ;;
  *pulls/2*) echo '{"head":{"sha":"bbb2","ref":"feature/x-2222","repo":{"full_name":"o/r"}},"base":{"ref":"main","repo":{"full_name":"o/r"}},"author_association":"OWNER","mergeable":null,"body":"b"}'; exit 0 ;;
  *check-runs) echo '{"check_runs":[{"name":"cargo fmt --check (pinned)","status":"completed","conclusion":"failure","html_url":"https://github.com/o/r/actions/runs/1/job/9"}]}'; exit 0 ;;
  */logs) echo "Diff in /w/w/crates/fno-agents/src/x.rs:1:"; exit 0 ;;
  */status) echo '{"statuses":[]}'; exit 0 ;;
esac; done
echo '[]'
"#
    .replace("MERGEABLE", pr1_mergeable);
    write_exec(dir, "gh", &body);
}

/// A stub `fno` whose `do pr push` seam answers a caller-named
/// stdout/exit/stderr triple; the backlog lane logs and answers.
fn stub_fno_push(dir: &Path, push_stdout: &str, push_exit: u8, push_stderr: &str) {
    let body = r#"#!/bin/sh
D="$(dirname "$0")"
echo "fno $*" >> "$D/fno.log"
case "$*" in
  *"do pr push"*) printf '%s' 'PUSH_STDOUT' ; printf '%s' 'PUSH_STDERR' >&2; exit PUSH_EXIT ;;
  *"backlog idea"*) echo "backlog node fno-abc9 created"; exit 0 ;;
esac
exit 0
"#
    .replace("PUSH_STDOUT", push_stdout)
    .replace("PUSH_EXIT", &push_exit.to_string())
    .replace("PUSH_STDERR", push_stderr);
    write_exec(dir, "fno", &body);
}

/// Hold a live `merge-slot:main` claim naming `pr`, as the sweep would.
fn hold_slot_claim(dir: &Path, pr: u64) {
    let opts = crate::claims::AcquireOpts {
        pid: Some(std::process::id()),
        root: Some(dir.to_path_buf()),
        events_dir: Some(dir.to_path_buf()),
        ..Default::default()
    };
    match crate::claims::acquire("merge-slot:main", &format!("pr:{pr}"), opts) {
        crate::claims::AcquireOutcome::Acquired(_) => {}
        other => panic!("slot claim setup failed: {other:?}"),
    }
}

#[test]
fn rebase_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive_rebase(d, "false");
    stub_git_drive(d);
    stub_cargo(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    stub_fno_push(
        d,
        "pr-push: origin/main behind-before=9 behind-after=0 preflight=full ci=settled sha=abc pushed=1",
        0,
        "",
    );
    let code = run_heal(&drive_args(d, &[]));
    // PR 2 has no worktree and reads as an escalation; PR 1's rebase
    // itself exits clean.
    assert_eq!(code, EXIT_ESCALATIONS);
    let fno = log_of(d, "fno.log");
    assert_eq!(fno.matches("do pr push").count(), 1, "one push: {fno}");
    assert_eq!(log_of(d, "cargo.log"), "", "no heal in the same pass");
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("\"rebased\":1"), "{events}");
    assert!(events.contains("pr_heal_pr"), "{events}");
    // The emitter's own row, read back the way --status reads it: the
    // acted list must survive the string-to-number trip.
    let sa = parse_args(&[
        "--status".to_string(),
        "--armed".to_string(),
        "--events-file".to_string(),
        d.join("events.jsonl").to_string_lossy().into_owned(),
    ])
    .unwrap();
    assert!(
        status_line(&sa).contains("acted on PR 1"),
        "{}",
        status_line(&sa)
    );

    // `mergeable` reads null while GitHub computes; the merge slot is the
    // other trigger, and the ONLY reason this PR rebases. PR 2 conflicts
    // but holds no slot and has no worktree, so nothing else is rebased.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive_rebase(d, "null");
    stub_git_drive(d);
    stub_cargo(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    stub_fno_push(
        d,
        "pr-push: origin/main behind-before=3 behind-after=0 preflight=skipped sha=abc pushed=1",
        0,
        "",
    );
    hold_slot_claim(d, 1);
    run_heal(&drive_args(d, &[]));
    let fno = log_of(d, "fno.log");
    assert_eq!(fno.matches("do pr push").count(), 1, "{fno}");
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("\"rebased\":1"), "{events}");
    assert!(events.contains("\"skip_no_worktree\":1"), "{events}");

    // Refusal 1 runs FIRST: a conflicting PR whose node a live worker
    // holds is never rebased by the healer. PR 1 holds no trigger
    // (mergeable null, no slot), PR 2's node is claim-held and
    // conflicting, so no push may happen at all.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive_rebase(d, "null");
    stub_git_drive(d);
    stub_cargo(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    stub_fno_push(d, "", 0, "");
    hold_claim(d);
    run_heal(&drive_args(d, &[]));
    let fno = log_of(d, "fno.log");
    assert!(
        !fno.contains("do pr push"),
        "the held PR is never rebased: {fno}"
    );
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("\"skip_claim_held\":1"), "{events}");

    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    // Seven conflicting PRs, all on the one branch the git stub lists a
    // worktree for: the cap binds on the 7th, and PR 7 gets the skip.
    let mut prs = Vec::new();
    for n in 1..=7 {
        prs.push(format!(
            "{{\"number\":{n},\"head\":{{\"sha\":\"aaa1\",\"ref\":\"feature/x-1111\"}},\"base\":{{\"ref\":\"main\"}},\"mergeable\":false}}"
        ));
    }
    let listing = format!("[{}]", prs.join(","));
    let body = r#"#!/bin/sh
D="$(dirname "$0")"
echo "gh $*" >> "$D/gh.log"
for a in "$@"; do case "$a" in
  *'pulls?state=open'*) echo 'LISTING'
     exit 0 ;;
  *pulls/*) echo '{"head":{"sha":"aaa1","ref":"feature/x-1111","repo":{"full_name":"o/r"}},"base":{"ref":"main","repo":{"full_name":"o/r"}},"author_association":"OWNER","mergeable":false,"body":"b"}'; exit 0 ;;
  *check-runs) echo '{"check_runs":[]}'; exit 0 ;;
  */status) echo '{"statuses":[]}'; exit 0 ;;
esac; done
echo '[]'
"#
    .replace("LISTING", &listing);
    write_exec(d, "gh", &body);
    stub_git_drive(d);
    stub_cargo(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    stub_fno_push(
        d,
        "pr-push: origin/main behind-before=1 behind-after=0 preflight=absent sha=abc pushed=1",
        0,
        "",
    );
    run_heal(&drive_args(d, &[]));
    let fno = log_of(d, "fno.log");
    assert_eq!(fno.matches("do pr push").count(), 6, "{fno}");
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("\"skip_rebase_budget\":1"), "{events}");

    // A push that moved nothing is receipted skip_rebase_noop, never
    // rebased: no acted row, no per-run budget spent, and the checks of
    // the live sha classify below.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive_rebase(d, "false");
    stub_git_drive(d);
    stub_cargo(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    stub_fno_push(
        d,
        "pr-push: origin/main behind-before=3 behind-after=3 integrate=none sha=abc pushed=0 nothing to push",
        0,
        "",
    );
    run_heal(&drive_args(d, &[]));
    let fno = log_of(d, "fno.log");
    assert_eq!(fno.matches("do pr push").count(), 1, "{fno}");
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("\"skip_rebase_noop\":1"), "{events}");
    assert!(
        events.contains("\"action\":\"skip_rebase_noop\""),
        "{events}"
    );
    assert!(!events.contains("\"rebased\":1"), "{events}");
}

#[test]
fn rebase_cap_rows() {
    // A conflicting PR rebased within the last 24h is not rebased again:
    // no push runs and the receipt reads skip_rebase_cap. The same PR
    // holding merge-slot:main is exempt: the slot holder is next to merge,
    // so the push runs.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive_rebase(d, "false");
    stub_git_drive(d);
    stub_cargo(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    stub_fno_push(
        d,
        "pr-push: origin/main behind-before=3 behind-after=0 preflight=skipped sha=abc pushed=1",
        0,
        "",
    );
    // Seed one pr_heal_pr `rebased` row for PR 1, one hour old, the shape
    // the receipt closure writes (root stamped, per the tick row).
    let past = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .saturating_sub(3600);
    let (y, mo, dd, h, mi, s) = crate::events::civil_from_unix(past);
    let ts = format!("{y:04}-{mo:02}-{dd:02}T{h:02}:{mi:02}:{s:02}.000Z");
    let seed = |d: &Path, action: &str| {
        format!(
            "{{\"ts\":\"{ts}\",\"type\":\"pr_heal_pr\",\"source\":\"pr-heal\",\"data\":{{\"root\":\"{root}\",\"pr\":\"1\",\"action\":\"{action}\",\"reason\":\"x\"}}}}\n",
            root = d.to_string_lossy()
        )
    };
    std::fs::write(d.join("events.jsonl"), seed(d, "rebased")).unwrap();
    run_heal(&drive_args(d, &[]));
    let fno = log_of(d, "fno.log");
    assert!(
        !fno.contains("do pr push"),
        "the 24h cap skips the push: {fno}"
    );
    let events = log_of(d, "events.jsonl");
    assert!(
        events.contains("\"action\":\"skip_rebase_cap\""),
        "{events}"
    );
    assert!(!events.contains("\"skip_rebase_noop\":1"), "{events}");

    // The merge-slot exemption: the same journal, the same conflicting PR,
    // and the push runs.
    hold_slot_claim(d, 1);
    run_heal(&drive_args(d, &[]));
    let fno = log_of(d, "fno.log");
    assert_eq!(fno.matches("do pr push").count(), 1, "{fno}");

    // A still-conflicting rebase attempt is NOT capped: the owner may
    // resolve and need the retry, so the attempt runs and spends the
    // per-run budget instead.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive_rebase(d, "false");
    stub_git_drive(d);
    stub_cargo(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    stub_fno_push(
        d,
        "",
        3,
        "status needs_resolver; not safely rebasable; files: a, b). Resolve by hand, then re-run `fno do pr push`",
    );
    std::fs::write(d.join("events.jsonl"), seed(d, "skip_rebase_conflict")).unwrap();
    run_heal(&drive_args(d, &[]));
    let fno = log_of(d, "fno.log");
    assert_eq!(fno.matches("do pr push").count(), 1, "{fno}");
    let events = log_of(d, "events.jsonl");
    assert!(
        events.contains("\"action\":\"skip_rebase_conflict\""),
        "{events}"
    );
    assert!(
        !events.contains("\"action\":\"skip_rebase_cap\""),
        "{events}"
    );

    // The cap counts a no-op refresh (`skip_rebase_noop`) the same way.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive_rebase(d, "false");
    stub_git_drive(d);
    stub_cargo(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    stub_fno_push(
        d,
        "pr-push: origin/main behind-before=3 behind-after=3 integrate=none sha=abc pushed=0 nothing to push",
        0,
        "",
    );
    std::fs::write(d.join("events.jsonl"), seed(d, "skip_rebase_noop")).unwrap();
    run_heal(&drive_args(d, &[]));
    let fno = log_of(d, "fno.log");
    assert!(
        !fno.contains("do pr push"),
        "the cap covers a no-op refresh: {fno}"
    );
    let events = log_of(d, "events.jsonl");
    assert!(
        events.contains("\"action\":\"skip_rebase_cap\""),
        "{events}"
    );
}

/// The drive stub with PR 1's one red check a failing pytest shard: the
/// rerunnable-escalation class the widening exists for.
fn stub_gh_drive_pytest(dir: &Path) {
    let body = r#"#!/bin/sh
D="$(dirname "$0")"
echo "gh $*" >> "$D/gh.log"
for a in "$@"; do case "$a" in
  *'pulls?state=open'*)
     echo '[{"number":1,"head":{"sha":"aaa1","ref":"pytest-leak-guard"},"base":{"ref":"main"},"mergeable":null},{"number":2,"head":{"sha":"bbb2","ref":"feature/x-2222"},"base":{"ref":"main"},"mergeable":null}]'
     exit 0 ;;
  *pulls/1*) echo '{"head":{"sha":"aaa1","ref":"pytest-leak-guard","repo":{"full_name":"o/r"}},"base":{"ref":"main","repo":{"full_name":"o/r"}},"author_association":"OWNER","mergeable":null,"body":"b"}'; exit 0 ;;
  *pulls/2*) echo '{"head":{"sha":"bbb2","ref":"feature/x-2222","repo":{"full_name":"o/r"}},"base":{"ref":"main","repo":{"full_name":"o/r"}},"author_association":"OWNER","mergeable":null,"body":"b"}'; exit 0 ;;
  *check-runs) echo '{"check_runs":[{"name":"smoke-pytest (5)",CHECKROW}]}'; exit 0 ;;
  */logs) echo "FAILED tests/test_a.py::test_flaky_a"; exit 0 ;;
  */status) echo '{"statuses":[]}'; exit 0 ;;
esac; done
echo '[]'
"#
    .replace("pytest-leak-guard", "feature/x-1111")
    .replace(
        "CHECKROW",
        r#""status":"completed","conclusion":"failure","html_url":"https://github.com/o/r/actions/runs/777/job/9""#,
    );
    write_exec(dir, "gh", &body);
}

#[test]
fn pytest_rerun_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive_pytest(d);
    stub_git_drive(d);
    stub_cargo(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    stub_fno(d);
    hold_claim(d);
    let code = run_heal(&drive_args(d, &[]));
    assert_eq!(code, EXIT_CLEAN, "the rerun acted: {code}");
    let gh = log_of(d, "gh.log");
    assert_eq!(
        gh.matches("run rerun 777 --failed").count(),
        1,
        "exactly one --failed rerun: {gh}"
    );
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("\"reran\":1"), "{events}");
    assert!(events.contains("\"rerun_keys\":[\"aaa1:777\"]"), "{events}");

    // The dedup key is (sha, run id), not the check name: nine failing
    // checks from one workflow run are ONE rerun. Measured on PR 2162:
    // nine red checks, one run id, one `--failed` rerun covers all nine.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh(d, false);
    let mut findings: Vec<Finding> = (1..=9)
        .map(|i| Finding {
            check: format!("smoke-pytest ({i})"),
            signature: "unknown",
            remedy: Remedy::Escalate {
                repro: "unrecognized".to_string(),
            },
            link: format!("https://github.com/o/r/actions/runs/777/job/{i}"),
        })
        .collect();
    let args = parse_args(&args_for(d, &["--apply"])).unwrap();
    let issued = apply_rerun(&args, &mut findings, "aaa1");
    assert_eq!(issued, vec!["aaa1:777".to_string()]);
    // The printed command is the applied one, flag included.
    assert_eq!(findings[0].detail(), "gh run rerun 777 --failed");
    let gh = log_of(d, "gh.log");
    assert_eq!(
        gh.matches("run rerun 777 --failed").count(),
        1,
        "one rerun for nine checks: {gh}"
    );

    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive_pytest(d);
    stub_git_drive(d);
    stub_cargo(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    stub_fno(d);
    hold_claim(d);
    run_heal(&drive_args(d, &[]));
    run_heal(&drive_args(d, &[]));
    let gh = log_of(d, "gh.log");
    assert_eq!(
        gh.matches("run rerun 777 --failed").count(),
        1,
        "the rerun never re-issues: {gh}"
    );
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("\"still_red\":1"), "{events}");
}

/// The drive stub with PR 1 fully green on Actions run 777: the shape a
/// rerun that came back clean leaves behind.
fn stub_gh_drive_flake(d: &Path) {
    let body = r#"#!/bin/sh
D="$(dirname "$0")"
echo "gh $*" >> "$D/gh.log"
for a in "$@"; do case "$a" in
  *'pulls?state=open'*)
     echo '[{"number":1,"head":{"sha":"aaa1","ref":"feature/x-1111"},"base":{"ref":"main"},"mergeable":null},{"number":2,"head":{"sha":"bbb2","ref":"feature/x-2222"},"base":{"ref":"main"},"mergeable":null}]'
     exit 0 ;;
  *pulls/1*) echo '{"head":{"sha":"aaa1","ref":"feature/x-1111","repo":{"full_name":"o/r"}},"base":{"ref":"main","repo":{"full_name":"o/r"}},"author_association":"OWNER","mergeable":null,"body":"b"}'; exit 0 ;;
  *pulls/2*) echo '{"head":{"sha":"bbb2","ref":"feature/x-2222","repo":{"full_name":"o/r"}},"base":{"ref":"main","repo":{"full_name":"o/r"}},"author_association":"OWNER","mergeable":null,"body":"b"}'; exit 0 ;;
  *check-runs) echo '{"check_runs":[{"name":"ci","status":"completed","conclusion":"success","html_url":"https://github.com/o/r/actions/runs/777/job/9"}]}'; exit 0 ;;
  */logs) echo "all steps passed"; exit 0 ;;
  */status) echo '{"statuses":[]}'; exit 0 ;;
esac; done
echo '[]'
"#;
    write_exec(d, "gh", &body);
}

/// Seed one pr_heal_flake row for the `ci` key under a foreign guard.
/// The store dedupes by row_hash, so two occurrences need distinct
/// run ids to both commit.
fn seed_flake_row(dir: &Path, run: &str, node: Option<&str>) {
    let node_json = node
        .map(|n| format!(",\"node_id\":\"{n}\""))
        .unwrap_or_default();
    let row = format!(
        "{{\"ts\":\"2026-09-17T12:00:00Z\",\"type\":\"pr_heal_flake\",\"source\":\"pr-heal\",\"data\":{{\"key_guard\":\"old:1:ci\",\"key\":\"ci\",\"sha\":\"old\",\"run_id\":\"{run}\",\"check\":\"ci\"{node_json}}}}}"
    );
    let path = dir.join("events.jsonl");
    crate::event_store::append_envelope(&path, row.trim_end(), None).unwrap();
}

/// Seed the journal with a tick row carrying rerun keys.
fn seed_tick_with_keys(dir: &Path, keys: &[&str]) {
    let row = format!(
        "{{\"ts\":\"2026-09-17T12:00:00Z\",\"type\":\"pr_heal_tick\",\"data\":{{\"rerun_keys\":{}}}}}\n",
        serde_json::json!(keys)
    );
    std::fs::write(dir.join("events.jsonl"), row).unwrap();
}

#[test]
fn flake_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive_flake(d);
    stub_git_drive(d);
    stub_cargo(d);
    stub_fno(d);
    hold_claim(d);
    seed_tick_with_keys(d, &["aaa1:777"]);
    run_heal(&drive_args(d, &[]));
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("pr_heal_flake"), "{events}");
    assert!(events.contains("\"key_guard\":\"aaa1:777:ci\""), "{events}");
    assert!(events.contains("\"key\":\"ci\""), "{events}");

    // Two prior rows for key ci, no node yet: the third green rerun
    // files exactly one node through `fno backlog idea`.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive_flake(d);
    stub_git_drive(d);
    stub_cargo(d);
    stub_fno_push(d, "", 0, "");
    hold_claim(d);
    seed_tick_with_keys(d, &["aaa1:777"]);
    seed_flake_row(d, "1", None);
    seed_flake_row(d, "2", None);
    run_heal(&drive_args(d, &[]));
    let fno = log_of(d, "fno.log");
    assert_eq!(fno.matches("backlog idea").count(), 1, "{fno}");
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("\"node_id\":\"fno-abc9\""), "{events}");

    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive_flake(d);
    stub_git_drive(d);
    stub_cargo(d);
    stub_fno_push(d, "", 0, "");
    hold_claim(d);
    seed_tick_with_keys(d, &["aaa1:777"]);
    seed_flake_row(d, "1", Some("fno-abc9"));
    run_heal(&drive_args(d, &[]));
    let fno = log_of(d, "fno.log");
    assert!(!fno.contains("backlog idea"), "{fno}");

    let text = playbook();
    assert!(text.contains("rerunnable"), "{text}");
    assert!(text.contains("yes"), "{text}");
    assert!(text.contains("no "), "{text}");
}

// ── the detached drive loop ───────────────────────────────────────────

fn detach_args(dir: &Path) -> Vec<String> {
    drive_args(dir, &["--detach"])
}

/// The clear verdict, as a closure the detach runner reads; the armed
/// test passes its own.
fn clear_pause() -> crate::loops_pause::DispatchPause {
    crate::loops_pause::DispatchPause::Clear
}

#[test]
fn detach_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let a = parse_args(&detach_args(d)).unwrap();
    let argv_sent = detach_args(d);
    let spawned = std::cell::RefCell::new(Vec::<Vec<String>>::new());
    let spawn = |argv: &[String]| -> std::io::Result<u32> {
        spawned.borrow_mut().push(argv.to_vec());
        Ok(std::process::id())
    };
    let code = run_detached(&a, &argv_sent, &clear_pause, &spawn);
    assert_eq!(code, EXIT_CLEAN);
    let pid_file = heal_pid_file(&a);
    assert_eq!(
        read_pid_file(&pid_file),
        Some(std::process::id()),
        "the pid file names the child"
    );
    let calls = spawned.borrow();
    assert_eq!(calls.len(), 1, "one spawn: {calls:?}");
    let child = &calls[0];
    assert_eq!(child[1], "pr-heal", "the child argv re-adds the verb");
    assert!(
        !child.iter().any(|s| s == "--detach"),
        "the child runs the real loop: {child:?}"
    );
    assert!(
        child
            .windows(2)
            .any(|w| w[0] == "--cwd" && Path::new(&w[1]) == d),
        "the child heals this root: {child:?}"
    );
    drop(calls);
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("control_plane_tick"), "{events}");
    assert!(events.contains("\"arm\":\"heal\""), "{events}");
    assert!(events.contains("\"acted\":1"), "{events}");

    // A drive loop still running when the next tick fires is skipped,
    // never double-spawned (the in-flight case the pid file exists for).
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let a = parse_args(&detach_args(d)).unwrap();
    let pid_file = heal_pid_file(&a);
    std::fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
    std::fs::write(&pid_file, format!("{}\n", std::process::id())).unwrap();
    let spawned = std::cell::RefCell::new(0);
    let spawn = |_: &[String]| -> std::io::Result<u32> {
        *spawned.borrow_mut() += 1;
        Ok(std::process::id())
    };
    let code = run_detached(&a, &detach_args(d), &clear_pause, &spawn);
    assert_eq!(code, EXIT_CLEAN);
    assert_eq!(*spawned.borrow(), 0, "never double-spawned");
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("\"acted\":0"), "{events}");
    assert!(events.contains("in_flight"), "{events}");

    // The breaker exists to stop spawns: the drive loop obeys it on its
    // own arm row instead of feeding the admission gate a spawn to
    // refuse, and the tick still completes its other legs.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let a = parse_args(&detach_args(d)).unwrap();
    let spawned = std::cell::RefCell::new(0);
    let spawn = |_: &[String]| -> std::io::Result<u32> {
        *spawned.borrow_mut() += 1;
        Ok(std::process::id())
    };
    let armed = || crate::loops_pause::DispatchPause::FleetIncident {
        generation: 9,
        reason: "rustc storm".to_string(),
        holds: vec!["spawns".to_string(), "tests".to_string()],
    };
    let code = run_detached(&a, &detach_args(d), &armed, &spawn);
    assert_eq!(code, EXIT_CLEAN);
    assert_eq!(*spawned.borrow(), 0, "nothing spawned under the breaker");
    assert!(
        read_pid_file(&heal_pid_file(&a)).is_none(),
        "no pid file under an armed breaker"
    );
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("\"arm\":\"heal\""), "{events}");
    assert!(events.contains("\"acted\":0"), "{events}");
    assert!(events.contains("fleet_stop"), "{events}");
    assert!(events.contains("generation 9"), "{events}");

    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    // A real, now-dead pid: spawned, waited, gone.
    let dead = {
        let mut c = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        pid
    };
    let a = parse_args(&detach_args(d)).unwrap();
    let pid_file = heal_pid_file(&a);
    std::fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
    std::fs::write(&pid_file, format!("{dead}\n")).unwrap();
    let spawned = std::cell::RefCell::new(0);
    let spawn = |_: &[String]| -> std::io::Result<u32> {
        *spawned.borrow_mut() += 1;
        Ok(std::process::id())
    };
    let code = run_detached(&a, &detach_args(d), &clear_pause, &spawn);
    assert_eq!(code, EXIT_CLEAN);
    assert_eq!(
        *spawned.borrow(),
        1,
        "a dead pid is never mistaken for a live loop"
    );
    assert_eq!(
        read_pid_file(&heal_pid_file(&a)),
        Some(std::process::id()),
        "the pid file names the new child"
    );

    // The per-root Python loop is gone: one spawn whose child argv
    // carries every --cwd, one pid file. The second root rides
    // drive_args' own --cwd plus one more.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let other = tempfile::tempdir().unwrap();
    let other_s = other.path().to_str().unwrap();
    let argv = drive_args(d, &["--detach", "--cwd", other_s]);
    let a = parse_args(&argv).unwrap();
    assert_eq!(
        a.roots,
        vec![d.to_path_buf(), other.path().to_path_buf()],
        "both roots parsed, deduplicated"
    );
    let spawned = std::cell::RefCell::new(Vec::<Vec<String>>::new());
    let spawn = |argv: &[String]| -> std::io::Result<u32> {
        spawned.borrow_mut().push(argv.to_vec());
        Ok(std::process::id())
    };
    let code = run_detached(&a, &argv, &clear_pause, &spawn);
    assert_eq!(code, EXIT_CLEAN);
    let calls = spawned.borrow();
    assert_eq!(calls.len(), 1, "one spawn: {calls:?}");
    let child = &calls[0];
    for root in [d, other.path()] {
        assert!(
            child
                .windows(2)
                .any(|w| w[0] == "--cwd" && Path::new(&w[1]) == root),
            "the child carries --cwd {root:?}: {child:?}"
        );
    }
    drop(calls);
    assert_eq!(
        heal_pid_file(&a),
        events_dir(&a).join("heal").join("pr-heal.pid"),
        "one root-free pid file, in its own subfolder"
    );
    assert_eq!(
        read_pid_file(&heal_pid_file(&a)),
        Some(std::process::id()),
        "one pid file written"
    );

    // The in-flight guard reads the ONE pid file whatever root asked:
    // the per-root files never let it see a loop started for a root
    // other than its own.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let other = tempfile::tempdir().unwrap();
    let other_s = other.path().to_str().unwrap();
    let argv = drive_args(d, &["--detach", "--cwd", other_s]);
    let a = parse_args(&argv).unwrap();
    let pid_file = heal_pid_file(&a);
    std::fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
    std::fs::write(&pid_file, format!("{}\n", std::process::id())).unwrap();
    let spawned = std::cell::RefCell::new(0);
    let spawn = |_: &[String]| -> std::io::Result<u32> {
        *spawned.borrow_mut() += 1;
        Ok(std::process::id())
    };
    let code = run_detached(&a, &argv, &clear_pause, &spawn);
    assert_eq!(code, EXIT_CLEAN);
    assert_eq!(*spawned.borrow(), 0, "never double-spawned");
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("\"acted\":0"), "{events}");
    assert!(events.contains("in_flight"), "{events}");

    assert_eq!(
        run_heal(&["--detach".to_string()]),
        EXIT_READ_ERROR,
        "--detach without --all --apply has nothing to detach"
    );
}

#[test]
fn tick_rows() {
    // Deadline already spent: no PR is read, every PR gets one
    // skip_deadline receipt, and the tick row carries the count.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let listing = format!(
        "[{},{},{}]",
        r#"{"number":1,"head":{"sha":"aaa1","ref":"feature/x-1111"},"base":{"ref":"main"},"mergeable":null}"#,
        r#"{"number":2,"head":{"sha":"bbb2","ref":"feature/x-2222"},"base":{"ref":"main"},"mergeable":null}"#,
        r#"{"number":3,"head":{"sha":"ccc3","ref":"feature/x-3333"},"base":{"ref":"main"},"mergeable":null}"#,
    );
    let body = r#"#!/bin/sh
D="$(dirname "$0")"
echo "gh $*" >> "$D/gh.log"
for a in "$@"; do case "$a" in
  *'pulls?state=open'*) echo 'LISTING'; exit 0 ;;
esac; done
echo '[]'
"#
    .replace("LISTING", &listing);
    write_exec(d, "gh", &body);
    stub_git_drive(d);
    stub_cargo(d);
    let mut a = parse_args(&drive_args(d, &[])).unwrap();
    a.deadline = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
    run_all_apply(&a, false);
    let gh = log_of(d, "gh.log");
    assert!(
        !gh.contains("pulls/"),
        "no PR is read past a spent deadline: {gh}"
    );
    let events = log_of(d, "events.jsonl");
    assert_eq!(
        events.matches("\"action\":\"skip_deadline\"").count(),
        3,
        "one receipt per unreached PR: {events}"
    );
    assert!(events.contains("\"skip_deadline\":3"), "{events}");

    // One process, one pr_heal_tick row per root, folded to the worst
    // verdict. Instant stubs and a generous budget: no timing dependence
    // (the skip itself is covered by the spent-deadline tests). The budget
    // sits above `read_bound` for every root, since the admission gate
    // refuses a root whose remaining share cannot fit one pre-loop read.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let roots: Vec<std::path::PathBuf> = (0..3)
        .map(|i| {
            let p = d.join(format!("root-{i}"));
            std::fs::create_dir_all(&p).unwrap();
            p
        })
        .collect();
    let body = r#"#!/bin/sh
D="$(dirname "$0")"
echo "gh $*" >> "$D/gh.log"
pwd >> "$D/gh.order"
for a in "$@"; do case "$a" in
  *'pulls?state=open'*) echo '[]'; exit 0 ;;
esac; done
echo '[]'
"#;
    write_exec(d, "gh", body);
    stub_git_drive(d);
    stub_cargo(d);
    let mut a = parse_args(&drive_args(d, &[])).unwrap();
    a.roots = roots.clone();
    a.deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(480));
    let code = run_roots_apply(&a, false);
    assert_eq!(code, EXIT_CLEAN);
    let events = log_of(d, "events.jsonl");
    for root in &roots {
        let row = events
            .lines()
            .find(|l| l.contains("pr_heal_tick") && l.contains(root.to_str().unwrap()))
            .unwrap_or_else(|| panic!("no tick row for {root:?}: {events}"));
        assert!(row.contains("\"dry_run\":false"), "{row}");
    }
    // Empty journal: heal order is the `--cwd` order.
    let order = log_of(d, "gh.order");
    let listed: Vec<&str> = order
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(
        listed,
        vec!["root-0", "root-1", "root-2"],
        "input order on a fresh journal: {order}"
    );

    let tmp = tempfile::tempdir().unwrap();
    let a = parse_args(&args_for(tmp.path(), &[])).unwrap();
    assert_eq!(remedy_timeout(&a), REMEDY_TIMEOUT, "no deadline: unchanged");
    let mut short = a.clone();
    short.deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(10));
    assert!(
        remedy_timeout(&short) <= std::time::Duration::from_secs(10),
        "10s left: never 300s"
    );
    let mut spent = a.clone();
    spent.deadline = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
    assert_eq!(
        remedy_timeout(&spent),
        std::time::Duration::from_secs(1),
        "spent deadline: the 1s floor"
    );

    // One --cwd behaves as today, except the pr_heal_tick row now names
    // its root: the field the two-probe done-check reads.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_drive_rebase(d, "null");
    stub_git_drive(d);
    stub_cargo(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    stub_fno_push(d, "", 0, "");
    run_heal(&drive_args(d, &[]));
    let events = log_of(d, "events.jsonl");
    assert!(
        events.contains(&format!("\"root\":\"{}\"", d.to_str().unwrap())),
        "{events}"
    );

    // Migration guard: a pre-upgrade loop still alive in an old
    // pr-heal.<root>.pid file holds the tick, so a binary swap cannot
    // double-spawn the healer while the old loop lives.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let a = parse_args(&detach_args(d)).unwrap();
    std::fs::write(
        events_dir(&a).join("pr-heal.some-root.pid"),
        format!("{}\n", std::process::id()),
    )
    .unwrap();
    let spawned = std::cell::RefCell::new(0);
    let spawn = |_: &[String]| -> std::io::Result<u32> {
        *spawned.borrow_mut() += 1;
        Ok(std::process::id())
    };
    let code = run_detached(&a, &detach_args(d), &clear_pause, &spawn);
    assert_eq!(code, EXIT_CLEAN);
    assert_eq!(
        *spawned.borrow(),
        0,
        "a live stale-file loop holds the tick"
    );
    let events = log_of(d, "events.jsonl");
    assert!(events.contains("in_flight"), "{events}");

    // A stale pid file outlives its loop and pids are recycled: the pid
    // in an old file may belong to any unrelated process. `ps` names the
    // owner; a non-healer owner never holds the tick (the P1 codex
    // finding: otherwise one stale file disables healing forever).
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let a = parse_args(&detach_args(d)).unwrap();
    let sleeper = {
        let c = std::process::Command::new("/bin/sleep")
            .arg("15")
            .spawn()
            .unwrap();
        let pid = c.id();
        // Leave it running under its own pid for the ps read.
        std::mem::forget(c);
        pid
    };
    std::fs::write(
        events_dir(&a).join("pr-heal.old-root.pid"),
        format!("{sleeper}\n"),
    )
    .unwrap();
    let spawned = std::cell::RefCell::new(0);
    let spawn = |_: &[String]| -> std::io::Result<u32> {
        *spawned.borrow_mut() += 1;
        Ok(std::process::id())
    };
    let code = run_detached(&a, &detach_args(d), &clear_pause, &spawn);
    assert_eq!(code, EXIT_CLEAN);
    assert_eq!(
        *spawned.borrow(),
        1,
        "a recycled pid naming no healer never holds the tick"
    );

    // Zero slice, zero work: an exhausted budget runs no listing, no
    // worktree scan, no main-HEAD read for any root, and writes no tick
    // row (AC2 corner codex raised: the reads cost up to 60s each).
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let ra = d.join("root-a");
    let rb = d.join("root-b");
    std::fs::create_dir_all(&ra).unwrap();
    std::fs::create_dir_all(&rb).unwrap();
    let body = r#"#!/bin/sh
D="$(dirname "$0")"
echo "gh $*" >> "$D/gh.log"
exit 0
"#;
    write_exec(d, "gh", body);
    stub_git_drive(d);
    stub_cargo(d);
    let mut a = parse_args(&drive_args(d, &[])).unwrap();
    a.roots = vec![ra, rb];
    a.deadline = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
    let code = run_roots_apply(&a, false);
    assert_eq!(code, EXIT_CLEAN);
    assert_eq!(log_of(d, "gh.log"), "", "no pre-loop read past the budget");
    let events = log_of(d, "events.jsonl");
    assert!(
        !events.contains("pr_heal_tick"),
        "no tick row for a root that never ran: {events}"
    );
}

#[test]
fn status_rows() {
    let a = parse_args(&["--status".to_string()]).unwrap();
    assert_eq!(
        status_line(&a),
        "Heal: unarmed (auto_heal.enabled=false; arm with: fno config set auto_heal.enabled true)"
    );

    let tmp = tempfile::tempdir().unwrap();
    let a = parse_args(&[
        "--status".to_string(),
        "--armed".to_string(),
        "--events-file".to_string(),
        tmp.path()
            .join("events.jsonl")
            .to_string_lossy()
            .into_owned(),
    ])
    .unwrap();
    assert_eq!(status_line(&a), "Heal: armed; never ran");

    let tmp = tempfile::tempdir().unwrap();
    let events = tmp.path().join("events.jsonl");
    let ts = (chrono::Utc::now() - chrono::Duration::minutes(12) - chrono::Duration::seconds(5))
        .to_rfc3339();
    std::fs::write(
        &events,
        format!(
            "{{\"ts\":\"{ts}\",\"type\":\"pr_heal_tick\",\"data\":{{\"healed\":1,\"escalated\":3}}}}\n"
        ),
    )
    .unwrap();
    let a = parse_args(&[
        "--status".to_string(),
        "--armed".to_string(),
        "--events-file".to_string(),
        events.to_string_lossy().into_owned(),
    ])
    .unwrap();
    let line = status_line(&a);
    assert!(line.starts_with("Heal: armed; last run "), "{line}");
    assert!(line.contains("(12m ago, root unknown)"), "{line}");
    assert!(
        line.contains("healed 1, rebased 0, reran 0, escalated 3"),
        "{line}"
    );
    assert!(line.contains("acted on nothing"), "{line}");
    assert!(line.contains("in-flight none"), "{line}");

    let tmp = tempfile::tempdir().unwrap();
    let events = tmp.path().join("events.jsonl");
    let ts = chrono::Utc::now().to_rfc3339();
    std::fs::write(
        &events,
        format!(
            "{{\"ts\":\"{ts}\",\"type\":\"pr_heal_tick\",\"data\":{{\"healed\":0,\"rebased\":1,\"reran\":2,\"escalated\":1,\"acted_prs\":[2155,2162]}}}}\n"
        ),
    )
    .unwrap();
    let a = parse_args(&[
        "--status".to_string(),
        "--armed".to_string(),
        "--events-file".to_string(),
        events.to_string_lossy().into_owned(),
    ])
    .unwrap();
    let line = status_line(&a);
    assert!(
        line.contains("healed 0, rebased 1, reran 2, escalated 1; acted on PR 2155, 2162"),
        "{line}"
    );

    let tmp = tempfile::tempdir().unwrap();
    let events = tmp.path().join("events.jsonl");
    let ts = (chrono::Utc::now() - chrono::Duration::hours(30)).to_rfc3339();
    std::fs::write(
        &events,
        format!("{{\"ts\":\"{ts}\",\"type\":\"pr_heal_tick\",\"data\":{{}}}}\n"),
    )
    .unwrap();
    let a = parse_args(&[
        "--status".to_string(),
        "--armed".to_string(),
        "--events-file".to_string(),
        events.to_string_lossy().into_owned(),
    ])
    .unwrap();
    assert!(
        status_line(&a).contains("(1d ago, root unknown)"),
        "{}",
        status_line(&a)
    );

    let tmp = tempfile::tempdir().unwrap();
    let events = tmp.path().join("events.jsonl");
    let ts = chrono::Utc::now().to_rfc3339();
    std::fs::write(
        &events,
        format!("{{\"ts\":\"{ts}\",\"type\":\"pr_heal_tick\",\"data\":{{}}}}\n"),
    )
    .unwrap();
    let a = parse_args(&[
        "--status".to_string(),
        "--armed".to_string(),
        "--events-file".to_string(),
        events.to_string_lossy().into_owned(),
    ])
    .unwrap();
    let pid_file = heal_pid_file(&a);
    std::fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
    std::fs::write(&pid_file, format!("{}\n", std::process::id())).unwrap();
    assert!(
        status_line(&a).contains(&format!("in-flight {}", std::process::id())),
        "{}",
        status_line(&a)
    );

    // One row names one root's run: the line labels whose counts these
    // are instead of letting the last root's row read as the whole tick.
    let tmp = tempfile::tempdir().unwrap();
    let events = tmp.path().join("events.jsonl");
    let ts = chrono::Utc::now().to_rfc3339();
    std::fs::write(
        &events,
        format!(
            "{{\"ts\":\"{ts}\",\"type\":\"pr_heal_tick\",\"data\":{{\"root\":\"/srv/repo\",\"healed\":2}}}}\n"
        ),
    )
    .unwrap();
    let a = parse_args(&[
        "--status".to_string(),
        "--armed".to_string(),
        "--events-file".to_string(),
        events.to_string_lossy().into_owned(),
    ])
    .unwrap();
    let line = status_line(&a);
    assert!(line.contains("root /srv/repo"), "{line}");
    assert!(line.contains("healed 2"), "{line}");
}

#[test]
fn roots_rotate_to_the_least_recently_healed() {
    // The journal is the rotation state: a root the last tick row names
    // goes behind a root it does not name, so a root one tick skipped
    // (the starvation shape: an earlier root's reads ate the budget)
    // leads the next. The tick row names the root; the listing order
    // proves the heal order.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let ra = d.join("root-a");
    let rb = d.join("root-expect-second");
    std::fs::create_dir_all(ra.join(".git")).unwrap();
    std::fs::create_dir_all(rb.join(".git")).unwrap();
    std::fs::write(
        d.join("events.jsonl"),
        format!(
            "{{\"ts\":\"2020-01-01T00:00:00Z\",\"type\":\"pr_heal_tick\",\"data\":{{\"root\":\"{}\",\"healed\":1}}}}\n",
            ra.display()
        ),
    )
    .unwrap();
    let body = r#"#!/bin/sh
D="$(dirname "$0")"
echo "gh $*" >> "$D/gh.log"
pwd >> "$D/gh.order"
for a in "$@"; do case "$a" in
  *'pulls?state=open'*) echo '[]'; exit 0 ;;
esac; done
echo '[]'
"#;
    write_exec(d, "gh", body);
    stub_git_drive(d);
    stub_cargo(d);
    let mut a = parse_args(&drive_args(d, &[])).unwrap();
    a.roots = vec![ra, rb];
    a.deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(480));
    let code = run_roots_apply(&a, false);
    assert_eq!(code, EXIT_CLEAN);
    let order = log_of(d, "gh.order");
    let listed: Vec<&str> = order
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(
        listed,
        vec!["root-expect-second", "root-a"],
        "the never-healed root leads: {order}"
    );
}

#[test]
fn overrun_leaves_the_next_root_unstarted() {
    // A root whose listing read overruns its slice pays for it alone:
    // the next root's admission gate refuses to start a listing it
    // cannot fit, and waits for the next tick having done no read. The
    // old split started the read anyway, paying up to one READ_TIMEOUT
    // past the slice for zero work -- the shape that starved footnote
    // behind fnh for a day.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let ra = d.join("root-a");
    let rb = d.join("root-b");
    std::fs::create_dir_all(ra.join(".git")).unwrap();
    std::fs::create_dir_all(rb.join(".git")).unwrap();
    let body = r#"#!/bin/sh
D="$(dirname "$0")"
echo "gh $*" >> "$D/gh.log"
pwd >> "$D/gh.order"
for a in "$@"; do case "$a" in
  *'pulls?state=open'*) sleep 3.5; echo '[]'; exit 0 ;;
esac; done
echo '[]'
"#;
    write_exec(d, "gh", body);
    stub_git_drive(d);
    stub_cargo(d);
    let mut a = parse_args(&drive_args(d, &[])).unwrap();
    a.roots = vec![ra, rb];
    a.read_bound = std::time::Duration::from_secs(1);
    a.deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(4));
    let code = run_roots_apply(&a, false);
    assert_eq!(code, EXIT_CLEAN);
    let order = log_of(d, "gh.order");
    let listed: Vec<&str> = order
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(
        listed,
        vec!["root-a"],
        "the overrunning root alone listed; the next root never started: {order}"
    );
    let events = log_of(d, "events.jsonl");
    assert!(
        events.contains("pr_heal_tick") && events.contains("root-a"),
        "the first root still writes its tick row: {events}"
    );
    assert!(
        !events.contains("root-b"),
        "the skipped root writes no tick row: {events}"
    );
}
