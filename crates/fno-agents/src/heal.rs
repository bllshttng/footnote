//! `fno-agents pr-heal` -- classify a red check by signature, apply the
//! canonical fix, push once.
//!
//! Everything after a push already had a reader (`fno do pr status` names the
//! failing step, `fno do pr logs` spools its log, `loop-check` knows whether
//! main's HEAD is red on the same check). Nothing acted on what those read, so
//! a red check cost a hand-driven fix-and-repush round every time. This module
//! is the actor: one signature table, three mechanical remedies, one push, and
//! an honest escalation carrying a local repro for everything else.
//!
//! The table is the single source for both classification and `--playbook`, so
//! a signature can never be documented one way and matched another.
//!
//! Two properties are load-bearing and are what the tests pin:
//!
//! * **A run in flight is never pushed over.** One session cancelled seven
//!   in-flight runs by pushing three times; heal pushes exactly once, and only
//!   after re-reading the checks.
//! * **A failure inherited from main is never counted against the PR.** It is
//!   reported and left alone -- fixing it here would put main's problem in
//!   someone else's diff.

use regex::Regex;
use serde_json::Value;

/// A command a remedy runs, with the repo-relative directory it runs in.
/// `cwd` is empty for the repo root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cmd {
    pub cwd: String,
    pub argv: Vec<String>,
}

impl Cmd {
    fn new(cwd: &str, argv: &[&str]) -> Self {
        Cmd {
            cwd: cwd.to_string(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// The command as an operator would type it, for the report.
    pub(crate) fn render(&self) -> String {
        let joined = self.argv.join(" ");
        if self.cwd.is_empty() {
            joined
        } else {
            format!("cd {} && {joined}", self.cwd)
        }
    }
}

/// What heal does about one failing check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Remedy {
    /// Run `run`, then `verify`. A failed verify demotes the row to
    /// [`Remedy::Escalate`] rather than committing a fix that did not work.
    Auto { run: Vec<Cmd>, verify: Vec<Cmd> },
    /// Append a closure trailer per node id to the PR body. No commit, no
    /// push: the workflow's `types` includes `edited`, so the edit re-fires it.
    EditBody { nodes: Vec<String> },
    /// Not mechanically fixable. `repro` is the command that reproduces it
    /// locally, which is the whole value of the row.
    Escalate { repro: String },
    /// The same check is red on main's HEAD. Reported, never remedied, never
    /// counted against this PR.
    Inherited,
}

/// One classified failing check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    pub check: String,
    pub signature: &'static str,
    pub remedy: Remedy,
}

impl Finding {
    /// The report's action column.
    pub(crate) fn action(&self) -> &'static str {
        match self.remedy {
            Remedy::Auto { .. } => "auto",
            Remedy::EditBody { .. } => "edit-body",
            Remedy::Escalate { .. } => "escalate",
            Remedy::Inherited => "inherited",
        }
    }

    /// The report's remedy column: what heal will run, or how to reproduce.
    pub(crate) fn detail(&self) -> String {
        match &self.remedy {
            Remedy::Auto { run, .. } => {
                run.iter().map(Cmd::render).collect::<Vec<_>>().join(" && ")
            }
            Remedy::EditBody { nodes } => {
                format!("fno do pr closure-trailer {}", nodes.join(" "))
            }
            Remedy::Escalate { repro } => repro.clone(),
            Remedy::Inherited => "inherited from main; not this PR's to fix".to_string(),
        }
    }

    /// True when this row is the PR's own problem. `inherited` rows are red
    /// but not the PR's, so they never decide heal's exit code.
    pub(crate) fn counts_against_pr(&self) -> bool {
        !matches!(self.remedy, Remedy::Inherited)
    }
}

/// What a signature matches against. `log` arrives already stripped of the
/// runner's timestamp prefix, so every pattern below reads like the text a
/// human sees in the job's own output.
pub(crate) struct Ctx<'a> {
    pub check: &'a str,
    pub workflow: &'a str,
    pub log: &'a str,
}

/// A signature: how a class of failure is recognized, and what to do about it.
/// `plan` is the `--playbook` column, so the documented remedy and the applied
/// one are the same string's neighbours in one table.
struct Signature {
    name: &'static str,
    plan: &'static str,
    matches: fn(&Ctx) -> bool,
    resolve: fn(&Ctx) -> Remedy,
}

/// The pinned rustfmt toolchain. Keep in lockstep with `PINNED_FMT` in
/// `scripts/ci/preflight.sh` and `RUSTFMT_TOOLCHAIN` in `rust-ci.yml`: a fix
/// applied by a different rustfmt is drift of its own.
const PINNED_FMT: &str = "+1.94.1";

/// First match wins. Ordered so a narrower class is asked before a wider one:
/// ruff and mypy both name a `.py` line, and the guard-script catch-all would
/// otherwise swallow anything a guard printed alongside a real failure.
const SIGNATURES: &[Signature] = &[
    Signature {
        name: "rustfmt-drift",
        plan: "auto: cargo +1.94.1 fmt --all in each crate rustfmt named",
        matches: |c| c.check.contains("cargo fmt --check") || fmt_crates(c.log).is_some(),
        resolve: |c| {
            let crates = fmt_crates(c.log).unwrap_or_else(default_fmt_crates);
            Remedy::Auto {
                run: crates
                    .iter()
                    .map(|dir| Cmd::new(dir, &["cargo", PINNED_FMT, "fmt", "--all"]))
                    .collect(),
                verify: crates
                    .iter()
                    .map(|dir| Cmd::new(dir, &["cargo", PINNED_FMT, "fmt", "--all", "--check"]))
                    .collect(),
            }
        },
    },
    Signature {
        name: "closure-trailer",
        plan: "edit-body: append a Backlog-Closure trailer per node the branch names",
        matches: |c| {
            c.check.contains("check-pr-node-closure")
                && c.log.contains("the exact trailer claims none")
        },
        resolve: |c| Remedy::EditBody {
            nodes: closure_nodes(c.log),
        },
    },
    Signature {
        name: "ruff-lint",
        plan: "auto: ruff check --fix + ruff format over cli/src and cli/tests",
        matches: |c| ruff_re().is_match(c.log),
        resolve: |_| Remedy::Auto {
            run: vec![
                Cmd::new(
                    "cli",
                    &["uv", "run", "ruff", "check", "--fix", "src", "tests"],
                ),
                Cmd::new("cli", &["uv", "run", "ruff", "format", "src", "tests"]),
            ],
            verify: vec![Cmd::new(
                "cli",
                &["uv", "run", "ruff", "check", "src", "tests"],
            )],
        },
    },
    Signature {
        name: "mypy",
        plan: "escalate: a type error is a decision, never a mechanical rewrite",
        matches: |c| mypy_re().is_match(c.log),
        resolve: |_| Remedy::Escalate {
            repro: "cd cli && uv run mypy src/".to_string(),
        },
    },
    Signature {
        name: "pytest",
        plan: "escalate: repro names the failing node ids",
        matches: |c| !pytest_nodeids(c.log).is_empty(),
        resolve: |c| Remedy::Escalate {
            repro: format!(
                "cd cli && uv run pytest {}",
                pytest_nodeids(c.log).join(" ")
            ),
        },
    },
    Signature {
        name: "cargo-test",
        plan: "escalate: repro names the failing test path",
        matches: |c| c.check.contains("cargo test") || !cargo_test_names(c.log).is_empty(),
        resolve: |c| {
            let names = cargo_test_names(c.log);
            let crate_dir = fmt_crates(c.log)
                .and_then(|dirs| dirs.into_iter().next())
                .unwrap_or_else(|| "crates/fno-agents".to_string());
            let mut repro = format!("cd {crate_dir} && cargo test --lib --bins");
            if !names.is_empty() {
                repro.push_str(&format!("  # failed: {}", names.join(", ")));
            }
            Remedy::Escalate { repro }
        },
    },
    Signature {
        name: "guard-script",
        plan: "escalate: repro is the guard's own script",
        matches: |c| guard_script(c.log).is_some(),
        resolve: |c| Remedy::Escalate {
            repro: match guard_script(c.log) {
                Some(name) => format!("bash scripts/ci/{name}.sh"),
                // `matches` already answered yes, so this arm is unreachable;
                // naming the log rather than panicking keeps a classifier that
                // never aborts a heal run.
                None => "bash scripts/ci/  # see the log".to_string(),
            },
        },
    },
];

/// The remedy for a check nothing in [`SIGNATURES`] recognized: the log's tail
/// and the one instruction that retires the class.
fn unknown_remedy(log: &str) -> Remedy {
    let tail: Vec<&str> = log.lines().rev().take(TAIL_LINES).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    Remedy::Escalate {
        repro: format!(
            "unrecognized; add a signature to heal.rs. Last {} log lines:\n{}",
            tail.len(),
            tail.join("\n")
        ),
    }
}

/// How much of an unrecognized log the report carries. Matches `fno do pr
/// logs`'s own tail, so the two readers show an operator the same window.
const TAIL_LINES: usize = 40;

/// Classify one failing check. `inherited` comes from the caller's read of
/// main's HEAD and is checked FIRST: a check that main is already failing is
/// not this PR's, whatever its log happens to match.
pub(crate) fn classify(ctx: &Ctx, inherited: bool) -> Finding {
    if inherited {
        return Finding {
            check: ctx.check.to_string(),
            signature: "inherited",
            remedy: Remedy::Inherited,
        };
    }
    for sig in SIGNATURES {
        if (sig.matches)(ctx) {
            return Finding {
                check: ctx.check.to_string(),
                signature: sig.name,
                remedy: (sig.resolve)(ctx),
            };
        }
    }
    Finding {
        check: ctx.check.to_string(),
        signature: "unknown",
        remedy: unknown_remedy(ctx.log),
    }
}

/// The playbook: every signature and its remedy, from the same table
/// `classify` walks. Printed by `--playbook`; deliberately not duplicated into
/// a doc, so the two can never disagree.
pub(crate) fn playbook() -> String {
    let mut out = String::from("signature       remedy\n");
    for sig in SIGNATURES {
        out.push_str(&format!("{:<15} {}\n", sig.name, sig.plan));
    }
    out.push_str(&format!(
        "{:<15} report only; the same check is red on main HEAD\n",
        "inherited"
    ));
    out.push_str(&format!(
        "{:<15} escalate with the last {TAIL_LINES} log lines\n",
        "unknown"
    ));
    out
}

// ── log readers ─────────────────────────────────────────────────────────────

/// Drop the runner's ISO-8601 timestamp prefix from every line. Without this
/// every pattern below would have to carry the prefix, and an anchored one
/// (`^FAILED`, `^test `) could not match at all.
pub(crate) fn strip_timestamps(raw: &str) -> String {
    let ts = Regex::new(r"^\d{4}-\d{2}-\d{2}T[\d:.]+Z ").expect("static regex");
    raw.lines()
        .map(|line| ts.replace(line, "").into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The crates rustfmt named, in first-seen order. rustfmt prints an ABSOLUTE
/// path (`Diff in /home/runner/work/footnote/footnote/crates/fno/src/x.rs`),
/// so the crate is read out of the path rather than guessed from the check
/// name -- the fmt job is ONE check covering both crates, and its name carries
/// `(pinned)`, never the crate.
fn fmt_crates(log: &str) -> Option<Vec<String>> {
    let re = Regex::new(r"Diff in \S*?/(crates/[A-Za-z0-9_.-]+)/").expect("static regex");
    let mut found: Vec<String> = Vec::new();
    for caps in re.captures_iter(log) {
        let dir = caps[1].to_string();
        if !found.contains(&dir) {
            found.push(dir);
        }
    }
    if found.is_empty() {
        None
    } else {
        Some(found)
    }
}

/// Both crates, for a fmt check whose log named no path (an expired or
/// truncated log). Formatting a clean crate is a no-op, so the wider sweep is
/// safe; guessing ONE of the two would silently leave the other drifted.
fn default_fmt_crates() -> Vec<String> {
    vec!["crates/fno-agents".to_string(), "crates/fno".to_string()]
}

/// The node ids `check-pr-node-closure` said the branch names. Its refusal
/// reads `... names x-f8e3, and the exact trailer claims none of them.`
fn closure_nodes(log: &str) -> Vec<String> {
    let re = Regex::new(r"names ([^,]+), and the exact trailer claims none").expect("static regex");
    match re.captures(log) {
        Some(caps) => caps[1]
            .split_whitespace()
            .map(|s| s.trim_end_matches(',').to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        None => Vec::new(),
    }
}

/// Ruff's diagnostic header. Modern ruff prints the CODE on its own line and
/// the location beneath it (`F821 Undefined name` / `   --> src/x.py:886:21`),
/// NOT the one-line `path:line:col: CODE` form -- a pattern written for the
/// old shape matches nothing and reads as "no ruff failures ever".
fn ruff_re() -> Regex {
    Regex::new(r"(?m)^[EFNW]\d{3} \S|^\s*--> \S+\.py:\d+:\d+$").expect("static regex")
}

/// Mypy's one-line diagnostic. Disjoint from [`ruff_re`] on purpose: mypy
/// writes `path.py:12: error:` (one colon-number), ruff's location line writes
/// `path.py:886:21` (two) and never the word `error:`.
fn mypy_re() -> Regex {
    Regex::new(r"(?m)^\S+\.py:\d+: error:").expect("static regex")
}

/// Pytest's short-summary node ids. The job runs with `cli/` as its working
/// directory, so the paths are `tests/...`, never `cli/tests/...`; a repro
/// built from the raw path only runs from `cli`.
fn pytest_nodeids(log: &str) -> Vec<String> {
    let re = Regex::new(r"(?m)^FAILED (\S+::\S+)").expect("static regex");
    let mut ids: Vec<String> = Vec::new();
    for caps in re.captures_iter(log) {
        let id = caps[1].to_string();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

/// The cargo tests that failed, from `test <path> ... FAILED`.
fn cargo_test_names(log: &str) -> Vec<String> {
    let re = Regex::new(r"(?m)^test (\S+) \.\.\. FAILED").expect("static regex");
    re.captures_iter(log).map(|c| c[1].to_string()).collect()
}

/// The guard that refused, from its own `check-<name>: ` line prefix. Only the
/// shell guards under `scripts/ci/` print this; the two Python ones do not, so
/// they fall through to `unknown` rather than being handed a wrong repro.
fn guard_script(log: &str) -> Option<String> {
    let re = Regex::new(r"(?m)^(check-[a-z0-9-]+): ").expect("static regex");
    re.captures(log).map(|c| c[1].to_string())
}

// ── check-row reading ───────────────────────────────────────────────────────

/// The failing rows of a `gh pr checks --json name,bucket,link,workflow`
/// payload, already deduped to the latest run per name by the shared
/// [`crate::check_supersession::latest_per_name`]. `cancel` counts as failing
/// for the same reason the stop gate counts it: a cancelled check is not a
/// pass, and heal's in-flight guard is what keeps it from being one it caused.
pub(crate) fn failing_rows(checks: &Value) -> Vec<Value> {
    let deduped = crate::check_supersession::latest_per_name(checks);
    deduped
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter(|row| {
                    matches!(
                        row.get("bucket")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_lowercase()
                            .as_str(),
                        "fail" | "cancel"
                    )
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// True when any check is still running. Read AFTER a fix commit and before
/// the push: pushing over a run in flight cancels it, which is the exact harm
/// one session did seven times in one session.
pub(crate) fn any_pending(checks: &Value) -> bool {
    let deduped = crate::check_supersession::latest_per_name(checks);
    deduped
        .as_array()
        .map(|rows| {
            rows.iter().any(|row| {
                !matches!(
                    row.get("bucket")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_lowercase()
                        .as_str(),
                    "pass" | "fail" | "skipping" | "cancel"
                )
            })
        })
        .unwrap_or(false)
}

/// The job id out of a check's `link`
/// (`.../actions/runs/<run>/job/<job>`). Mirrors `_JOB_URL` in
/// `cli/src/fno/pr/_logs.py`; a check with no job link (a commit
/// StatusContext) has no log to read and answers `None`.
pub(crate) fn job_id(link: &str) -> Option<String> {
    let re = Regex::new(r"^https?://[^/]+/[^/]+/[^/]+/actions/runs/\d+/job/(\d+)")
        .expect("static regex");
    re.captures(link).map(|c| c[1].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A real `cargo fmt --check (pinned)` failure, timestamps and all.
    const FMT_LOG: &str = concat!(
        "2026-09-03T03:06:02.1431957Z ##[group]Run cargo \"+$RUSTFMT_TOOLCHAIN\" fmt --all --check\n",
        "2026-09-03T03:06:03.2159034Z Diff in /home/runner/work/footnote/footnote/crates/fno/src/server.rs:17553:\n",
        "2026-09-03T03:06:03.2234373Z -            core.pane_send(\n",
        "2026-09-03T03:06:04.8929731Z ##[error]Process completed with exit code 1.\n",
    );

    fn ctx<'a>(check: &'a str, workflow: &'a str, log: &'a str) -> Ctx<'a> {
        Ctx {
            check,
            workflow,
            log,
        }
    }

    #[test]
    fn timestamps_are_stripped_so_anchored_patterns_can_match() {
        let stripped = strip_timestamps(FMT_LOG);
        assert!(
            stripped.contains("Diff in /home/runner/work/footnote/footnote/crates/fno/src/"),
            "got: {stripped}"
        );
        assert!(!stripped.contains("2026-09-03T03:06"), "got: {stripped}");
    }

    #[test]
    fn a_fmt_red_row_classifies_as_rustfmt_drift_in_the_crate_the_log_named() {
        let log = strip_timestamps(FMT_LOG);
        let f = classify(&ctx("cargo fmt --check (pinned)", "rust-ci", &log), false);
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
    }

    #[test]
    fn two_drifted_crates_yield_one_finding_carrying_both() {
        let log = strip_timestamps(&format!(
            "{FMT_LOG}2026-09-03T03:06:03Z Diff in /home/runner/work/footnote/footnote/crates/fno-agents/src/heal.rs:1:\n"
        ));
        let f = classify(&ctx("cargo fmt --check (pinned)", "rust-ci", &log), false);
        match f.remedy {
            Remedy::Auto { run, .. } => {
                let dirs: Vec<&str> = run.iter().map(|c| c.cwd.as_str()).collect();
                assert_eq!(dirs, vec!["crates/fno", "crates/fno-agents"]);
            }
            other => panic!("expected Auto, got {other:?}"),
        }
    }

    #[test]
    fn a_fmt_check_whose_log_named_no_path_sweeps_both_crates() {
        let f = classify(
            &ctx("cargo fmt --check (pinned)", "rust-ci", "log unavailable"),
            false,
        );
        assert_eq!(f.signature, "rustfmt-drift");
        match f.remedy {
            Remedy::Auto { run, .. } => assert_eq!(run.len(), 2, "{run:?}"),
            other => panic!("expected Auto, got {other:?}"),
        }
    }

    #[test]
    fn modern_ruff_output_classifies_as_ruff_lint() {
        // The shape ruff actually prints: code first, location beneath.
        let log = strip_timestamps(concat!(
            "2026-09-03T07:15:29.7310174Z F821 Undefined name `Callable`\n",
            "2026-09-03T07:15:29.7310931Z    --> src/fno/king/board.py:886:21\n",
            "2026-09-03T07:15:29.7322711Z Found 1 error.\n",
        ));
        let f = classify(
            &ctx("Python static correctness (495 sources)", "cli-ci", &log),
            false,
        );
        assert_eq!(f.signature, "ruff-lint");
        match f.remedy {
            Remedy::Auto { run, .. } => {
                assert_eq!(run[0].cwd, "cli");
                assert!(run[0].argv.contains(&"--fix".to_string()), "{run:?}");
            }
            other => panic!("expected Auto, got {other:?}"),
        }
    }

    #[test]
    fn a_mypy_error_is_mypy_not_ruff_lint() {
        let log = "src/fno/x.py:12: error: Incompatible return value type";
        let f = classify(&ctx("Python static correctness", "cli-ci", log), false);
        assert_eq!(f.signature, "mypy");
        assert!(
            matches!(&f.remedy, Remedy::Escalate { repro } if repro.contains("mypy")),
            "{:?}",
            f.remedy
        );
    }

    #[test]
    fn a_missing_closure_trailer_edits_the_body_with_the_named_node() {
        let log = concat!(
            "check-pr-node-closure: HEAD ref 'feature/x-f8e3' names x-f8e3, ",
            "and the exact trailer claims none of them.\n",
            "  Add a line reading:\n",
        );
        let f = classify(&ctx("check-pr-node-closure", "pr-node-closure", log), false);
        assert_eq!(f.signature, "closure-trailer");
        assert_eq!(
            f.remedy,
            Remedy::EditBody {
                nodes: vec!["x-f8e3".to_string()]
            }
        );
    }

    #[test]
    fn a_pytest_failure_escalates_with_a_repro_that_runs_from_cli() {
        let log = concat!(
            "FAILED tests/unit/test_mail_force.py::test_force_writes_a_row - AssertionError\n",
            "FAILED tests/unit/test_agents_cli_fold.py::test_paths - assert\n",
            "3 failed, 19455 passed, 301 skipped\n",
        );
        let f = classify(&ctx("smoke-pytest", "guards", log), false);
        assert_eq!(f.signature, "pytest");
        match f.remedy {
            Remedy::Escalate { repro } => {
                assert!(repro.starts_with("cd cli && uv run pytest "), "{repro}");
                assert!(repro.contains("tests/unit/test_mail_force.py::test_force_writes_a_row"));
                assert!(repro.contains("tests/unit/test_agents_cli_fold.py::test_paths"));
            }
            other => panic!("expected Escalate, got {other:?}"),
        }
    }

    #[test]
    fn a_cargo_test_failure_escalates_naming_the_failing_test() {
        let log = "test stream_worker::tests::mid_turn_silence ... FAILED\n\
                   test result: FAILED. 1948 passed; 1 failed;\n";
        let f = classify(&ctx("cargo test + schema parity", "rust-ci", log), false);
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
    }

    #[test]
    fn a_guard_refusal_escalates_with_the_guards_own_script() {
        let log = "check-file-budget: cli/src/fno/mail/cli.py is 6105 lines (budget 5000)";
        let f = classify(&ctx("guards", "guards", log), false);
        assert_eq!(f.signature, "guard-script");
        assert_eq!(
            f.remedy,
            Remedy::Escalate {
                repro: "bash scripts/ci/check-file-budget.sh".to_string()
            }
        );
    }

    #[test]
    fn a_check_red_on_main_reads_inherited_even_when_its_log_matches() {
        let log = strip_timestamps(FMT_LOG);
        let f = classify(&ctx("cargo fmt --check (pinned)", "rust-ci", &log), true);
        assert_eq!(f.signature, "inherited");
        assert_eq!(f.remedy, Remedy::Inherited);
        assert!(!f.counts_against_pr());
    }

    #[test]
    fn an_unmatched_log_is_unknown_and_carries_its_tail() {
        let log: String = (1..=60)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let f = classify(&ctx("mystery", "somewhere", &log), false);
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
    }

    #[test]
    fn an_unavailable_log_is_unknown_never_dropped() {
        let f = classify(&ctx("some-check", "guards", ""), false);
        assert_eq!(f.signature, "unknown");
        assert!(f.counts_against_pr());
    }

    #[test]
    fn the_playbook_names_every_signature_in_the_table() {
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
    }

    #[test]
    fn failing_rows_keeps_fail_and_cancel_and_drops_the_rest() {
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
    }

    #[test]
    fn any_pending_sees_a_run_still_in_flight() {
        let settled = json!([{"name": "a", "bucket": "fail"}, {"name": "b", "bucket": "pass"}]);
        assert!(!any_pending(&settled));
        let running = json!([{"name": "a", "bucket": "pass"}, {"name": "b", "bucket": "pending"}]);
        assert!(any_pending(&running));
    }

    #[test]
    fn job_id_reads_a_check_link_and_declines_a_status_context() {
        assert_eq!(
            job_id("https://github.com/o/r/actions/runs/123/job/456"),
            Some("456".to_string())
        );
        assert_eq!(job_id("https://example.test/build/7"), None);
    }
}
