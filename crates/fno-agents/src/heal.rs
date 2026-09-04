//! `fno-agents pr-heal` -- classify a red check by signature, apply the
//! canonical fix, push once. `--all --apply` is the drive loop: one heal per
//! red open PR, each from that PR's own worktree, behind four refusals, with
//! one `pr_heal_tick` journal row per invocation.
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
            // Matched by CHECK NAME, which is all the main-HEAD read gives.
            // Measured: the same check was red on both, and the failing TEST
            // differed (two flakes in one suite). So this says the check is
            // not this PR's to fix, never that the two failures are the same
            // one.
            Remedy::Inherited => {
                "the same check is red on main HEAD, so it is not this PR's to fix                  (matched by check name, not by the failing test)"
                    .to_string()
            }
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
    pub log: &'a str,
    /// The `gh pr checks` bucket. Some classes are decided by the check's
    /// STATE, never by its log: a cancelled run leaves a log with nothing in
    /// it, and reading that absence as an unrecognized failure is the
    /// absence-has-three-explanations trap.
    pub bucket: &'a str,
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
        name: "cancelled",
        plan: "escalate: not a verdict; the run was superseded or killed",
        matches: |c| c.bucket == "cancel",
        // Measured on three open PRs: every `unknown` heal reported was a
        // CANCELLED check whose log carried one line. A cancelled run
        // concluded nothing, so there is no defect to name and no signature
        // to add. The action is a rerun, which is a person's call.
        resolve: |_| Remedy::Escalate {
            repro: "the run was cancelled, so it reached no verdict; push again or rerun it"
                .to_string(),
        },
    },
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
        plan: "auto: ruff check --fix over cli/src, exactly the gate's scope",
        matches: |c| ruff_re().is_match(c.log),
        // The remedy mirrors the GATE, not ruff's fuller surface. The gate is
        // `uv run ruff check --no-respect-gitignore src/`; it runs no
        // `ruff format` and never looks at `tests/`. A remedy that reached
        // wider would rewrite files the gate does not read -- `ruff format`
        // over this tree touches more than a thousand of them -- which is a
        // heal nobody asked for wearing a red check as its excuse.
        resolve: |_| Remedy::Auto {
            run: vec![Cmd::new(
                "cli",
                &[
                    "uv",
                    "run",
                    "ruff",
                    "check",
                    "--fix",
                    "--no-respect-gitignore",
                    "src/",
                ],
            )],
            verify: vec![Cmd::new(
                "cli",
                &[
                    "uv",
                    "run",
                    "ruff",
                    "check",
                    "--no-respect-gitignore",
                    "src/",
                ],
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
        resolve: |c| {
            let ids = pytest_nodeids(c.log);
            let shown: Vec<String> = ids.iter().take(PYTEST_REPRO_CAP).cloned().collect();
            let mut repro = format!("cd cli && uv run pytest {}", shown.join(" "));
            if ids.len() > shown.len() {
                repro.push_str(&format!(
                    "  # and {} more; the log lists them all",
                    ids.len() - shown.len()
                ));
            }
            Remedy::Escalate { repro }
        },
    },
    Signature {
        name: "shard-rollup",
        plan: "escalate: a fan-in gate; the real failures are its named shards",
        matches: |c| shard_rollup_shards(c.log).is_some(),
        // Not unknown, and not a defect of its own. This job runs one echo and
        // exits on its shards' results, so classifying it `unknown` printed 38
        // lines of runner boilerplate on every red PR and pointed at nothing.
        resolve: |c| Remedy::Escalate {
            repro: match shard_rollup_shards(c.log) {
                Some(shards) => format!("a fan-in gate; heal the failing shard: {shards}"),
                None => "a fan-in gate; heal its failing shards".to_string(),
            },
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
        name: "review-gate",
        plan: "escalate: not a CI failure; it clears when the review attests",
        matches: |c| c.check.starts_with("fno/review-coverage"),
        // A red review-coverage status is the gate saying the review has not
        // landed yet. Nothing in the diff fixes it, and reporting it as an
        // unrecognized CI failure would send someone hunting a defect that
        // does not exist.
        resolve: |_| Remedy::Escalate {
            repro: "not a CI failure; run the review, then `fno do pr status <n>`".to_string(),
        },
    },
    Signature {
        name: "smoke-step",
        plan: "escalate: the shard runner names its own failing step",
        matches: |c| smoke_failed_step(c.log).is_some(),
        // Ordered ABOVE guard-script because a smoke shard runs dozens of
        // guards and EVERY one announces itself on success. Prefix-matching
        // the log named a guard that had passed, and the repro exited 0 --
        // a report that reads like a diagnosis and points at nothing. The
        // runner's own fail-fast line names the step outright, so it wins
        // wherever it exists.
        resolve: |c| Remedy::Escalate {
            repro: match smoke_failed_step(c.log) {
                Some(step) => format!("failing step: {step}; the log's group carries its output"),
                None => "the shard runner named no step".to_string(),
            },
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
///
/// Several candidates are joined with commas and NO space (`IFS=,` in the
/// guard), so a `[^,]+` capture cannot cross them: on a two-node branch it
/// matched nothing at all, the finding became an empty `EditBody`, and the
/// remedy was a silent no-op. The capture is lazy across commas instead, and
/// the split takes both separators.
fn closure_nodes(log: &str) -> Vec<String> {
    let re = Regex::new(r"names (.+?), and the exact trailer claims none").expect("static regex");
    match re.captures(log) {
        Some(caps) => caps[1]
            .split([',', ' '])
            .map(|s| s.trim().to_string())
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
        // Shards disagree about the working directory: one prints
        // `tests/unit/x.py::t`, another `cli/tests/unit/x.py::t`. Dropping a
        // leading `cli/` makes both spellings one repro that runs from `cli`;
        // without it half the repros named a path that does not exist there.
        let id = caps[1].trim_start_matches("cli/").to_string();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

/// How many failing node ids a repro names before it stops being a command
/// and starts being a paste of the log.
const PYTEST_REPRO_CAP: usize = 5;

/// The cargo tests that failed, from `test <path> ... FAILED`.
fn cargo_test_names(log: &str) -> Vec<String> {
    let re = Regex::new(r"(?m)^test (\S+) \.\.\. FAILED").expect("static regex");
    re.captures_iter(log).map(|c| c[1].to_string()).collect()
}

/// The step a smoke shard stopped on, from the runner's own fail-fast line.
/// Load-bearing because the shard's steps are not all `check-*` scripts: the
/// verb-surface ratchet prints `verb-ratchet:`, and nothing in a prefix scan
/// can see it.
fn smoke_failed_step(log: &str) -> Option<String> {
    let re = Regex::new(r"(?m)^smoke: step failed, stopping \(fail-fast\): (.+)$")
        .expect("static regex");
    re.captures(log).map(|c| c[1].trim().to_string())
}

/// The shards a fan-in gate folded, from its own `<name>=<result>` echo.
/// `Some` only when at least one of them did not pass.
///
/// `cancelled` and `skipped` belong in the vocabulary beside `failure`. A
/// regex that accepted only `success|failure` missed every rollup carrying a
/// cancelled shard, and those are common: a push over a run in flight
/// cancels one, which is the exact harm this verb exists to stop. The gate
/// then read `unknown` and printed 38 lines of runner boilerplate.
fn shard_rollup_shards(log: &str) -> Option<String> {
    let word = "(?:success|failure|cancelled|skipped|timed_out)";
    let re = Regex::new(&format!(
        r"(?m)^([a-z0-9-]+={word}(?: [a-z0-9-]+={word})+)$"
    ))
    .expect("static regex");
    let line = re.captures(log)?.get(1)?.as_str();
    let bad: Vec<String> = line
        .split_whitespace()
        .filter_map(|pair| pair.split_once('='))
        .filter(|(_, result)| *result != "success" && *result != "skipped")
        .map(|(name, result)| format!("{name} ({result})"))
        .collect();
    if bad.is_empty() {
        None
    } else {
        Some(bad.join(", "))
    }
}

/// The guard that REFUSED, from its own `check-<name>: ` line prefix. Only the
/// shell guards under `scripts/ci/` print this; the two Python ones do not, so
/// they fall through to `unknown` rather than being handed a wrong repro.
///
/// The one that matters is the LAST such line before the runner's error
/// marker, not the first in the log. A `guards` job runs dozens of these and
/// every one announces itself on SUCCESS too, so taking the first match named
/// a guard that had passed and handed over a repro that exits 0 -- a report
/// that reads like a diagnosis and points at nothing.
fn guard_script(log: &str) -> Option<String> {
    let re = Regex::new(r"(?m)^(check-[a-z0-9-]+): ").expect("static regex");
    let head = match log.find("##[error]") {
        Some(at) => &log[..at],
        None => log,
    };
    re.captures_iter(head).last().map(|c| c[1].to_string())
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

// ── the verb ────────────────────────────────────────────────────────────────

/// Exit codes. Zero means the PR has nothing red of its own; an `inherited`
/// row is red but is main's, so it never decides this.
pub(crate) const EXIT_CLEAN: i32 = 0;
pub(crate) const EXIT_ESCALATIONS: i32 = 1;
pub(crate) const EXIT_IN_FLIGHT: i32 = 2;
pub(crate) const EXIT_CWD_REFUSAL: i32 = 3;
pub(crate) const EXIT_READ_ERROR: i32 = 4;
pub(crate) const EXIT_NO_GH: i32 = 127;

/// A `gh` read. Generous next to the stop gate's 30s because a paginated
/// check-runs read on a busy PR is slower than a single rollup read.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// A remedy. `cargo fmt` over a large crate is the long pole.
const REMEDY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Parsed `pr-heal` arguments. `gh_bin` / `git_bin` / `cwd` are the same test
/// seams `loop-check` carries, so push discipline is provable against stub
/// executables instead of a real remote. `claims_root` and `events_file`
/// exist for the same reason the drive loop needs them: its two side effects
/// (the claim read, the tick row) must be provable without touching the real
/// `~/.fno`.
#[derive(Clone)]
struct Args {
    pr: Option<String>,
    apply: bool,
    all: bool,
    playbook: bool,
    /// Rehearse the drive loop: every refusal is walked and printed, no
    /// remedy runs, nothing pushes, no question is filed.
    dry_run: bool,
    gh_bin: String,
    git_bin: String,
    cwd: std::path::PathBuf,
    /// Prepended when resolving a remedy's binary (`cargo`, `uv`). Empty
    /// means resolve off PATH. It exists so a test can inject a stub WITHOUT
    /// mutating the process PATH: PATH is global, and a stub `git` placed
    /// there leaked into three unrelated tests running in parallel.
    bin_dir: String,
    /// Explicit claims ROOT (the dir containing `.fno/claims`). Empty
    /// resolves by key prefix: `node:` keys route to `$FNO_CLAIMS_ROOT`,
    /// else `$HOME`.
    claims_root: String,
    /// Explicit `events.jsonl` for the `pr_heal_tick` row. Empty writes the
    /// global `~/.fno/events.jsonl`, the same journal the pr-watch tick's own
    /// `_emit_event` defaults to.
    events_file: String,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut a = Args {
        pr: None,
        apply: false,
        all: false,
        playbook: false,
        dry_run: false,
        gh_bin: "gh".to_string(),
        git_bin: "git".to_string(),
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        bin_dir: String::new(),
        claims_root: String::new(),
        events_file: String::new(),
    };
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let take = |name: &str| -> Result<String, String> {
            argv.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg {
            "--apply" => a.apply = true,
            "--all" => a.all = true,
            "--playbook" => a.playbook = true,
            "--dry-run" => a.dry_run = true,
            "--gh-bin" => {
                a.gh_bin = take("--gh-bin")?;
                i += 1;
            }
            "--git-bin" => {
                a.git_bin = take("--git-bin")?;
                i += 1;
            }
            "--cwd" => {
                a.cwd = std::path::PathBuf::from(take("--cwd")?);
                i += 1;
            }
            "--bin-dir" => {
                a.bin_dir = take("--bin-dir")?;
                i += 1;
            }
            "--claims-root" => {
                a.claims_root = take("--claims-root")?;
                i += 1;
            }
            "--events-file" => {
                a.events_file = take("--events-file")?;
                i += 1;
            }
            other if other.starts_with('-') => return Err(format!("unknown flag: {other}")),
            other => a.pr = Some(other.to_string()),
        }
        i += 1;
    }
    Ok(a)
}

/// Run a command, returning (exit ok, stdout). A spawn failure and a non-zero
/// exit are both "did not succeed" here; the caller only ever branches on
/// success, and the distinction that matters (gh absent) is probed once.
fn run(
    bin: &str,
    args: &[&str],
    cwd: &std::path::Path,
    timeout: std::time::Duration,
) -> Result<(bool, String, String), String> {
    match crate::loopcheck::bounded_read(bin.as_ref(), args, cwd, "heal", timeout) {
        Ok(out) => Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr_tail).into_owned(),
        )),
        // heal's own wording. `bounded_read_diagnostic` hardcodes a
        // `loop-check:` prefix, and naming a subsystem that did not run sends
        // a reader to the wrong place.
        Err(err) => Err(
            crate::loopcheck::bounded_read_diagnostic("pr-heal", &err).replacen(
                "loop-check: ",
                "",
                1,
            ),
        ),
    }
}

/// `gh api` against the current repo. `{owner}`/`{repo}` are gh's own
/// placeholders, resolved from the checkout, so heal never reads the remote
/// just to learn its own name.
///
/// `--allow-escape-sequences` is not optional: gh refuses a colorized job log
/// without it, through a pipe and a redirect alike. The retry without the
/// flag covers a gh too old to know it, which is the same fallback the Python
/// twin in `fno.pr._logs` carries. Without it, an older gh kills every read
/// here, not just the log fetch.
fn gh_api(a: &Args, path: &str, extra: &[&str]) -> Result<String, String> {
    let mut args: Vec<&str> = vec!["api", "--allow-escape-sequences", path];
    args.extend_from_slice(extra);
    let (ok, out, err) = run(&a.gh_bin, &args, &a.cwd, READ_TIMEOUT)?;
    if ok {
        return Ok(out);
    }
    if err.to_lowercase().contains("unknown flag") {
        let mut plain: Vec<&str> = vec!["api", path];
        plain.extend_from_slice(extra);
        let (ok, out, err) = run(&a.gh_bin, &plain, &a.cwd, READ_TIMEOUT)?;
        if ok {
            return Ok(out);
        }
        return Err(format!("gh api {path} failed: {}", err.trim()));
    }
    // The stderr tail is the whole difference between "404" and "rate limit"
    // and "log expired"; a bare "failed" sends a reader back to gh by hand.
    Err(format!("gh api {path} failed: {}", err.trim()))
}

/// Read a paginated `gh api` endpoint as one JSON array of PAGES.
///
/// `--paginate` alone concatenates page bodies with NO separator (`}{` for
/// objects, `][` for arrays), which is not parseable JSON and has no reliable
/// split point: a `][` appears inside any PR body carrying a markdown
/// reference link. `--slurp` is gh's own answer and wraps the pages in a real
/// array, so nothing here has to guess at a boundary.
fn gh_api_pages(a: &Args, path: &str) -> Result<Vec<Value>, String> {
    let raw = gh_api(a, path, &["--paginate", "--slurp"])?;
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Array(pages)) => Ok(pages),
        Ok(other) => Ok(vec![other]),
        Err(e) => Err(format!("gh api {path} returned unparseable pages: {e}")),
    }
}

/// The PR's head sha, head ref, and body.
fn read_pr(a: &Args, pr: &str) -> Result<(String, String, String), String> {
    let raw = gh_api(a, &format!("repos/{{owner}}/{{repo}}/pulls/{pr}"), &[])?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| format!("pr json: {e}"))?;
    let head = v
        .pointer("/head/sha")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    let head_ref = v
        .pointer("/head/ref")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    let body = v
        .get("body")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    if head.is_empty() {
        return Err("pr json carried no head sha".to_string());
    }
    Ok((head, head_ref, body))
}

/// The PR head's check runs, in the `bucket`/`link` shape the classifier and
/// [`crate::check_supersession::latest_per_name`] already speak.
///
/// The read is REST. `gh pr checks` is GraphQL, and this repo's quota broker
/// routes it away unconditionally, so a heal built on it could never run. The
/// translation below is the whole cost of reading the cheap endpoint.
fn read_checks(a: &Args, head: &str) -> Result<Value, String> {
    let pages = gh_api_pages(
        a,
        &format!("repos/{{owner}}/{{repo}}/commits/{head}/check-runs"),
    )?;
    let mut rows: Vec<Value> = Vec::new();
    for page in pages {
        let Some(runs) = page.get("check_runs").and_then(|r| r.as_array()) else {
            continue;
        };
        for run in runs {
            rows.push(serde_json::json!({
                "name": run.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "bucket": rest_bucket(run),
                "link": run.get("html_url").and_then(|v| v.as_str()).unwrap_or(""),
                "workflow": run
                    .pointer("/check_suite/id")
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                "startedAt": run.get("started_at").and_then(|v| v.as_str()).unwrap_or(""),
                "completedAt": run.get("completed_at").and_then(|v| v.as_str()).unwrap_or(""),
            }));
        }
    }
    // The check-runs endpoint returns ONLY check-runs. A commit StatusContext
    // (`fno/review-coverage`, `stacked-base-guard`) lives on a different
    // endpoint, and reading one without the other is a false green: a PR whose
    // every job passed while a status failed reported "nothing red".
    rows.extend(read_statuses(a, head));
    if rows.is_empty() {
        return Err("check-runs read named no checks".to_string());
    }
    Ok(Value::Array(rows))
}

/// The commit's StatusContexts, in the same row shape as the check-runs.
/// A read failure yields none rather than aborting the run: the check-runs
/// half is still worth reporting, and the caller's exit code already treats
/// an unreadable world as unsettled.
fn read_statuses(a: &Args, head: &str) -> Vec<Value> {
    let Ok(raw) = gh_api(
        a,
        &format!("repos/{{owner}}/{{repo}}/commits/{head}/status"),
        &[],
    ) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let Some(rows) = v.get("statuses").and_then(|s| s.as_array()) else {
        return Vec::new();
    };
    rows.iter()
        .map(|st| {
            let state = st
                .get("state")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            serde_json::json!({
                "name": st.get("context").and_then(|v| v.as_str()).unwrap_or(""),
                "bucket": match state.as_str() {
                    "success" => "pass",
                    "pending" => "pending",
                    _ => "fail",
                },
                "link": st.get("target_url").and_then(|v| v.as_str()).unwrap_or(""),
                "workflow": "",
                "startedAt": st.get("created_at").and_then(|v| v.as_str()).unwrap_or(""),
                "completedAt": st.get("updated_at").and_then(|v| v.as_str()).unwrap_or(""),
            })
        })
        .collect()
}

/// A REST check-run's `status`/`conclusion` folded to the `gh pr checks`
/// bucket vocabulary. An unrecognized conclusion buckets `fail` rather than
/// `pass`: a bucket heal does not understand must never read green.
fn rest_bucket(run: &Value) -> &'static str {
    let status = run
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    if status != "completed" {
        return "pending";
    }
    match run
        .get("conclusion")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "success" => "pass",
        "skipped" | "neutral" => "skipping",
        "cancelled" => "cancel",
        _ => "fail",
    }
}

/// Classify every failing row of one PR.
fn findings_for(a: &Args, pr: &str, head: &str) -> Result<Vec<Finding>, String> {
    let checks = read_checks(a, head)?;
    // `None` means the main-head read did not answer, which is NOT the same
    // as "main is green". Defaulting it to an empty set silently reclassified
    // every inherited failure as this PR's own, so the caller is told instead
    // and the report says the classification was unavailable.
    let inherited = crate::loopcheck::main_head_failing_checks(&a.gh_bin, &a.cwd, 20);
    if inherited.is_none() {
        println!(
            "note: could not read main's HEAD, so no row can be shown as inherited; \
             a failure below may be main's rather than this PR's"
        );
    }
    let inherited = inherited.unwrap_or_default();
    let mut out = Vec::new();
    for row in failing_rows(&checks) {
        let check = row["name"].as_str().unwrap_or("").to_string();
        let log = match job_id(row["link"].as_str().unwrap_or("")) {
            Some(id) => gh_api(
                a,
                &format!("repos/{{owner}}/{{repo}}/actions/jobs/{id}/logs"),
                &[],
            )
            // A log the API cannot serve (expired retention, a commit status
            // with no job) is REPORTED as unreadable, never dropped: a check
            // heal cannot read is still red.
            .unwrap_or_else(|e| format!("log unavailable: {e}")),
            None => "log unavailable: not an Actions job".to_string(),
        };
        let stripped = strip_timestamps(&log);
        let bucket = row["bucket"].as_str().unwrap_or("").to_string();
        out.push(classify(
            &Ctx {
                check: &check,
                log: &stripped,
                bucket: &bucket,
            },
            inherited.iter().any(|n| n == &check),
        ));
    }
    let _ = pr;
    Ok(out)
}

/// Refuse unless this checkout is the PR's branch and its tree is clean.
/// Both are named in one message: a caller who is on the wrong branch AND
/// dirty should learn that in one run, not two.
fn refuse_wrong_worktree(a: &Args, head_ref: &str) -> Option<String> {
    let branch = run(
        &a.git_bin,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        &a.cwd,
        READ_TIMEOUT,
    )
    .map(|(_, out, _)| out.trim().to_string())
    .unwrap_or_default();
    let dirty = run(&a.git_bin, &["status", "--porcelain"], &a.cwd, READ_TIMEOUT)
        .map(|(_, out, _)| !out.trim().is_empty())
        .unwrap_or(true);
    let mut reasons = Vec::new();
    if branch != head_ref {
        reasons.push(format!("on branch {branch}, the PR's head is {head_ref}"));
    }
    if dirty {
        reasons.push("the worktree has uncommitted changes".to_string());
    }
    if reasons.is_empty() {
        None
    } else {
        Some(reasons.join("; "))
    }
}

/// The worktree's uncommitted state, verbatim. Compared BEFORE and AFTER each
/// remedy, because "is the tree dirty" is a whole-worktree question and the
/// remedies share one worktree: the first remedy's edits are still
/// uncommitted when the second runs, so a second remedy that changed nothing
/// read as dirty and took credit for the first one's work.
fn porcelain(a: &Args) -> String {
    run(&a.git_bin, &["status", "--porcelain"], &a.cwd, READ_TIMEOUT)
        .map(|(_, out, _)| out.trim().to_string())
        .unwrap_or_default()
}

/// Whether the worktree carries uncommitted changes.
fn dirty(a: &Args) -> bool {
    !porcelain(a).is_empty()
}

/// Apply the auto remedies. Returns the signatures that were fixed and
/// verified. A remedy whose verify stays red is demoted in place, so the run
/// never commits a fix that did not work.
fn apply_auto(a: &Args, findings: &mut [Finding]) -> Vec<String> {
    let mut healed = Vec::new();
    for f in findings.iter_mut() {
        let Remedy::Auto { run: cmds, verify } = f.remedy.clone() else {
            continue;
        };
        let before = porcelain(a);
        let mut failure: Option<String> = None;
        for cmd in cmds.iter().chain(verify.iter()) {
            let dir = a.cwd.join(&cmd.cwd);
            let argv: Vec<&str> = cmd.argv.iter().map(|s| s.as_str()).collect();
            let bin = if a.bin_dir.is_empty() {
                argv[0].to_string()
            } else {
                std::path::Path::new(&a.bin_dir)
                    .join(argv[0])
                    .to_string_lossy()
                    .into_owned()
            };
            let ok = run(&bin, &argv[1..], &dir, REMEDY_TIMEOUT)
                .map(|(ok, _, _)| ok)
                .unwrap_or(false);
            if !ok {
                failure = Some(cmd.render());
                break;
            }
        }
        match failure {
            // A remedy whose run AND verify both exit 0 while leaving the
            // worktree BYTE-FOR-BYTE as it found it fixed nothing: CI is red
            // on something this checkout does not reproduce (a different
            // toolchain, or a check that has not re-run). Counting that as
            // healed reported exit 0 with the check still red, which is the
            // false green heal exists to end.
            None if porcelain(a) == before => {
                f.remedy = Remedy::Escalate {
                    repro: format!(
                        "the remedy ran clean and changed nothing, so this red \
                         does not reproduce here; compare toolchains, or re-read \
                         after the next run: {}",
                        cmds.first().map(Cmd::render).unwrap_or_default()
                    ),
                }
            }
            None => healed.push(f.signature.to_string()),
            Some(cmd) => {
                f.remedy = Remedy::Escalate {
                    repro: format!("the automatic fix did not succeed; run it by hand: {cmd}"),
                }
            }
        }
    }
    healed
}

/// Append the missing closure trailers to the PR body. No commit and no push:
/// the closure workflow re-fires on an `edited` event.
fn apply_edit_body(
    a: &Args,
    pr: &str,
    body: &str,
    findings: &mut [Finding],
) -> Result<bool, String> {
    let mut lines: Vec<String> = Vec::new();
    for f in findings.iter() {
        let Remedy::EditBody { nodes } = &f.remedy else {
            continue;
        };
        for node in nodes {
            // The trailer is generated by the verb that checks the id against
            // the graph. Pasting a candidate out of the refusal is the exact
            // move that refusal warns against: a branch segment can match the
            // id grammar without naming a real node.
            let (ok, out, _) = run(
                "fno",
                &["do", "pr", "closure-trailer", node],
                &a.cwd,
                READ_TIMEOUT,
            )?;
            // The empty-stdout half is not defensive padding. Measured
            // against the real verb: an id the graph does not know exits 0
            // and prints NOTHING, with nothing on stderr either. Trusting
            // the exit code alone would append a blank line to the PR body
            // and report the trailer added.
            if !ok || out.trim().is_empty() {
                return Err(format!("could not generate a closure trailer for {node}"));
            }
            lines.push(out.trim().to_string());
        }
    }
    if lines.is_empty() {
        return Ok(false);
    }
    let new_body = format!("{}\n\n{}\n", body.trim_end(), lines.join("\n"));
    let path = a.cwd.join(".fno").join("heal-pr-body.md");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, &new_body).map_err(|e| format!("write body file: {e}"))?;
    let (ok, _, _) = run(
        &a.gh_bin,
        &["pr", "edit", pr, "--body-file", &path.to_string_lossy()],
        &a.cwd,
        READ_TIMEOUT,
    )?;
    let _ = std::fs::remove_file(&path);
    if ok {
        Ok(true)
    } else {
        Err("gh pr edit refused the body".to_string())
    }
}

/// Print the report and return the exit code the findings imply.
fn report(findings: &[Finding], dry_run: bool, terse: bool) -> i32 {
    if dry_run {
        println!("dry run: nothing was changed, nothing was pushed");
    }
    for f in findings {
        // A 40-line log tail per unrecognized check is the right answer for
        // ONE PR and unreadable across every open one: 13 unknowns buried the
        // whole `--all` report in runner boilerplate. Terse mode names the
        // check and points at the single-PR run.
        let detail = if terse {
            f.detail().lines().next().unwrap_or_default().to_string()
        } else {
            f.detail()
        };
        println!("{}  {}  {}  {}", f.check, f.signature, f.action(), detail);
    }
    let own: Vec<&Finding> = findings.iter().filter(|f| f.counts_against_pr()).collect();
    if own.is_empty() {
        return EXIT_CLEAN;
    }
    // An escalation is the only thing left for a person to do. A dry run
    // reports what it WOULD do, so its remaining Auto and EditBody rows are
    // still work; after --apply they have been done. An applied EditBody used
    // to fall through to "escalations remain" with no repro to show for it.
    let escalations = own
        .iter()
        .filter(|f| matches!(f.remedy, Remedy::Escalate { .. }))
        .count();
    if escalations > 0 {
        return EXIT_ESCALATIONS;
    }
    if dry_run {
        EXIT_ESCALATIONS
    } else {
        EXIT_CLEAN
    }
}

/// `fno-agents pr-heal <n> [--apply] [--all] [--playbook]`.
pub fn run_heal(argv: &[String]) -> i32 {
    let a = match parse_args(argv) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("pr-heal: {msg}");
            eprintln!("usage: pr-heal <pr> [--apply] | --all [--apply] [--dry-run] | --playbook");
            return EXIT_READ_ERROR;
        }
    };
    if a.playbook {
        print!("{}", playbook());
        return EXIT_CLEAN;
    }
    if a.all && a.apply {
        return run_all_apply(&a, a.dry_run);
    }
    if a.dry_run {
        eprintln!(
            "pr-heal: --dry-run rehearses the --all --apply drive loop; plain --all \
             and the single-PR report are already dry"
        );
        return EXIT_READ_ERROR;
    }
    if a.all {
        return run_all(&a);
    }
    let Some(pr) = a.pr.clone() else {
        eprintln!("pr-heal: needs a PR number (or --all, or --playbook)");
        return EXIT_READ_ERROR;
    };
    run_one(&a, &pr)
}

/// One heal per red open PR, report-only. Uses the REST listing for the same
/// reason [`read_checks`] does: `gh pr list` is GraphQL and gets routed away.
fn run_all(a: &Args) -> i32 {
    let pages = match gh_api_pages(a, "repos/{owner}/{repo}/pulls?state=open&per_page=100") {
        Ok(pages) => pages,
        Err(msg) => {
            eprintln!("pr-heal: {msg}");
            return EXIT_READ_ERROR;
        }
    };
    let mut worst = EXIT_CLEAN;
    for num in open_pr_numbers(&pages) {
        println!("── PR {num}");
        worst = worse_of(worst, run_one(a, &num));
    }
    worst
}

/// The exit code a caller should act on first when several PRs answered.
/// A read error outranks an escalation, which outranks in-flight: keeping
/// whichever code came LAST let a later escalation mask an earlier failure to
/// read the world at all.
pub(crate) fn worse_of(a: i32, b: i32) -> i32 {
    let rank = |code: i32| match code {
        EXIT_CLEAN => 0,
        EXIT_IN_FLIGHT => 1,
        EXIT_ESCALATIONS => 2,
        _ => 3,
    };
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

/// The PR numbers in a slurped REST pulls listing (an array of pages, each a
/// JSON array of PRs).
pub(crate) fn open_pr_numbers(pages: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    for page in pages {
        let rows = match page {
            Value::Array(rows) => rows.clone(),
            other => vec![other.clone()],
        };
        for row in rows {
            if let Some(n) = row.get("number").and_then(|v| v.as_u64()) {
                out.push(n.to_string());
            }
        }
    }
    out
}

// ── the drive loop (--all --apply) ───────────────────────────────────────────

/// Node ids a PR head ref names, as delimiter-bounded segments. Parity with
/// `branch_node_ids` in `cli/src/fno/pr/closure.py` (`NODE_ID_BODY`), whose
/// delimiter-bounded rule exists so fixed-width hex can never make `x-5b66`
/// count inside `x-5b667`. The Rust regex crate has no lookahead, so the
/// Python pattern's `(?=$|[/-])` becomes explicit boundary checks on the
/// characters either side of each match.
pub(crate) fn branch_node_ids(head_ref: &str) -> Vec<String> {
    let re = Regex::new(r"[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}").expect("static regex");
    let mut out: Vec<String> = Vec::new();
    for m in re.find_iter(head_ref) {
        let before = &head_ref[..m.start()];
        let before_ok = before.is_empty() || before.ends_with('/') || before.ends_with('-');
        let after = &head_ref[m.end()..];
        let after_ok = after.is_empty() || after.starts_with('/') || after.starts_with('-');
        let id = m.as_str().to_string();
        if before_ok && after_ok && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Every worktree of this checkout's repo, as (branch, path) pairs parsed
/// from `git worktree list --porcelain`. Detached worktrees carry no `branch`
/// line and are skipped: the drive loop keys strictly on the PR's head ref.
/// An unreadable answer yields an empty list, which the caller reports as
/// `no_worktree` per PR rather than guessing.
pub(crate) fn worktrees_by_branch(
    git_bin: &str,
    cwd: &std::path::Path,
) -> Vec<(String, std::path::PathBuf)> {
    let Ok((true, stdout, _)) = run(
        git_bin,
        &["worktree", "list", "--porcelain"],
        cwd,
        READ_TIMEOUT,
    ) else {
        return Vec::new();
    };
    let mut list = Vec::new();
    let mut path: Option<std::path::PathBuf> = None;
    for line in stdout.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(std::path::PathBuf::from(p.trim()));
        } else if let Some(b) = line.strip_prefix("branch ") {
            if let (Some(p), Some(name)) = (&path, b.trim().strip_prefix("refs/heads/")) {
                list.push((name.to_string(), p.clone()));
            }
        }
    }
    list
}

/// The live claim holder on any node the head ref names, if one exists.
/// `Suspect` counts as held (its TTL still protects the slot); `Stale` does
/// not (the holder is dead and the slot is recoverable). The claim lockfile
/// is the read, never a stored pid or a manifest snapshot.
fn claim_holder(head_ref: &str, claims_root: Option<&std::path::Path>) -> Option<(String, String)> {
    for id in branch_node_ids(head_ref) {
        let (state, rec) = crate::claims::status(&format!("node:{id}"), claims_root);
        if matches!(
            state,
            crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect
        ) {
            let holder = rec.map(|r| r.holder).unwrap_or_default();
            return Some((id, holder));
        }
    }
    None
}

/// A binary through the bin-dir seam when set (the same resolution the
/// remedies use), so a test can stub `fno` without mutating PATH.
fn seam_bin(a: &Args, name: &str) -> String {
    if a.bin_dir.is_empty() {
        name.to_string()
    } else {
        std::path::Path::new(&a.bin_dir)
            .join(name)
            .to_string_lossy()
            .into_owned()
    }
}

/// Does an already-open operator question carry this marker? Read through
/// the same `fno inbox outstanding --json` an operator would run, so dedup
/// can never drift from what is actually on the board. An unreadable board
/// fails toward asking: silence is the failure a question exists to prevent.
fn open_questions_mention(a: &Args, marker: &str) -> bool {
    let bin = seam_bin(a, "fno");
    match run(
        &bin,
        &["inbox", "outstanding", "--json"],
        &a.cwd,
        READ_TIMEOUT,
    ) {
        Ok((true, out, _)) => serde_json::from_str::<Value>(&out)
            .ok()
            .and_then(|v| v.get("questions").and_then(|q| q.as_array()).cloned())
            .is_some_and(|qs| {
                qs.iter().any(|q| {
                    q.get("question")
                        .and_then(|s| s.as_str())
                        .is_some_and(|s| s.contains(marker))
                })
            }),
        _ => false,
    }
}

/// File one operator question for a failing check no signature recognized,
/// via `fno inbox outstanding ask` -- the inbox, never a file nobody reads.
/// Deduplicated on the marker, so a 600s tick cannot re-ask a question the
/// board already carries. Returns true when a question was filed.
fn escalate_unknown_signature(a: &Args, pr: &str, check: &str, head_ref: &str) -> bool {
    let marker = format!("heal: PR {pr} check {check}");
    if open_questions_mention(a, &marker) {
        return false;
    }
    let question = format!(
        "{marker} failed with no playbook signature. Classify it with \
         `fno do pr heal {pr}` (that report carries the log tail) or add a \
         signature in crates/fno-agents/src/heal.rs."
    );
    let bin = seam_bin(a, "fno");
    let mut argv: Vec<&str> = vec!["inbox", "outstanding", "ask", &question];
    let node = branch_node_ids(head_ref).into_iter().next();
    let node_flag;
    if let Some(n) = &node {
        node_flag = vec!["--node", n.as_str()];
        argv.extend(node_flag.iter().copied());
    }
    match run(&bin, &argv, &a.cwd, READ_TIMEOUT) {
        Ok((true, _, _)) => true,
        Ok((_, _, err)) => {
            eprintln!(
                "pr-heal: escalation for PR {pr} {check} refused: {}",
                err.trim()
            );
            false
        }
        Err(e) => {
            eprintln!("pr-heal: escalation for PR {pr} {check} failed: {e}");
            false
        }
    }
}

/// One `pr_heal_tick` row per drive-loop invocation: the arm's visibility
/// (x-1b88's shared row widens onto this later). Written to the global
/// `~/.fno/events.jsonl`, the same default journal the tick's own
/// `_emit_event` writes, so `fno doctor event audit` reads one place.
fn emit_tick_event(
    a: &Args,
    counts: &std::collections::BTreeMap<&'static str, usize>,
    unknown: usize,
    dry_run: bool,
) {
    let path = if a.events_file.is_empty() {
        crate::paths::AgentsHome::from_env()
            .root()
            .parent()
            .map(|p| p.join("events.jsonl"))
            .unwrap_or_else(|| std::path::PathBuf::from(".fno/events.jsonl"))
    } else {
        std::path::PathBuf::from(&a.events_file)
    };
    let mut fields = serde_json::Map::new();
    for (k, v) in counts {
        fields.insert((*k).to_string(), serde_json::json!(v));
    }
    fields.insert("unknown".to_string(), serde_json::json!(unknown));
    fields.insert("dry_run".to_string(), serde_json::json!(dry_run));
    if let Err(e) =
        crate::events::EventEmitter::new(path, "pr-heal").emit_fields("pr_heal_tick", fields)
    {
        eprintln!("pr-heal: the pr_heal_tick row did not land: {e}");
    }
}

/// The drive loop: one heal per red open PR, from that PR's own worktree,
/// behind four refusals -- claim free, known signature, one push per PR per
/// cycle, inherited failures named and skipped. `--dry-run` walks every
/// refusal and prints the plan without touching a worktree or the inbox.
fn run_all_apply(a: &Args, dry_run: bool) -> i32 {
    let pages = match gh_api_pages(a, "repos/{owner}/{repo}/pulls?state=open&per_page=100") {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("pr-heal: {msg}");
            return EXIT_READ_ERROR;
        }
    };
    let worktrees = worktrees_by_branch(&a.git_bin, &a.cwd);
    let claims_root = if a.claims_root.is_empty() {
        None
    } else {
        Some(std::path::Path::new(&a.claims_root))
    };
    let mut counts: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    let bump = |counts: &mut std::collections::BTreeMap<&'static str, usize>, key: &'static str| {
        *counts.entry(key).or_default() += 1;
    };
    let mut unknown: Vec<(String, String, String)> = Vec::new();
    let mut worst = EXIT_CLEAN;
    for pr in open_pr_numbers(&pages) {
        bump(&mut counts, "seen");
        println!("── PR {pr}");
        let (head, head_ref, _body) = match read_pr(a, &pr) {
            Ok(v) => v,
            Err(msg) => {
                eprintln!("pr-heal: {msg}");
                bump(&mut counts, "skip_read_error");
                worst = worse_of(worst, EXIT_READ_ERROR);
                continue;
            }
        };
        // Refusal 1: a live worker owns this node; a healer pushing under it
        // is the two-writers failure.
        if let Some((node, holder)) = claim_holder(&head_ref, claims_root) {
            println!(
                "skip claim_held: {node} is held by {holder}; \
                 the healer never pushes under a live worker"
            );
            bump(&mut counts, "skip_claim_held");
            continue;
        }
        let findings = match findings_for(a, &pr, &head) {
            Ok(f) => f,
            Err(msg) => {
                eprintln!("pr-heal: {msg}");
                bump(&mut counts, "skip_read_error");
                worst = worse_of(worst, EXIT_READ_ERROR);
                continue;
            }
        };
        // Refusal 4 is classify()'s own: a check red on main HEAD reads
        // Inherited, is named in the report, and never counts against the PR.
        let own: Vec<&Finding> = findings.iter().filter(|f| f.counts_against_pr()).collect();
        if own.is_empty() {
            println!("skip inherited: nothing red here that main is not already red on");
            bump(&mut counts, "skip_inherited");
            continue;
        }
        // Refusal 2: an unknown signature is escalated, never guessed at.
        for f in own.iter().filter(|f| f.signature == "unknown") {
            unknown.push((pr.clone(), f.check.clone(), head_ref.clone()));
        }
        let healable = own
            .iter()
            .any(|f| matches!(f.remedy, Remedy::Auto { .. } | Remedy::EditBody { .. }));
        if !healable {
            // Known-but-escalate rows (pytest, mypy, a guard refusal) have a
            // playbook entry and a repro; the report is their lane.
            report(&findings, true, true);
            bump(&mut counts, "skip_escalate_only");
            worst = worse_of(worst, EXIT_ESCALATIONS);
            continue;
        }
        if dry_run {
            report(&findings, true, true);
            bump(&mut counts, "would_heal");
            continue;
        }
        // Applying needs the PR's own checkout: every remedy runs in it and
        // the push is from it. No worktree means no heal; the loop never
        // clones a repo on its own.
        let Some((_, wt)) = worktrees.iter().find(|(b, _)| b == &head_ref) else {
            println!(
                "skip no_worktree: no checkout on branch {head_ref}; \
                 heal it by hand from that PR's worktree"
            );
            bump(&mut counts, "skip_no_worktree");
            worst = worse_of(worst, EXIT_ESCALATIONS);
            continue;
        };
        let sub = Args {
            cwd: wt.clone(),
            ..a.clone()
        };
        // Refusal 3 rides inside run_one: the pre-push re-read holds the
        // commit local over a run in flight, and it pushes exactly once.
        let code = run_one(&sub, &pr);
        match code {
            EXIT_CLEAN => bump(&mut counts, "healed"),
            EXIT_IN_FLIGHT => bump(&mut counts, "skip_in_flight"),
            EXIT_CWD_REFUSAL => bump(&mut counts, "skip_dirty_tree"),
            EXIT_ESCALATIONS | EXIT_READ_ERROR => bump(&mut counts, "still_red"),
            _ => bump(&mut counts, "still_red"),
        }
        worst = worse_of(worst, code);
    }
    let unknown_n = unknown.len();
    if !dry_run {
        for (pr, check, head_ref) in &unknown {
            if escalate_unknown_signature(a, pr, check, head_ref) {
                println!("escalated: PR {pr} check {check} is now an inbox question");
            }
        }
    }
    emit_tick_event(a, &counts, unknown_n, dry_run);
    let skipped: Vec<String> = counts
        .iter()
        .filter(|(k, _)| k.starts_with("skip_"))
        .map(|(k, v)| format!("{}={v}", k.strip_prefix("skip_").unwrap_or(k)))
        .collect();
    println!(
        "pr heal: seen={} healed={} unknown={} skipped={}",
        counts.get("seen").unwrap_or(&0),
        counts.get("healed").unwrap_or(&0),
        unknown_n,
        skipped.join(",")
    );
    worst
}

fn run_one(a: &Args, pr: &str) -> i32 {
    let (head, head_ref, body) = match read_pr(a, pr) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("pr-heal: {msg}");
            return if msg.contains("No such file") || msg.contains("NotFound") {
                EXIT_NO_GH
            } else {
                EXIT_READ_ERROR
            };
        }
    };
    if a.apply {
        if let Some(why) = refuse_wrong_worktree(a, &head_ref) {
            eprintln!("pr-heal: refusing to apply: {why}");
            return EXIT_CWD_REFUSAL;
        }
    }
    let mut findings = match findings_for(a, pr, &head) {
        Ok(f) => f,
        Err(msg) => {
            eprintln!("pr-heal: {msg}");
            return EXIT_READ_ERROR;
        }
    };
    if !a.apply {
        return report(&findings, true, a.all);
    }

    let healed = apply_auto(a, &mut findings);
    // A failed body edit must DEMOTE its rows. Logging the error and leaving
    // them as `EditBody` let `report` see zero escalations and exit 0 with the
    // trailer never appended and the check still red.
    if let Err(msg) = apply_edit_body(a, pr, &body, &mut findings) {
        eprintln!("pr-heal: body edit failed: {msg}");
        for f in findings.iter_mut() {
            if let Remedy::EditBody { nodes } = f.remedy.clone() {
                f.remedy = Remedy::Escalate {
                    repro: format!(
                        "the body edit failed ({msg}); add it by hand: \
                         fno do pr closure-trailer {}",
                        nodes.join(" ")
                    ),
                };
            }
        }
    }

    let mut committed = false;
    if !healed.is_empty() && dirty(a) {
        let msg = format!("style: heal {}", healed.join(", "));
        let _ = run(&a.git_bin, &["add", "-u"], &a.cwd, READ_TIMEOUT);
        let (ok, _, err) = run(&a.git_bin, &["commit", "-m", &msg], &a.cwd, READ_TIMEOUT)
            .unwrap_or((false, String::new(), String::new()));
        committed = ok;
        // A fix that could not be committed was not applied, whatever the
        // remedy's own exit code said. A pre-commit hook, a signing failure
        // or a full disk all land here, and reporting the row as healed
        // exited 0 with nothing pushed and the check still red.
        if !ok {
            eprintln!(
                "pr-heal: the fix is in the worktree but git commit failed: {}",
                err.trim()
            );
            for f in findings.iter_mut() {
                if matches!(f.remedy, Remedy::Auto { .. }) {
                    f.remedy = Remedy::Escalate {
                        repro: "the remedy ran but git commit failed; commit and push by hand"
                            .to_string(),
                    };
                }
            }
        }
    }

    let code = report(&findings, false, a.all);
    if !committed {
        return code;
    }
    // Re-read BEFORE pushing. A push over a run in flight cancels it, and
    // that is the harm this verb exists to stop repeating.
    match read_checks(a, &head) {
        Ok(checks) if any_pending(&checks) => {
            println!(
                "run in flight; commit kept local, not pushing; \
                 rerun after fno do pr wait {pr}"
            );
            EXIT_IN_FLIGHT
        }
        Ok(_) => {
            let (ok, _, _) = run(&a.git_bin, &["push"], &a.cwd, READ_TIMEOUT).unwrap_or((
                false,
                String::new(),
                String::new(),
            ));
            if ok {
                println!("pushed once");
                code
            } else {
                eprintln!("pr-heal: the fix is committed but the push failed");
                EXIT_READ_ERROR
            }
        }
        Err(msg) => {
            // Unreadable is not "settled". Holding the commit local is the
            // safe half of the fork; pushing on an unanswered read is the one
            // that cancels somebody's run.
            println!("could not re-read checks ({msg}); commit kept local, not pushing");
            EXIT_IN_FLIGHT
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        }
    }

    fn cancelled_ctx<'a>(check: &'a str, log: &'a str) -> Ctx<'a> {
        Ctx {
            check,
            log,
            bucket: "cancel",
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
    }

    #[test]
    fn two_drifted_crates_yield_one_finding_carrying_both() {
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
    }

    #[test]
    fn a_fmt_check_whose_log_named_no_path_sweeps_both_crates() {
        let f = classify(&ctx("cargo fmt --check (pinned)", "log unavailable"), false);
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
    }

    #[test]
    fn a_mypy_error_is_mypy_not_ruff_lint() {
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
    fn a_missing_closure_trailer_edits_the_body_with_the_named_node() {
        let log = concat!(
            "check-pr-node-closure: HEAD ref 'feature/x-f8e3' names x-f8e3, ",
            "and the exact trailer claims none of them.\n",
            "  Add a line reading:\n",
        );
        let f = classify(&ctx("check-pr-node-closure", log), false);
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
    }

    #[test]
    fn a_pytest_repro_runs_from_cli_whichever_cwd_the_shard_used() {
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
    }

    #[test]
    fn a_long_pytest_failure_list_is_capped_and_says_how_many_it_dropped() {
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
    fn a_fan_in_gate_names_its_failing_shards_instead_of_reading_unknown() {
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
    }

    #[test]
    fn an_all_green_rollup_is_not_a_shard_rollup_finding() {
        // The gate only classifies when a shard did not pass; an all-green
        // echo in some other job's log must not capture that job.
        assert_eq!(shard_rollup_shards("a=success b=success"), None);
        assert_eq!(shard_rollup_shards("a=success b=skipped"), None);
    }

    #[test]
    fn a_cancelled_shard_is_named_not_read_as_unknown() {
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
    fn terse_mode_keeps_one_line_per_check_for_the_all_report() {
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
    }

    #[test]
    fn a_cargo_test_failure_escalates_naming_the_failing_test() {
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
    }

    #[test]
    fn a_guard_refusal_escalates_with_the_guards_own_script() {
        let log = "check-file-budget: cli/src/fno/mail/cli.py is 6105 lines (budget 5000)";
        let f = classify(&ctx("guards", log), false);
        assert_eq!(f.signature, "guard-script");
        assert_eq!(
            f.remedy,
            Remedy::Escalate {
                repro: "bash scripts/ci/check-file-budget.sh".to_string()
            }
        );
    }

    #[test]
    fn a_smoke_shard_reads_the_runners_own_failing_step_not_a_guard_prefix() {
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
    }

    #[test]
    fn a_guards_job_names_the_guard_that_refused_not_the_first_that_announced() {
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
    }

    #[test]
    fn a_red_review_coverage_status_is_the_gate_not_a_defect() {
        let f = classify(&ctx("fno/review-coverage", ""), false);
        assert_eq!(f.signature, "review-gate");
        assert!(
            matches!(&f.remedy, Remedy::Escalate { repro } if repro.contains("not a CI failure")),
            "{:?}",
            f.remedy
        );
    }

    #[test]
    fn a_cancelled_check_is_not_an_unrecognized_failure() {
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
    fn a_cancelled_check_red_on_main_still_reads_inherited() {
        // inherited is checked before the table, so main's problem never
        // becomes this PR's whatever the bucket says.
        let f = classify(&cancelled_ctx("guards", ""), true);
        assert_eq!(f.signature, "inherited");
    }

    #[test]
    fn a_check_red_on_main_reads_inherited_even_when_its_log_matches() {
        let log = strip_timestamps(FMT_LOG);
        let f = classify(&ctx("cargo fmt --check (pinned)", &log), true);
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
    }

    #[test]
    fn an_unavailable_log_is_unknown_never_dropped() {
        let f = classify(&ctx("some-check", ""), false);
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

    // ── the verb: push discipline ───────────────────────────────────────────
    //
    // Driven through stub `gh` and `git` executables rather than a real
    // remote, so the two properties that matter (exactly one push, and never
    // a push over a run in flight) are provable rather than argued.

    fn write_exec(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

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
  */pulls/*) echo '{{"head":{{"sha":"deadbeef","ref":"feature/x"}},"body":"b"}}'; exit 0 ;;
  */check-runs) B=completed; {flip}
     if [ "$B" = in_progress ]; then
       echo '{{"check_runs":[{{"name":"cargo fmt --check (pinned)","status":"in_progress","conclusion":null,"html_url":"https://github.com/o/r/actions/runs/1/job/9"}}]}}'
     else
       echo '{{"check_runs":[{{"name":"cargo fmt --check (pinned)","status":"completed","conclusion":"failure","html_url":"https://github.com/o/r/actions/runs/1/job/9"}}]}}'
     fi
     exit 0 ;;
  */logs) echo "Diff in /w/w/crates/fno-agents/src/x.rs:1:"; exit 0 ;;
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
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    }

    #[test]
    fn a_dry_run_touches_nothing_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        stub_gh(d, false);
        stub_git(d, "feature/x", false);
        stub_cargo(d);
        let code = run_heal(&args_for(d, &[]));
        assert_eq!(code, EXIT_ESCALATIONS.max(EXIT_CLEAN), "dry run reports");
        assert_eq!(log_of(d, "cargo.log"), "", "no remedy ran");
        assert!(!log_of(d, "git.log").contains("push"), "no push");
    }

    #[test]
    fn a_dirty_worktree_refuses_before_any_remedy_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        stub_gh(d, false);
        stub_git(d, "feature/x", true);
        stub_cargo(d);
        let code = run_heal(&args_for(d, &["--apply"]));
        assert_eq!(code, EXIT_CWD_REFUSAL);
        assert_eq!(log_of(d, "cargo.log"), "", "no remedy ran");
        assert!(!log_of(d, "git.log").contains("push"));
    }

    #[test]
    fn the_wrong_branch_refuses_too() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        stub_gh(d, false);
        stub_git(d, "main", false);
        stub_cargo(d);
        assert_eq!(run_heal(&args_for(d, &["--apply"])), EXIT_CWD_REFUSAL);
        assert_eq!(log_of(d, "cargo.log"), "");
    }

    #[test]
    fn apply_commits_once_and_pushes_once() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        stub_gh(d, false);
        stub_git(d, "feature/x", false);
        stub_cargo(d);
        run_heal(&args_for(d, &["--apply"]));
        let git = log_of(d, "git.log");
        assert_eq!(git.matches("git commit").count(), 1, "{git}");
        assert_eq!(git.matches("git push").count(), 1, "{git}");
    }

    #[test]
    fn a_run_in_flight_keeps_the_commit_local_and_never_pushes() {
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
    }

    #[test]
    fn a_remedy_that_changed_nothing_takes_no_credit_for_a_dirty_tree() {
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
    }

    #[test]
    fn a_failed_commit_never_reports_the_pr_clean() {
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
    }

    #[test]
    fn dry_run_outside_the_drive_loop_is_a_usage_error() {
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
     echo '[{{"number":1,"head":{{"sha":"aaa1","ref":"feature/x-1111"}},"body":"b"}},{{"number":2,"head":{{"sha":"bbb2","ref":"feature/x-2222"}},"body":"b"}}]'
     exit 0 ;;
  *pulls/1*) echo '{{"head":{{"sha":"aaa1","ref":"feature/x-1111"}},"body":"b"}}'; exit 0 ;;
  *pulls/2*) echo '{{"head":{{"sha":"bbb2","ref":"feature/x-2222"}},"body":"b"}}'; exit 0 ;;
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

    /// A stub `fno` answering the inbox reads. `existing` is the question
    /// list `outstanding --json` reports; every `ask` lands in fno-ask.log.
    fn stub_fno(dir: &Path, existing: &str) {
        write_exec(
            dir,
            "fno",
            &format!(
                r#"#!/bin/sh
D="$(dirname "$0")"
echo "fno $*" >> "$D/fno.log"
case "$*" in
  *"outstanding --json"*) echo '{existing}'; exit 0 ;;
  *"outstanding ask"*) echo "ask $*" >> "$D/fno-ask.log"; exit 0 ;;
esac
exit 0
"#
            ),
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
    fn branch_node_ids_match_the_closure_producers_delimiter_rule() {
        // Parity cases from the Python half and the CI gate: a plain branch
        // names nothing, a node branch names its node, and a trailing segment
        // never re-glues into a second, bogus candidate.
        assert_eq!(branch_node_ids("main"), Vec::<String>::new());
        assert_eq!(branch_node_ids("fix/respawn-race"), Vec::<String>::new());
        assert_eq!(
            branch_node_ids("feature/x-974c"),
            vec!["x-974c".to_string()]
        );
        assert_eq!(
            branch_node_ids("feature/x-cdef-1234"),
            vec!["x-cdef".to_string()],
            "the all-hex suffix must not re-glue into cdef-1234"
        );
        // Fixed-width hex makes x-5b66 a prefix of x-5b667: only the
        // delimiter-bounded one counts.
        assert_eq!(
            branch_node_ids("feature/x-5b667"),
            vec!["x-5b667".to_string()]
        );
    }

    #[test]
    fn the_drive_loop_skips_a_claimed_pr_and_rehearses_the_free_one() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        stub_gh_drive(d, false);
        stub_git_drive(d);
        stub_cargo(d);
        stub_fno(d, r#"{"questions":[]}"#);
        hold_claim(d);
        std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
        let code = run_heal(&drive_args(d, &["--dry-run"]));
        assert_eq!(code, EXIT_CLEAN, "a rehearsal never exits as a real heal");
        let out = log_of(d, "gh.log");
        assert!(out.contains("pulls/2"), "the claimed PR was read: {out}");
        assert_eq!(log_of(d, "cargo.log"), "", "a dry run runs no remedy");
        assert!(!log_of(d, "git.log").contains("push"), "and pushes nothing");
        // The tick row lands with the dry_run marker and both verdicts.
        let events = log_of(d, "events.jsonl");
        assert!(events.contains("pr_heal_tick"), "{events}");
        assert!(events.contains("\"dry_run\":true"), "{events}");
        assert!(events.contains("\"skip_claim_held\":1"), "{events}");
        assert!(events.contains("\"would_heal\":1"), "{events}");
    }

    #[test]
    fn the_drive_loop_heals_the_free_pr_from_its_own_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        stub_gh_drive(d, false);
        stub_git_drive(d);
        stub_cargo(d);
        stub_fno(d, r#"{"questions":[]}"#);
        hold_claim(d);
        std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
        let code = run_heal(&drive_args(d, &[]));
        assert_eq!(code, EXIT_CLEAN, "the free PR healed: {code}");
        let git = log_of(d, "git.log");
        assert_eq!(git.matches("git push").count(), 1, "one push: {git}");
        assert_ne!(log_of(d, "cargo.log"), "", "the remedy ran");
        let events = log_of(d, "events.jsonl");
        assert!(events.contains("\"healed\":1"), "{events}");
        assert!(events.contains("\"skip_claim_held\":1"), "{events}");
    }

    #[test]
    fn an_unknown_signature_escalates_to_the_inbox_once() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        stub_gh_drive(d, true);
        stub_git_drive(d);
        stub_fno(d, r#"{"questions":[]}"#);
        hold_claim(d);
        let code = run_heal(&drive_args(d, &[]));
        assert_eq!(code, EXIT_ESCALATIONS, "the unknown row remains work");
        let asks = log_of(d, "fno-ask.log");
        assert_eq!(asks.matches("outstanding ask").count(), 1, "{asks}");
        assert!(
            asks.contains("no playbook signature") && asks.contains("mystery-check"),
            "{asks}"
        );
        let events = log_of(d, "events.jsonl");
        assert!(events.contains("\"unknown\":1"), "{events}");
    }

    #[test]
    fn a_question_already_on_the_board_is_not_re_asked_each_tick() {
        // The tick fires every 600s; an unanswered question must not become
        // one new inbox row per tick.
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        stub_gh_drive(d, true);
        stub_git_drive(d);
        hold_claim(d);
        stub_fno(
            d,
            r#"{"questions":[{"question":"heal: PR 1 check mystery-check failed with no playbook signature"}]}"#,
        );
        run_heal(&drive_args(d, &[]));
        assert_eq!(log_of(d, "fno-ask.log"), "", "deduped, never re-asked");
    }

    #[test]
    fn a_pr_with_no_worktree_is_named_and_skipped_never_cloned() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        stub_gh_drive(d, false);
        // No stub_git_drive: `git worktree list` fails, so no worktree is
        // found for the free PR's branch.
        stub_git(d, "feature/x-1111", false);
        stub_cargo(d);
        stub_fno(d, r#"{"questions":[]}"#);
        hold_claim(d);
        std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
        let code = run_heal(&drive_args(d, &[]));
        assert_ne!(code, EXIT_CLEAN, "the red PR is still red");
        assert_eq!(log_of(d, "cargo.log"), "", "no remedy ran anywhere");
        let events = log_of(d, "events.jsonl");
        assert!(events.contains("skip_no_worktree"), "{events}");
    }

    #[test]
    fn playbook_exits_clean_without_reading_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        stub_gh(d, false);
        stub_git(d, "feature/x", false);
        assert_eq!(run_heal(&args_for(d, &["--playbook"])), EXIT_CLEAN);
        assert_eq!(log_of(d, "gh.log"), "", "no read");
    }

    #[test]
    fn an_unknown_flag_is_refused_rather_than_ignored() {
        assert_eq!(run_heal(&["--nope".to_string()]), EXIT_READ_ERROR);
    }

    #[test]
    fn open_pr_numbers_reads_one_page_and_several() {
        // --slurp hands back an array of pages, so nothing has to find a
        // boundary. The old hand-rolled `split("][")` cut inside any PR body
        // carrying a markdown reference link and dropped those PRs silently.
        let one: Vec<Value> = vec![json!([{"number": 7}, {"number": 9}])];
        assert_eq!(open_pr_numbers(&one), vec!["7", "9"]);

        let many: Vec<Value> = vec![json!([{"number": 7}]), json!([{"number": 9}])];
        assert_eq!(open_pr_numbers(&many), vec!["7", "9"]);

        let with_bracket_pair: Vec<Value> =
            vec![json!([{"number": 7, "body": "see [the doc][ref]"}])];
        assert_eq!(open_pr_numbers(&with_bracket_pair), vec!["7"]);
    }

    #[test]
    fn a_read_error_outranks_an_escalation_across_prs() {
        // Keeping the LAST non-clean code let a later escalation mask an
        // earlier failure to read the world at all.
        assert_eq!(worse_of(EXIT_READ_ERROR, EXIT_ESCALATIONS), EXIT_READ_ERROR);
        assert_eq!(worse_of(EXIT_ESCALATIONS, EXIT_IN_FLIGHT), EXIT_ESCALATIONS);
        assert_eq!(worse_of(EXIT_CLEAN, EXIT_IN_FLIGHT), EXIT_IN_FLIGHT);
        assert_eq!(worse_of(EXIT_CLEAN, EXIT_CLEAN), EXIT_CLEAN);
    }

    #[test]
    fn a_multi_node_branch_still_yields_its_closure_nodes() {
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
    }

    #[test]
    fn a_rest_conclusion_heal_does_not_know_buckets_fail_never_pass() {
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
}
