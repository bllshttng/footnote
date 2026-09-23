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

use crate::pr_push::{job_id, READ_TIMEOUT};
use regex::Regex;
use serde_json::Value;

/// The shared push layer lives in pr_push. These adapters keep heal's call
/// sites and message shapes unchanged while the implementation is single.
fn run(
    bin: &str,
    args: &[&str],
    cwd: &std::path::Path,
    timeout: std::time::Duration,
) -> Result<(bool, String, String), String> {
    crate::pr_push::run_labeled("pr-heal", bin, args, cwd, timeout)
}

fn gh_api(a: &Args, path: &str, extra: &[&str]) -> Result<String, String> {
    crate::pr_push::gh_api(&a.gh_bin, &a.cwd, path, extra)
}

fn read_checks(a: &Args, head: &str) -> Result<Value, String> {
    crate::pr_push::read_checks(&a.gh_bin, &a.cwd, head)
}

fn gh_api_pages(a: &Args, path: &str) -> Result<Vec<Value>, String> {
    crate::pr_push::gh_api_pages(&a.gh_bin, &a.cwd, path)
}

fn porcelain(a: &Args) -> String {
    crate::pr_push::porcelain(&a.git_bin, &a.cwd)
}

fn dirty(a: &Args) -> bool {
    crate::pr_push::dirty(&a.git_bin, &a.cwd)
}

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
    /// `gh run rerun <id>` (full rerun) for a cancelled run: it reached no
    /// verdict, so rerunning it IS reaching a verdict. `--failed` is wrong
    /// here: it reruns only `failure` conclusions and a cancelled run has
    /// none. Issued at most once per (head sha, run id); a second red verdict
    /// on the same pair escalates.
    Rerun { run_id: String },
    /// `gh run rerun <id> --failed` for a rerunnable escalation (a
    /// test-shaped or unknown red): only the failed jobs rerun. Same
    /// once-per-(sha, run id) guard, and the printed command is the applied
    /// one.
    RerunFailed { run_id: String },
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
    /// The check's `html_url`, carried so the rerun guard can key on the
    /// Actions run id at apply time, when the journal is readable.
    pub link: String,
}

impl Finding {
    /// The report's action column.
    pub(crate) fn action(&self) -> &'static str {
        match self.remedy {
            Remedy::Auto { .. } => "auto",
            Remedy::EditBody { .. } => "edit-body",
            Remedy::Rerun { .. } | Remedy::RerunFailed { .. } => "rerun",
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
            Remedy::EditBody { nodes } => edit_body_cmd(nodes),
            Remedy::Rerun { run_id } => format!("gh run rerun {run_id}"),
            Remedy::RerunFailed { run_id } => format!("gh run rerun {run_id} --failed"),
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
    /// The check's `html_url` (`.../actions/runs/<run>/job/<job>`), which a
    /// remedy aimed at the run itself (a rerun) must resolve.
    pub link: &'a str,
}

/// A signature: how a class of failure is recognized, and what to do about it.
/// `plan` is the `--playbook` column, so the documented remedy and the applied
/// one are the same string's neighbours in one table.
struct Signature {
    name: &'static str,
    plan: &'static str,
    matches: fn(&Ctx) -> bool,
    resolve: fn(&Ctx) -> Remedy,
    /// Whether a rerun can change this class's verdict. A deterministic
    /// failure reruns to the same red and spends CI for nothing.
    rerunnable: bool,
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
        name: "timed_out",
        plan: "escalate: the job hit its timeout-minutes cap; cut or split the work",
        matches: |c| c.log.contains("exceeded the maximum execution time"),
        resolve: |c| {
            Remedy::Escalate {
                repro: format!(
                    "{}: the job hit its timeout-minutes cap, so a rerun hits it again; cut or split the work",
                    c.log
                ),
            }
        },
        rerunnable: false,
    },
    Signature {
        name: "cancelled",
        plan: "rerun: gh run rerun <run>, at most once per head sha",
        matches: |c| c.bucket == "cancel",
        // Measured on three open PRs: every `unknown` heal reported was a
        // CANCELLED check whose log carried one line. A cancelled run
        // concluded nothing, so a rerun is not papering over a defect; it is
        // how the run reaches a verdict at all. 3 of the 15 red open PRs
        // measured on 2026-09-16 were exactly this class. Without a run id
        // in the link there is nothing to rerun, so the row escalates as
        // before.
        resolve: |c| match run_id(c.link) {
            Some(id) => Remedy::Rerun { run_id: id },
            None => Remedy::Escalate {
                repro: "the run was cancelled, so it reached no verdict; push again or rerun it"
                    .to_string(),
            },
        },
        rerunnable: true,
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
        rerunnable: false,
    },
    Signature {
        name: "closure-trailer",
        plan: "edit-body: write ONE Backlog-Closure line naming every node the branch names",
        matches: |c| {
            c.check.contains("check-pr-node-closure")
                && c.log.contains("the exact trailer claims none")
        },
        resolve: |c| Remedy::EditBody {
            nodes: closure_nodes(c.log),
        },
        rerunnable: false,
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
        rerunnable: false,
    },
    Signature {
        name: "mypy",
        plan: "escalate: a type error is a decision, never a mechanical rewrite",
        matches: |c| mypy_re().is_match(c.log),
        resolve: |_| Remedy::Escalate {
            repro: "cd cli && uv run mypy src/".to_string(),
        },
        rerunnable: false,
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
        rerunnable: true,
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
        rerunnable: true,
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
        rerunnable: true,
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
        rerunnable: false,
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
        rerunnable: true,
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
        rerunnable: false,
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
            link: ctx.link.to_string(),
        };
    }
    for sig in SIGNATURES {
        if (sig.matches)(ctx) {
            return Finding {
                check: ctx.check.to_string(),
                signature: sig.name,
                remedy: (sig.resolve)(ctx),
                link: ctx.link.to_string(),
            };
        }
    }
    Finding {
        check: ctx.check.to_string(),
        signature: "unknown",
        remedy: unknown_remedy(ctx.log),
        link: ctx.link.to_string(),
    }
}

/// True when a rerun can change this class's verdict. `unknown` counts: a
/// check no signature recognizes is exactly the check nobody can say is
/// real, which is the rerun's whole point.
fn rerunnable_class(signature: &str) -> bool {
    if signature == "unknown" {
        return true;
    }
    SIGNATURES
        .iter()
        .any(|s| s.name == signature && s.rerunnable)
}

/// The playbook: every signature and its remedy, from the same table
/// `classify` walks. Printed by `--playbook`; deliberately not duplicated into
/// a doc, so the two can never disagree.
pub(crate) fn playbook() -> String {
    let mut out = String::from("signature       rerunnable  remedy\n");
    for sig in SIGNATURES {
        out.push_str(&format!(
            "{:<15} {:<10} {}\n",
            sig.name,
            if sig.rerunnable { "yes" } else { "no" },
            sig.plan
        ));
    }
    out.push_str(&format!(
        "{:<15} {:<10} report only; the same check is red on main HEAD\n",
        "inherited", "no"
    ));
    out.push_str(&format!(
        "{:<15} {:<10} rerun once per (sha, run id), then escalate with the last {TAIL_LINES} log lines\n",
        "unknown", "yes"
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
/// reads `... names x-aaaa, and the exact trailer claims none of them.`
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
pub(crate) fn pytest_nodeids(log: &str) -> Vec<String> {
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
pub(crate) fn cargo_test_names(log: &str) -> Vec<String> {
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

/// The Actions run id out of a check's link
/// (`.../actions/runs/<run>/job/<job>`), which `gh run rerun` names.
pub(crate) fn run_id(link: &str) -> Option<String> {
    let re = Regex::new(r"^https?://[^/]+/[^/]+/[^/]+/actions/runs/(\d+)").expect("static regex");
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

/// A remedy. `cargo fmt` over a large crate is the long pole.
const REMEDY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The whole drive loop's budget: under the pr-watch `StartInterval` of 600s,
/// so a loop cannot span ticks and pile one live store reader on the last.
const DRIVE_BUDGET: std::time::Duration = std::time::Duration::from_secs(480);

/// The cap for one remedy: [`REMEDY_TIMEOUT`], capped at what the root's
/// slice of [`DRIVE_BUDGET`] still has, floored at 1s. A remedy started near
/// the deadline must not run past it.
fn remedy_timeout(a: &Args) -> std::time::Duration {
    match a.deadline {
        None => REMEDY_TIMEOUT,
        Some(d) => d
            .saturating_duration_since(std::time::Instant::now())
            .min(REMEDY_TIMEOUT)
            .max(std::time::Duration::from_secs(1)),
    }
}

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
    /// One control_plane_tick arm row per invocation; the 30s tick slice only
    /// ever pays the spawn, so the drive loop's own timeouts never bound it.
    detach: bool,
    /// `--status`: print one `Heal:` line and exit. The journal, pid files
    /// and arm state are this verb's reads; Python shells it rather than
    /// re-reading the journal itself.
    status: bool,
    /// The arm bit for `--status`, passed by Python after reading
    /// `config.auto_heal.enabled` (one config parser, the Python one).
    armed: bool,
    gh_bin: String,
    git_bin: String,
    cwd: std::path::PathBuf,
    /// Every `--cwd` root, deduplicated; `cwd` is the first. One process
    /// heals every root, so the tick pays one spawn and one pid file.
    roots: Vec<std::path::PathBuf>,
    /// When the root's slice of [`DRIVE_BUDGET`] is spent. Set by the
    /// `--all --apply` entry, never parsed; past it the loop skips and
    /// receipts each PR it did not reach.
    deadline: Option<std::time::Instant>,
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
        detach: false,
        status: false,
        armed: false,
        gh_bin: "gh".to_string(),
        git_bin: "git".to_string(),
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        roots: Vec::new(),
        deadline: None,
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
            "--detach" => a.detach = true,
            "--status" => a.status = true,
            "--armed" => a.armed = true,
            "--gh-bin" => {
                a.gh_bin = take("--gh-bin")?;
                i += 1;
            }
            "--git-bin" => {
                a.git_bin = take("--git-bin")?;
                i += 1;
            }
            "--cwd" => {
                let p = std::path::PathBuf::from(take("--cwd")?);
                if a.roots.is_empty() {
                    a.cwd = p.clone();
                }
                if !a.roots.contains(&p) {
                    a.roots.push(p);
                }
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

/// One PR read off the pulls endpoint: everything `run_one` and the rebase
/// triggers need, in the one call `read_pr` always made.
struct PrState {
    head: String,
    head_ref: String,
    body: String,
    /// The PR's base branch, the `merge-slot:<ref>` claim's key.
    base_ref: String,
    /// GitHub answers `null` while it computes the merge commit; `null` must
    /// never read as conflicting. Measured 2026-09-17: `mergeable_state` on
    /// this repo never reads `behind`, so base drift is NOT read here -- the
    /// compare endpoint owns that and the merge-slot claim already encodes it.
    mergeable: Option<bool>,
}

/// The PR's head sha, head ref, body, base ref, and mergeability.
fn read_pr(a: &Args, pr: &str) -> Result<PrState, String> {
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
    let base_ref = v
        .pointer("/base/ref")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    let mergeable = v.get("mergeable").and_then(|m| m.as_bool());
    if head.is_empty() {
        return Err("pr json carried no head sha".to_string());
    }
    Ok(PrState {
        head,
        head_ref,
        body,
        base_ref,
        mergeable,
    })
}

/// Classify every failing row of one PR. `cached_inherited` carries a
/// drive-loop invocation's one shared read of main's failing checks, so a
/// fleet of N PRs costs ONE main-HEAD read instead of N; `None` means "read
/// it here" and is what the single-PR path passes.
fn findings_for(
    a: &Args,
    pr: &str,
    head: &str,
    cached_inherited: Option<&Vec<String>>,
) -> Result<Vec<Finding>, String> {
    let checks = read_checks(a, head)?;
    // `None` means the main-head read did not answer, which is NOT the same
    // as "main is green". Defaulting it to an empty set silently reclassified
    // every inherited failure as this PR's own, so the caller is told instead
    // and the report says the classification was unavailable.
    let inherited = match cached_inherited {
        Some(v) => v.clone(),
        None => {
            let read = crate::loopcheck::main_head_failing_checks(&a.gh_bin, &a.cwd);
            if read.is_none() {
                println!(
                    "note: could not read main's HEAD, so no row can be shown as inherited; \
                     a failure below may be main's rather than this PR's"
                );
            }
            read.unwrap_or_default()
        }
    };
    let mut out = Vec::new();
    for row in failing_rows(&checks) {
        let check = row["name"].as_str().unwrap_or("").to_string();
        let log = if let Some(timeout) = row.get("timeout").and_then(|v| v.as_str()) {
            timeout.to_string()
        } else {
            match job_id(row["link"].as_str().unwrap_or("")) {
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
            }
        };
        let stripped = strip_timestamps(&log);
        let bucket = row["bucket"].as_str().unwrap_or("").to_string();
        let link = row["link"].as_str().unwrap_or("").to_string();
        out.push(classify(
            &Ctx {
                check: &check,
                log: &stripped,
                bucket: &bucket,
                link: &link,
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
            let bin = seam_bin(a, argv[0]);
            let ok = run(&bin, &argv[1..], &dir, remedy_timeout(a))
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

/// True when the line IS a `Backlog-Closure:` trailer line (anchored at the
/// line start, case-insensitive - the same match the gate's grep makes).
fn is_trailer_line(line: &str) -> bool {
    let prefix = "backlog-closure:";
    line.len() >= prefix.len() && line[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// The ids one `Backlog-Closure:` line claims: the label's own colon
/// stripped, tokens split on whitespace and comma - the grammar
/// `parse_closure_trailer` reads.
fn closure_line_ids(line: &str) -> Vec<String> {
    let Some((label, rest)) = line.split_once(':') else {
        return Vec::new();
    };
    if !label.eq_ignore_ascii_case("backlog-closure") {
        return Vec::new();
    }
    rest.split([' ', '\t', ','])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// The remedy's own node ids, across every EditBody finding, in order.
fn edit_body_nodes(findings: &[Finding]) -> Vec<String> {
    findings
        .iter()
        .filter_map(|f| match &f.remedy {
            Remedy::EditBody { nodes } => Some(nodes.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// The one union line's ids: the remedy's own nodes first, then every id the
/// body's existing trailer lines hold, deduplicated in order. Repeated heals
/// converge - the second heal reads the union and writes the same one line -
/// where append-per-node lost every id but the last to the last-line rule.
fn closure_union(findings: &[Finding], body: &str) -> Vec<String> {
    let mut union = edit_body_nodes(findings);
    for id in body
        .lines()
        .filter(|line| is_trailer_line(line))
        .flat_map(closure_line_ids)
    {
        if !union.contains(&id) {
            union.push(id);
        }
    }
    union
}

/// The body with every trailer line removed - the base the one new line is
/// appended to, so no stale line can outrank it.
fn body_without_trailer_lines(body: &str) -> String {
    body.lines()
        .filter(|line| !is_trailer_line(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The one-line remedy form: one positional id, the rest as `--extra`. The
/// verb takes one positional, so the old two-positional text named a command
/// that could not run.
fn edit_body_cmd(nodes: &[String]) -> String {
    match nodes.split_first() {
        Some((first, rest)) => {
            let mut cmd = format!("fno do pr closure-trailer {first}");
            for extra in rest {
                cmd.push_str(" --extra ");
                cmd.push_str(extra);
            }
            cmd
        }
        None => "fno do pr closure-trailer <node-id>".to_string(),
    }
}

/// Edit the PR body so exactly ONE Backlog-Closure line names every node the
/// heal covers plus every id the body's existing trailer lines held. No
/// commit and no push: the closure workflow re-fires on an `edited` event.
fn apply_edit_body(a: &Args, pr: &str, body: &str, findings: &[Finding]) -> Result<bool, String> {
    let union = closure_union(findings, body);
    if union.is_empty() {
        return Ok(false);
    }
    // The trailer is generated by the verb that checks each id against the
    // graph. Pasting a candidate out of the refusal is the exact move that
    // refusal warns against: a branch segment can match the id grammar
    // without naming a real node.
    let mut args: Vec<&str> = vec!["do", "pr", "closure-trailer", &union[0]];
    for extra in &union[1..] {
        args.push("--extra");
        args.push(extra);
    }
    let (ok, out, _) = run("fno", &args, &a.cwd, READ_TIMEOUT)?;
    // The empty-stdout half is not defensive padding. Measured against the
    // real verb: an id the graph does not know exits 0 and prints NOTHING,
    // with nothing on stderr either. One unknown id - a stale one carried on
    // an old trailer line, say - fails the whole edit instead of landing a
    // line that binds nothing.
    if !ok || out.trim().is_empty() {
        return Err(format!(
            "could not generate a closure trailer for {}",
            union.join(", ")
        ));
    }
    let new_body = format!(
        "{}\n\n{}\n",
        body_without_trailer_lines(body).trim_end(),
        out.trim()
    );
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

/// `fno-agents pr-heal <n> [--apply] [--all] [--apply --detach] [--status] [--playbook]`.
pub fn run_heal(argv: &[String]) -> i32 {
    let a = match parse_args(argv) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("pr-heal: {msg}");
            eprintln!("usage: pr-heal <pr> [--apply] | --all [--apply] [--dry-run] | --playbook | --status");
            return EXIT_READ_ERROR;
        }
    };
    if a.playbook {
        print!("{}", playbook());
        return EXIT_CLEAN;
    }
    if a.status {
        println!("{}", status_line(&a));
        return EXIT_CLEAN;
    }
    if a.detach {
        // The drive loop detached from the tick: the 30s phase slice only
        // ever pays the spawn. `--detach` is a drive-loop flag; without
        // --all --apply behind it there is nothing to detach.
        if !(a.all && a.apply) {
            eprintln!("pr-heal: --detach rehearses nothing and reports nothing on its own; it belongs to --all --apply");
            return EXIT_READ_ERROR;
        }
        return run_detached(
            &a,
            argv,
            &crate::loops_pause::dispatch_pause,
            &spawn_detached,
        );
    }
    if a.all && a.apply {
        // The loop is bounded to the tick: DRIVE_BUDGET, split evenly across
        // the roots, so no drive loop outlives the tick that spawned it.
        let mut a = a;
        a.deadline = Some(std::time::Instant::now() + DRIVE_BUDGET);
        if a.roots.len() > 1 {
            return run_roots_apply(&a, a.dry_run);
        }
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
    let (code, reran_keys) = run_one(&a, &pr);
    if !reran_keys.is_empty() {
        // The once-per-sha rerun guard reads rerun_shas off pr_heal_tick
        // rows. The drive loop writes its own; this is the single-PR apply
        // path's row, so a manual rerun is never issued a second time.
        emit_tick_event(
            &a,
            &std::collections::BTreeMap::new(),
            0,
            false,
            &reran_keys,
            &[], // no acted PRs on the single-PR path: the status line's
            // acted list is a drive-loop readout
            0.0,
        );
    }
    code
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
        let (code, _) = run_one(a, &num);
        worst = worse_of(worst, code);
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

/// Node ids a PR head ref names, as delimiter-bounded segments. Re-exported
/// from the shared `king_board::prs` predicate, so the heal drive loop, the
/// board, and the merge owner all read one rule (parity with
/// `cli/src/fno/pr/closure.py`).
pub(crate) use crate::king_board::prs::branch_node_ids;

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

/// The live merge-slot holder for `base_ref`, as a PR number. The same claim
/// and `pr:<n>` shape `authorized_merge` writes; heal only READS it -- never
/// acquire, never release, never refresh. The empty base (an older stub read
/// or a degenerate payload) names no slot. The root resolves through the test
/// seam when set, else the canonical repo root, exactly as the merge sweep's
/// own `slot_holder_read` does.
fn merge_slot_holder_pr(
    base_ref: &str,
    claims_root: Option<&std::path::Path>,
    cwd: &std::path::Path,
) -> Option<u64> {
    if base_ref.is_empty() {
        return None;
    }
    let root = match claims_root {
        Some(r) => Some(r.to_path_buf()),
        None => crate::paths::canonical_repo_root(cwd),
    };
    let (state, rec) = crate::claims::status(&format!("merge-slot:{base_ref}"), root.as_deref());
    if !matches!(
        state,
        crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect
    ) {
        return None;
    }
    crate::authorized_merge::parse_slot_holder(&rec.map(|r| r.holder).unwrap_or_default())
}

/// Rebases allowed in one drive-loop run. Both triggers are bounded by
/// construction, so the cap should never bind; if it does, a trigger leaked
/// and the loop must stop rather than restart CI on the whole fleet.
const REBASE_BUDGET: usize = 6;

/// `behind-before=` / `behind-after=` values out of the push verb's receipt
/// line. An unparseable receipt reads `?`, never a silent zero.
fn behind_numbers(out: &str) -> (String, String) {
    let grab = |tag: &str| {
        out.split(tag)
            .nth(1)
            .map(|rest| rest.split_whitespace().next().unwrap_or("?").to_string())
            .unwrap_or_else(|| "?".to_string())
    };
    (grab("behind-before="), grab("behind-after="))
}

/// The push verb's conflict exit: the rebase leg refused because the branch
/// is not safely rebasable (needs_resolver or refused). The verb's other
/// exit-3 refusals (protected branch, dirty tree) are NOT conflicts.
fn is_rebase_conflict(stderr: &str) -> bool {
    stderr.contains("not safely rebasable")
}

/// The conflicting file list out of the push verb's conflict stderr
/// (`... status needs_resolver; files: a, b). Resolve ...`).
fn conflict_files(stderr: &str) -> String {
    stderr
        .split("files: ")
        .nth(1)
        .map(|rest| rest.split(')').next().unwrap_or("").trim().to_string())
        .unwrap_or_default()
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
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

/// The fleet-task store: `questions.jsonl` beside the events journal, so a
/// test's `--events-file` moves it with the journal.
fn questions_store(a: &Args) -> std::path::PathBuf {
    journal_path(a).with_file_name("questions.jsonl")
}

/// File one heal fleet task, deduplicated on (lane, key, cwd), attributed to
/// the node the branch names. True when a NEW task was filed; an already
/// open one reads false, so the escalated receipt stays honest.
fn file_task(a: &Args, key: &str, text: &str, run_cmd: &str, head_ref: &str) -> bool {
    let node = branch_node_ids(head_ref).into_iter().next();
    match crate::fleet_task::file_once(
        &questions_store(a),
        "heal",
        key,
        a.cwd.to_string_lossy().as_ref(),
        text,
        Some(run_cmd),
        node.as_deref(),
    ) {
        Ok(crate::fleet_task::Filed::New(_)) => true,
        Ok(crate::fleet_task::Filed::Duplicate(_)) => false,
        Err(e) => {
            eprintln!("pr-heal: task refused: {e}");
            false
        }
    }
}

/// File one fleet task for a failing check no signature recognized.
/// Deduplicated on (lane, key, cwd), so a 600s tick cannot re-file a task
/// the store already carries.
fn escalate_unknown_signature(a: &Args, pr: &str, check: &str, head_ref: &str) -> bool {
    let key = format!("PR {pr} check {check}");
    let text = format!(
        "heal: {key} failed with no playbook signature. Classify it with \
         `fno do pr heal {pr}` (that report carries the log tail) or add a \
         signature in crates/fno-agents/src/heal.rs."
    );
    file_task(a, &key, &text, &format!("fno do pr heal {pr}"), head_ref)
}

/// The events journal for this invocation. `--events-file` is the test seam;
/// the default is the global `~/.fno/events.jsonl`, the same journal the
/// tick's own `_emit_event` and `fno doctor event` read.
fn journal_path(a: &Args) -> std::path::PathBuf {
    if a.events_file.is_empty() {
        crate::paths::AgentsHome::from_env()
            .root()
            .parent()
            .map(|p| p.join("events.jsonl"))
            .unwrap_or_else(|| std::path::PathBuf::from(".fno/events.jsonl"))
    } else {
        std::path::PathBuf::from(&a.events_file)
    }
}

/// One `control_plane_tick` arm row per detach decision: acted=1 on a spawn,
/// acted=0 with a skip_reason otherwise. Same event type and arm/acted/
/// skip_reason/detail vocabulary the tick's own `_emit_tick_row` writes, so
/// the journal agrees with the status line on why nothing ran.
fn emit_arm_row(a: &Args, acted: u8, skip_reason: Option<&str>, detail: &str) {
    let mut fields = serde_json::Map::new();
    fields.insert("arm".to_string(), serde_json::json!("heal"));
    fields.insert("acted".to_string(), serde_json::json!(acted));
    if let Some(s) = skip_reason {
        fields.insert("skip_reason".to_string(), serde_json::json!(s));
    }
    if !detail.is_empty() {
        fields.insert("detail".to_string(), serde_json::json!(detail));
    }
    if let Err(e) = crate::events::EventEmitter::new(journal_path(a), "pr-heal")
        .emit_fields("control_plane_tick", fields)
    {
        eprintln!("pr-heal: the control_plane_tick arm row did not land: {e}");
    }
}

/// The dir holding the journal; also where the drive loop's pid file lives.
fn events_dir(a: &Args) -> std::path::PathBuf {
    journal_path(a)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// The pid file for THE drive loop. One process heals every root, so one
/// pid file: the in-flight guard sees the loop whatever root asked, and
/// `live_heal_pids`' `pr-heal.` / `.pid` match still reads the name.
fn heal_pid_file(a: &Args) -> std::path::PathBuf {
    events_dir(a).join("pr-heal.pid")
}

// Test-only reader since the guard switched to live_heal_pids.
#[cfg_attr(not(test), allow(dead_code))]
fn read_pid_file(path: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Spawn `argv` as the leader of a new session with stdio on /dev/null (the
/// `evals_arm.rs` shape), and answer the child pid.
fn spawn_detached(argv: &[String]) -> std::io::Result<u32> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    // SAFETY: setsid() is async-signal-safe and takes no arguments; called
    // here it runs in the child after fork, before exec, single-threaded.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        })
    };
    cmd.spawn().map(|c| c.id())
}

/// `--detach`: one pid-file probe, one spawn, one arm row, exit 0. The child
/// is this binary with the same args minus `--detach`, in its own session
/// with stdio on /dev/null, so the tick's 30s slice only pays the spawn and
/// the drive loop's own 60s/300s bounds are what apply. `argv` is the exact
/// args this process received (the child argv re-adds the verb).
fn run_detached(
    a: &Args,
    argv: &[String],
    pause: &dyn Fn() -> crate::loops_pause::DispatchPause,
    spawn: &dyn Fn(&[String]) -> std::io::Result<u32>,
) -> i32 {
    // An armed load breaker holds this drive loop: heal dispatches workers,
    // so it obeys the pause the spawn gate already enforces at admission,
    // and it answers on its own arm row instead of feeding the gate a spawn
    // to refuse. Exit 0 is a drive-loop verdict: the Python phase reads
    // "ran" and writes no second row, so the journal holds exactly one heal
    // row for this tick.
    let hold = pause();
    if hold.is_paused() {
        emit_arm_row(a, 0, Some(hold.skip_reason()), &hold.detail());
        return EXIT_CLEAN;
    }
    let pid_file = heal_pid_file(a);
    // One live loop holds the tick, whatever file names it: the single
    // pr-heal.pid, or a stale per-root file a pre-upgrade loop still runs
    // in (the sweep filters dead pids).
    if let Some(pid) = live_heal_pids(&events_dir(a)).first() {
        emit_arm_row(
            a,
            0,
            Some("in_flight"),
            &format!("pid {pid} is still running the drive loop"),
        );
        return EXIT_CLEAN;
    }
    let mut child: Vec<String> = Vec::with_capacity(argv.len() + 2);
    child.push(crate::evals_arm::self_exe());
    child.push("pr-heal".to_string());
    child.extend(argv.iter().filter(|s| s.as_str() != "--detach").cloned());
    match spawn(&child) {
        Ok(pid) => {
            if let Some(dir) = pid_file.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let wrote = std::fs::write(&pid_file, format!("{pid}\n"));
            match wrote {
                Ok(()) => emit_arm_row(a, 1, None, &format!("spawned pid {pid}")),
                Err(e) => emit_arm_row(
                    a,
                    1,
                    None,
                    &format!("spawned pid {pid}; pid file write failed: {e}"),
                ),
            }
        }
        Err(e) => emit_arm_row(a, 0, Some("spawn_failed"), &format!("{e}")),
    }
    EXIT_CLEAN
}

/// The newest `pr_heal_tick` row: (ts, data), or None when none exists.
/// Committed store rows are the record (the cutover stopped journal
/// appends); commit order via `seq` makes the last row the newest.
fn newest_heal_tick(path: &std::path::Path) -> Option<(String, Value)> {
    let _ = crate::event_store::import_all(path);
    crate::event_store::query_events(
        path,
        &crate::event_store::EventQuery {
            types: vec!["pr_heal_tick".to_string()],
            ..Default::default()
        },
    )
    .ok()?
    .last()
    .and_then(|row| serde_json::from_str::<Value>(&row.line).ok())
    .and_then(|row| {
        let ts = row.get("ts").and_then(Value::as_str)?.to_string();
        let data = row.get("data").cloned().unwrap_or(Value::Null);
        Some((ts, data))
    })
}

/// Minutes under 24h, else days: one age vocabulary for the status line.
fn age_phrase(ts: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(t) => {
            let mins = (chrono::Utc::now() - t.with_timezone(&chrono::Utc))
                .num_minutes()
                .max(0);
            if mins < 24 * 60 {
                format!("{mins}m ago")
            } else {
                format!("{}d ago", mins / (24 * 60))
            }
        }
        Err(_) => "age unknown".to_string(),
    }
}

/// True when the pid names a healer process. A stale pid file outlives its
/// loop, and pid numbers are recycled: without an identity check a recycled
/// pid holds the tick (every root) for as long as the unrelated owner lives.
/// `ps -o command=` answers on macOS and Linux alike; an unreadable answer
/// counts as NOT the healer (fail open to a spawn, never stuck in_flight).
/// The current process is exempt: production never writes its own pid to a
/// file, and the test harness does exactly that.
fn pid_names_a_healer(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    let Ok((true, out, _)) = run(
        "ps",
        &["-p", &pid.to_string(), "-o", "command="],
        &std::env::temp_dir(),
        READ_TIMEOUT,
    ) else {
        return false;
    };
    out.contains("pr-heal") || out.contains("fno-agents")
}

/// Live pids across every `pr-heal.*.pid` file in `dir`. The pid lives in the
/// file CONTENT, read as an integer; EPERM counts alive. The glob also sweeps
/// the old per-root `pr-heal.<tag>.pid` files; they hold dead pids and skip,
/// and a recycled pid that belongs to an unrelated process skips too.
fn live_heal_pids(dir: &std::path::Path) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let fname = e.file_name();
        let Some(name) = fname.to_str() else {
            continue;
        };
        if !name.starts_with("pr-heal.") || !name.ends_with(".pid") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let Ok(pid) = text.trim().parse::<u32>() else {
            continue;
        };
        if crate::evals_arm::pid_alive(pid) && pid_names_a_healer(pid) {
            out.push(pid);
        }
    }
    out.sort_unstable();
    out
}

/// The one `Heal:` readout line. `--status` prints it; `_install.py` shells
/// this verb rather than re-reading the journal in Python.
fn status_line(a: &Args) -> String {
    if !a.armed {
        return "Heal: unarmed (auto_heal.enabled=false; arm with: fno config set auto_heal.enabled true)".to_string();
    }
    let Some((ts, data)) = newest_heal_tick(&journal_path(a)) else {
        return "Heal: armed; never ran".to_string();
    };
    let healed = data.get("healed").and_then(Value::as_u64).unwrap_or(0);
    let escalated = data.get("escalated").and_then(Value::as_u64).unwrap_or(0);
    let rebased = data.get("rebased").and_then(Value::as_u64).unwrap_or(0);
    let reran = data.get("reran").and_then(Value::as_u64).unwrap_or(0);
    let acted = data
        .get("acted_prs")
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(|x| x.as_u64().map(|n| n.to_string()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let acted_clause = if acted.is_empty() {
        "acted on nothing".to_string()
    } else {
        format!("acted on PR {acted}")
    };
    let pids = live_heal_pids(&events_dir(a));
    let in_flight = if pids.is_empty() {
        "none".to_string()
    } else {
        pids.iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    // One row names one root's run: a multi-root tick writes one row per
    // root, so the line labels whose counts these are.
    let root = data
        .get("root")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    format!(
        "Heal: armed; last run {ts} ({}, root {root}); healed {healed}, rebased {rebased}, reran {reran}, escalated {escalated}; {acted_clause}; in-flight {in_flight}",
        age_phrase(&ts)
    )
}

/// True when a prior `pr_heal_tick` row names `key` in `rerun_keys`: the
/// once-per-(sha, run id) guard shared by every rerun class. The journal is
/// the state; a second red verdict on the same pair means the rerun reached a
/// real result.
fn journal_has_rerun(path: &std::path::Path, key: &str) -> bool {
    let _ = crate::event_store::import_all(path);
    let Ok(rows) = crate::event_store::query_events(
        path,
        &crate::event_store::EventQuery {
            types: vec!["pr_heal_tick".to_string()],
            ..Default::default()
        },
    ) else {
        return false;
    };
    rows.iter().any(|r| {
        serde_json::from_str::<Value>(&r.line)
            .ok()
            .is_some_and(|row| {
                row.get("data")
                    .and_then(|d| d.get("rerun_keys"))
                    .and_then(|v| v.as_array())
                    .is_some_and(|keys| keys.iter().any(|k| k.as_str() == Some(key)))
            })
    })
}

/// Every `(sha, run id)` pair any tick ever reran, newest last. The drive
/// loop's one store read per run; flake detection filters it per PR head.
// ponytail: O(rows) per run; the drive loop runs every 30m and already
// reads GitHub several times per PR. A keyed sidecar is the upgrade path if
// the row count ever gets big enough to measure.
fn journal_rerun_keys(path: &std::path::Path) -> Vec<String> {
    let _ = crate::event_store::import_all(path);
    let Ok(rows) = crate::event_store::query_events(
        path,
        &crate::event_store::EventQuery {
            types: vec!["pr_heal_tick".to_string()],
            ..Default::default()
        },
    ) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|r| serde_json::from_str::<Value>(&r.line).ok())
        .filter_map(|row| {
            row.get("data")
                .and_then(|d| d.get("rerun_keys"))
                .and_then(|v| v.as_array())
                .map(|keys| {
                    keys.iter()
                        .filter_map(|k| k.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
        })
        .flatten()
        .collect()
}

/// The flake ledger. A rerun that came back green proves the first red was
/// not real. One `pr_heal_flake` row per (sha, run id, check), guarded so the
/// same observation never lands twice; the key is the failing test when the
/// log named one, else the check name. A key's third row files one node, so
/// a flake that reruns green forever is visible by construction.
fn detect_flakes(
    a: &Args,
    head: &str,
    rerun_keys_ever: &[String],
    flake_guards: &std::collections::HashSet<String>,
) {
    let mut run_ids: Vec<String> = Vec::new();
    for k in rerun_keys_ever {
        if let Some((sha, run)) = k.split_once(':') {
            if sha == head {
                run_ids.push(run.to_string());
            }
        }
    }
    if run_ids.is_empty() {
        return;
    }
    // An empty or unreadable checks read is never a green: a run GitHub has
    // not registered (or another API fault) is not evidence of anything.
    let Ok(rows) = crate::pr_push::read_checks_rows(&a.gh_bin, &a.cwd, head) else {
        return;
    };
    for run in run_ids {
        let run_rows: Vec<&Value> = rows
            .iter()
            .filter(|r| run_id(r["link"].as_str().unwrap_or("")).as_deref() == Some(&run))
            .collect();
        if run_rows.is_empty()
            || run_rows
                .iter()
                .any(|r| matches!(r["bucket"].as_str(), Some("fail") | Some("cancel")))
        {
            continue;
        }
        for row in run_rows {
            let check = row["name"].as_str().unwrap_or("").to_string();
            if check.is_empty() {
                continue;
            }
            let key_guard = format!("{head}:{run}:{check}");
            if flake_guards.contains(&key_guard) {
                continue;
            }
            // Where the log named a failing test, the key is the test; else
            // the key is the check name. One best-effort log read, once per
            // (sha, run id, check) -- the guard above keeps it rare.
            let log = match crate::pr_push::job_id(row["link"].as_str().unwrap_or("")) {
                Some(id) => gh_api(
                    a,
                    &format!("repos/{{owner}}/{{repo}}/actions/jobs/{id}/logs"),
                    &[],
                )
                .map(|raw| strip_timestamps(&raw))
                .unwrap_or_default(),
                None => String::new(),
            };
            let test = [
                pytest_nodeids(&log).into_iter().next(),
                cargo_test_names(&log).into_iter().next(),
                shard_rollup_shards(&log),
            ]
            .into_iter()
            .flatten()
            .next();
            let key = test.unwrap_or_else(|| check.clone());
            let prior = journal_flake_rows_for_key(&journal_path(a), &key);
            let existing_node = prior.iter().flatten().next().cloned();
            let node = if prior.len() + 1 >= 3 && existing_node.is_none() {
                file_flake_node(a, &key)
            } else {
                existing_node
            };
            emit_flake_row(a, &key_guard, &key, head, &run, &check, node.as_deref());
        }
    }
}

/// Every `(sha, run id, check)` the flake ledger already recorded, read once
/// per drive-loop run and consulted in memory.
fn journal_flake_guards(path: &std::path::Path) -> std::collections::HashSet<String> {
    let text = crate::event_store::journal_text(path, &["pr_heal_flake"]);
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|row| row.get("type").and_then(Value::as_str) == Some("pr_heal_flake"))
        .filter_map(|row| {
            row.get("data")
                .and_then(|d| d.get("key_guard"))
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        })
        .collect()
}

/// The node ids already filed for a flake key, and how many rows carry it.
fn journal_flake_rows_for_key(path: &std::path::Path, key: &str) -> Vec<Option<String>> {
    let text = crate::event_store::journal_text(path, &["pr_heal_flake"]);
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|row| row.get("type").and_then(Value::as_str) == Some("pr_heal_flake"))
        .filter_map(|row| {
            let data = row.get("data")?;
            if data.get("key").and_then(Value::as_str) == Some(key) {
                Some(
                    data.get("node_id")
                        .and_then(Value::as_str)
                        .map(|s| s.to_string()),
                )
            } else {
                None
            }
        })
        .collect()
}

/// File the node behind the `fno backlog idea` verb through the seam, and
/// parse the node id out of its output. Best-effort: a refused filing lands
/// no row and the next occurrence retries.
fn file_flake_node(a: &Args, key: &str) -> Option<String> {
    let bin = seam_bin(a, "fno");
    let msg = format!("{key} is flaky: passed on rerun 3 times");
    let (ok, out, _) = run(
        &bin,
        &["backlog", "idea", &msg, "--source-kind", "from_observation"],
        &a.cwd,
        READ_TIMEOUT,
    )
    .ok()?;
    if !ok {
        return None;
    }
    let re = Regex::new(r"\b[a-z]+-[0-9a-f]{4,}\b").expect("static regex");
    re.captures(&out).map(|c| c[0].to_string())
}

/// One `pr_heal_flake` journal row.
fn emit_flake_row(
    a: &Args,
    key_guard: &str,
    key: &str,
    sha: &str,
    run: &str,
    check: &str,
    node_id: Option<&str>,
) {
    let mut fields = serde_json::Map::new();
    fields.insert("key_guard".to_string(), serde_json::json!(key_guard));
    fields.insert("key".to_string(), serde_json::json!(key));
    fields.insert("sha".to_string(), serde_json::json!(sha));
    fields.insert("run_id".to_string(), serde_json::json!(run));
    fields.insert("check".to_string(), serde_json::json!(check));
    if let Some(n) = node_id {
        fields.insert("node_id".to_string(), serde_json::json!(n));
    }
    if let Err(e) = crate::events::EventEmitter::new(journal_path(a), "pr-heal")
        .emit_fields("pr_heal_flake", fields)
    {
        eprintln!("pr-heal: the pr_heal_flake row did not land: {e}");
    }
}

/// Issue `gh run rerun` for every rerunnable finding: the cancelled runs that
/// classify straight to [`Remedy::Rerun`], and the test-shaped or unknown
/// escalations no one can call real yet. Guarded once per (sha, run id) --
/// the dedup key, because nine failing checks can share one run id and one
/// `--failed` rerun covers them all. An already-issued or failed rerun
/// demotes the row to Escalate, so the report never reads the PR as clean
/// while the run is still red. Returns the keys issued this run.
fn apply_rerun(a: &Args, findings: &mut [Finding], head: &str) -> Vec<String> {
    let mut issued: Vec<String> = Vec::new();
    // One journal read per call: every (sha, run id) any tick ever reran.
    let known: std::collections::HashSet<String> =
        journal_rerun_keys(&journal_path(a)).into_iter().collect();
    for f in findings.iter_mut() {
        let run_id = match &f.remedy {
            Remedy::Rerun { run_id } | Remedy::RerunFailed { run_id } => Some(run_id.clone()),
            // A real failure reruns only its failed jobs. A cancelled run has
            // no failures to name, so `--failed` would rerun nothing.
            Remedy::Escalate { .. } if rerunnable_class(f.signature) => run_id(&f.link),
            _ => None,
        };
        let Some(run_id) = run_id else {
            continue;
        };
        let key = format!("{head}:{run_id}");
        let was_cancelled = matches!(f.remedy, Remedy::Rerun { .. });
        let rerun_args: Vec<&str> = if was_cancelled {
            vec!["run", "rerun", &run_id]
        } else {
            vec!["run", "rerun", &run_id, "--failed"]
        };
        if known.contains(&key) || issued.iter().any(|k| k == &key) {
            let short = &head[..head.len().min(12)];
            f.remedy = match f.remedy.clone() {
                Remedy::Rerun { .. } => Remedy::Escalate {
                    repro: format!(
                        "the run was cancelled and a rerun was already issued for sha {short}; \
                         if it is still red it reached a real verdict: fix it or rerun by hand"
                    ),
                },
                Remedy::RerunFailed { .. } | Remedy::Escalate { .. } => {
                    let prior_repro = match f.remedy.clone() {
                        Remedy::Escalate { repro } => repro,
                        _ => String::new(),
                    };
                    Remedy::Escalate {
                        repro: format!(
                            "failed twice on the same sha {short}; a rerun already ran. {prior_repro}"
                        ),
                    }
                }
                other => other,
            };
            continue;
        }
        match run(&a.gh_bin, &rerun_args, &a.cwd, remedy_timeout(a)) {
            Ok((true, _, _)) => {
                issued.push(key);
                // The report names the rerun as the action taken, and the
                // printed command matches the applied argv.
                f.remedy = if was_cancelled {
                    Remedy::Rerun { run_id }
                } else {
                    Remedy::RerunFailed { run_id }
                };
            }
            Ok((false, _, err)) => {
                f.remedy = Remedy::Escalate {
                    repro: format!("gh run rerun {run_id} refused: {}", err.trim()),
                };
            }
            Err(e) => {
                f.remedy = Remedy::Escalate {
                    repro: format!("gh run rerun {run_id} failed: {e}"),
                };
            }
        }
    }
    issued
}

/// One `pr_heal_tick` row per drive-loop invocation: the arm's visibility.
/// Written to the global `~/.fno/events.jsonl` (or `--events-file`), the same
/// journal `fno do pr watch status` reads through `pr-heal --status`.
/// `escalated` sums the rows a person must look at; `rerun_keys` is the
/// once-per-(sha, run id) guard's ledger, and `acted_prs` names the PRs the
/// run acted on, for the `Heal:` status line.
#[allow(clippy::too_many_arguments)]
fn emit_tick_event(
    a: &Args,
    counts: &std::collections::BTreeMap<&'static str, usize>,
    unknown: usize,
    dry_run: bool,
    reran_keys: &[String],
    acted_prs: &[String],
    duration_s: f64,
) {
    let path = journal_path(a);
    let mut fields = serde_json::Map::new();
    // One process writes one pr_heal_tick row per root; `root` names whose.
    fields.insert(
        "root".to_string(),
        serde_json::json!(a.cwd.to_string_lossy()),
    );
    for (k, v) in counts {
        fields.insert((*k).to_string(), serde_json::json!(v));
    }
    fields.insert("unknown".to_string(), serde_json::json!(unknown));
    fields.insert("dry_run".to_string(), serde_json::json!(dry_run));
    // Explicit defaults: the newest tick row must carry the keys the done
    // probe asserts on even when a run acted on nothing.
    fields.insert(
        "rebased".to_string(),
        serde_json::json!(counts.get("rebased").copied().unwrap_or(0)),
    );
    let escalated = counts.get("still_red").unwrap_or(&0)
        + counts.get("skip_escalate_only").unwrap_or(&0)
        + unknown;
    fields.insert("escalated".to_string(), serde_json::json!(escalated));
    fields.insert("reran".to_string(), serde_json::json!(reran_keys.len()));
    if !reran_keys.is_empty() {
        fields.insert("rerun_keys".to_string(), serde_json::json!(reran_keys));
    }
    if !acted_prs.is_empty() {
        // The status line reads these back as numbers; a string row would
        // render as "acted on nothing" whatever the run did.
        let nums: Vec<u64> = acted_prs.iter().filter_map(|p| p.parse().ok()).collect();
        fields.insert("acted_prs".to_string(), serde_json::json!(nums));
    }
    fields.insert(
        "duration_s".to_string(),
        serde_json::json!((duration_s * 1000.0).round() / 1000.0),
    );
    if let Err(e) =
        crate::events::EventEmitter::new(path, "pr-heal").emit_fields("pr_heal_tick", fields)
    {
        eprintln!("pr-heal: the pr_heal_tick row did not land: {e}");
    }
}

/// The multi-root drive loop: one process heals every `--cwd` root, so the
/// tick pays one spawn and one pid file. Each root runs with an even share
/// of what is left of [`DRIVE_BUDGET`]; a root that spends less passes its
/// unspent time on. Each root's `run_all_apply` writes its own `pr_heal_tick`
/// row, and `root` on the row names whose.
fn run_roots_apply(a: &Args, dry_run: bool) -> i32 {
    let deadline = a
        .deadline
        .unwrap_or_else(|| std::time::Instant::now() + DRIVE_BUDGET);
    let mut worst = EXIT_CLEAN;
    let mut roots_left = a.roots.len();
    for root in &a.roots {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            // Out of budget: the root's PRs wait for the next tick. Its
            // pre-loop reads (listing, worktree scan, main HEAD) would each
            // pay up to 60s past the deadline for zero work.
            roots_left -= 1;
            continue;
        }
        let slice = remaining / roots_left as u32;
        let root_args = Args {
            cwd: root.clone(),
            deadline: Some(std::time::Instant::now() + slice),
            ..a.clone()
        };
        worst = worse_of(worst, run_all_apply(&root_args, dry_run));
        roots_left -= 1;
    }
    worst
}

/// The drive loop: one heal per red open PR, from that PR's own worktree,
/// behind four refusals -- claim free, known signature, one push per PR per
/// cycle, inherited failures named and skipped. `--dry-run` walks every
/// refusal and prints the plan without touching a worktree or the inbox.
fn run_all_apply(a: &Args, dry_run: bool) -> i32 {
    let t0 = std::time::Instant::now();
    let pages = match gh_api_pages(a, "repos/{owner}/{repo}/pulls?state=open&per_page=100") {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("pr-heal: {msg}");
            return EXIT_READ_ERROR;
        }
    };
    let worktrees = worktrees_by_branch(&a.git_bin, &a.cwd);
    // One main-HEAD read per invocation, shared by every PR: the failing
    // checks of origin/main do not change between PRs in one cycle, and N
    // reads per tick is exactly the shared-quota spend the broker refuses.
    let inherited_once = crate::loopcheck::main_head_failing_checks(&a.gh_bin, &a.cwd);
    if inherited_once.is_none() {
        println!(
            "note: could not read main's HEAD, so no row can be shown as inherited; \
             a failure below may be main's rather than this PR's"
        );
    }
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
    // The trailing Option is the finding's rerun key (`sha:runid`), present
    // when the check's link names a run: an unknown that just got its one
    // rerun must not escalate in the same breath.
    let mut unknown: Vec<(String, String, String, Option<String>)> = Vec::new();
    let mut reran_keys: Vec<String> = Vec::new();
    let mut acted_prs: Vec<String> = Vec::new();
    let mut rebased_this_run = 0usize;
    // The epic's bar: no PR sits for more than one tick without a receipt
    // saying why. One row per PR per run, whatever the outcome was.
    let receipt = |pr: &str, action: &str, reason: &str| {
        if dry_run {
            return;
        }
        let mut fields = serde_json::Map::new();
        fields.insert("pr".to_string(), serde_json::json!(pr));
        fields.insert("action".to_string(), serde_json::json!(action));
        fields.insert("reason".to_string(), serde_json::json!(reason));
        if let Err(e) = crate::events::EventEmitter::new(journal_path(a), "pr-heal")
            .emit_fields("pr_heal_pr", fields)
        {
            eprintln!("pr-heal: the pr_heal_pr row for PR {pr} did not land: {e}");
        }
    };
    // One journal read per run for each ledger: every (sha, run id) any tick
    // ever reran, and every (sha, run id, check) already recorded as a flake.
    let rerun_keys_ever = journal_rerun_keys(&journal_path(a));
    let flake_guards = journal_flake_guards(&journal_path(a));
    let mut worst = EXIT_CLEAN;
    // The root's slice of the drive budget: past it, every PR not yet
    // reached gets one receipt and waits for the next tick.
    let deadline = a
        .deadline
        .unwrap_or_else(|| std::time::Instant::now() + DRIVE_BUDGET);
    for pr in open_pr_numbers(&pages) {
        bump(&mut counts, "seen");
        if std::time::Instant::now() >= deadline {
            bump(&mut counts, "skip_deadline");
            receipt(
                &pr,
                "skip_deadline",
                "the drive budget for this root is spent",
            );
            continue;
        }
        println!("── PR {pr}");
        let state = match read_pr(a, &pr) {
            Ok(v) => v,
            Err(msg) => {
                eprintln!("pr-heal: {msg}");
                bump(&mut counts, "skip_read_error");
                receipt(&pr, "skip_read_error", first_line(&msg));
                worst = worse_of(worst, EXIT_READ_ERROR);
                continue;
            }
        };
        let (head, head_ref) = (state.head.clone(), state.head_ref.clone());
        // Refusal 1: a live worker owns this node; a healer pushing under it
        // is the two-writers failure. It runs FIRST: a rebase is a bigger
        // write than a fix push, so it never runs ahead of this check.
        if let Some((node, holder)) = claim_holder(&head_ref, claims_root) {
            println!(
                "skip claim_held: {node} is held by {holder}; \
                 the healer never pushes under a live worker"
            );
            bump(&mut counts, "skip_claim_held");
            receipt(&pr, "skip_claim_held", &format!("{node} held by {holder}"));
            continue;
        }
        // Rebase triggers, ahead of any classification: a push restarted CI,
        // so classifying the old sha's checks would classify a dead run. Two
        // bounded triggers -- a conflicting PR is stuck by definition, and the
        // merge slot names the one PR that goes next. No threshold, no new
        // config key: `mergeable_state` never reads `behind` on this repo, so
        // the slot's behind-drift verdict is the bound.
        let trigger = if state.mergeable == Some(false) {
            Some("conflicting".to_string())
        } else {
            match merge_slot_holder_pr(&state.base_ref, claims_root, &a.cwd) {
                Some(n) if n.to_string() == pr => Some("merge-slot".to_string()),
                _ => None,
            }
        };
        if let Some(trigger) = trigger {
            let Some((_, wt)) = worktrees.iter().find(|(b, _)| b == &head_ref) else {
                println!(
                    "skip no_worktree: no checkout on branch {head_ref}; \
                     the rebase runs from the PR's own worktree"
                );
                bump(&mut counts, "skip_no_worktree");
                receipt(&pr, "skip_no_worktree", "rebase trigger, no worktree");
                continue;
            };
            if rebased_this_run >= REBASE_BUDGET {
                println!("skip rebase_budget: {rebased_this_run} rebases already this run");
                bump(&mut counts, "skip_rebase_budget");
                receipt(
                    &pr,
                    "skip_rebase_budget",
                    "the per-run rebase budget is spent",
                );
                continue;
            }
            if dry_run {
                println!("would rebase: PR {pr} ({trigger})");
                bump(&mut counts, "would_rebase");
                continue;
            }
            rebased_this_run += 1;
            let bin = seam_bin(a, "fno");
            let push = run(&bin, &["do", "pr", "push"], wt, remedy_timeout(a));
            match push {
                Ok((true, out, _)) => {
                    let (before, after) = behind_numbers(&out);
                    println!("rebased: PR {pr} {trigger} behind {before} -> {after}");
                    bump(&mut counts, "rebased");
                    acted_prs.push(pr.clone());
                    receipt(
                        &pr,
                        "rebased",
                        &format!("{trigger}; behind {before} -> {after}"),
                    );
                    // The conflict cleared: the task heal filed for it closes.
                    if let Err(e) = crate::fleet_task::close(
                        &questions_store(a),
                        "heal",
                        &format!("PR {pr} rebase conflict"),
                        a.cwd.to_string_lossy().as_ref(),
                        "rebased",
                        "heal",
                    ) {
                        eprintln!("pr-heal: task close refused: {e}");
                    }
                    // The push restarted CI; classifying the old sha's checks
                    // would classify a dead run.
                    continue;
                }
                Ok((false, _, err)) if is_rebase_conflict(&err) => {
                    let files = conflict_files(&err);
                    println!(
                        "skip rebase_conflict: PR {pr} conflicts rebasing onto origin/main; the owner decides"
                    );
                    bump(&mut counts, "skip_rebase_conflict");
                    receipt(&pr, "skip_rebase_conflict", &files);
                    let key = format!("PR {pr} rebase conflict");
                    let text = if files.is_empty() {
                        format!(
                            "heal: {key} onto origin/main. Resolve with \
                             `fno do pr rebase {pr}` from the PR's worktree."
                        )
                    } else {
                        format!(
                            "heal: {key} onto origin/main in: {files}. Resolve with \
                             `fno do pr rebase {pr}` from the PR's worktree."
                        )
                    };
                    if file_task(a, &key, &text, &format!("fno do pr rebase {pr}"), &head_ref) {
                        println!("escalated: PR {pr} rebase conflict is now a fleet task");
                    }
                    continue;
                }
                other => {
                    let detail = other
                        .map(|(_, _, err)| first_line(&err).to_string())
                        .unwrap_or_else(|e| e);
                    println!(
                        "skip rebase_failed: the push verb refused ({detail}); \
                         falling through to the heal path"
                    );
                    bump(&mut counts, "skip_rebase_failed");
                    receipt(&pr, "skip_rebase_failed", &detail);
                    // A rebase that could not run is not a verdict about the
                    // PR: fall through to the normal heal path.
                }
            }
        }
        // A green rerun is a flake observation, not a silence. Runs before
        // the findings read: it fires even when nothing is red anymore.
        if !dry_run {
            detect_flakes(a, &head, &rerun_keys_ever, &flake_guards);
        }
        let findings = match findings_for(
            a,
            &pr,
            &head,
            // A failed shared read classifies with an empty set, same as the
            // per-PR read it replaces; the note above already named it.
            inherited_once.as_ref().or(Some(&Vec::new())),
        ) {
            Ok(f) => f,
            Err(msg) => {
                eprintln!("pr-heal: {msg}");
                bump(&mut counts, "skip_read_error");
                receipt(&pr, "skip_read_error", first_line(&msg));
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
            receipt(
                &pr,
                "skip_inherited",
                "nothing red here that main is not already red on",
            );
            continue;
        }
        // Refusal 2: an unknown signature is escalated, never guessed at --
        // AFTER its one rerun, never in the same breath as issuing it.
        for f in own.iter().filter(|f| f.signature == "unknown") {
            let key = run_id(&f.link).map(|r| format!("{head}:{r}"));
            unknown.push((pr.clone(), f.check.clone(), head_ref.clone(), key));
        }
        let healable = own.iter().any(|f| match &f.remedy {
            Remedy::Auto { .. }
            | Remedy::EditBody { .. }
            | Remedy::Rerun { .. }
            | Remedy::RerunFailed { .. } => true,
            // A rerunnable escalation (pytest, smoke, an unknown) reruns
            // before its repro is spent, when the run id is readable.
            Remedy::Escalate { .. } => rerunnable_class(f.signature) && run_id(&f.link).is_some(),
            Remedy::Inherited => false,
        });
        if !healable {
            // Known-but-escalate rows (pytest, mypy, a guard refusal) have a
            // playbook entry and a repro; the report is their lane.
            report(&findings, true, true);
            bump(&mut counts, "skip_escalate_only");
            receipt(
                &pr,
                "skip_escalate_only",
                own.first()
                    .map(|f| f.check.as_str())
                    .unwrap_or("unknown rows remain"),
            );
            worst = worse_of(worst, EXIT_ESCALATIONS);
            continue;
        }
        if dry_run {
            report(&findings, true, true);
            bump(&mut counts, "would_heal");
            // A rehearsal reports work the way the report-only --all does:
            // red PRs exist, so a preflight reading exit 0 must mean "nothing
            // red", never "the rehearsal ran".
            worst = worse_of(worst, EXIT_ESCALATIONS);
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
            receipt(
                &pr,
                "skip_no_worktree",
                "no checkout on the branch; heal it by hand from the PR's worktree",
            );
            worst = worse_of(worst, EXIT_ESCALATIONS);
            continue;
        };
        let sub = Args {
            cwd: wt.clone(),
            ..a.clone()
        };
        // Refusal 3 rides inside run_one: the pre-push re-read holds the
        // commit local over a run in flight, and it pushes exactly once.
        let (code, keys) = run_one(&sub, &pr);
        if !keys.is_empty() {
            bump(&mut counts, "rerun");
            reran_keys.extend(keys);
            acted_prs.push(pr.clone());
        }
        match code {
            EXIT_CLEAN => {
                bump(&mut counts, "healed");
                acted_prs.push(pr.clone());
                receipt(&pr, "healed", "the drive loop healed and pushed one fix");
            }
            EXIT_IN_FLIGHT => {
                bump(&mut counts, "skip_in_flight");
                receipt(
                    &pr,
                    "skip_in_flight",
                    "a run is in flight; commit kept local",
                );
            }
            EXIT_CWD_REFUSAL => {
                bump(&mut counts, "skip_dirty_tree");
                receipt(&pr, "skip_dirty_tree", "the PR's worktree refused an apply");
            }
            _ => {
                bump(&mut counts, "still_red");
                receipt(&pr, "escalated", "still red after the drive loop's pass");
            }
        }
        worst = worse_of(worst, code);
    }
    let unknown_n = unknown.len();
    if !dry_run {
        for (pr, check, head_ref, key) in &unknown {
            // A rerun issued THIS run already wrote its key to the journal
            // only after this point; a key present NOW is a PRIOR run's
            // rerun, so the second red is the verdict and the ask fires. An
            // unparseable link (key None) escalates as today.
            let already_reran = key
                .as_ref()
                .is_some_and(|k| journal_has_rerun(&journal_path(a), k));
            if already_reran || key.is_none() {
                if escalate_unknown_signature(a, pr, check, head_ref) {
                    println!("escalated: PR {pr} check {check} is now a fleet task");
                }
            }
        }
    }
    emit_tick_event(
        a,
        &counts,
        unknown_n,
        dry_run,
        &reran_keys,
        &acted_prs,
        t0.elapsed().as_secs_f64(),
    );
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

fn run_one(a: &Args, pr: &str) -> (i32, Vec<String>) {
    let state = match read_pr(a, pr) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("pr-heal: {msg}");
            let code = if msg.contains("No such file") || msg.contains("NotFound") {
                EXIT_NO_GH
            } else {
                EXIT_READ_ERROR
            };
            return (code, Vec::new());
        }
    };
    let (head, head_ref, body) = (state.head, state.head_ref, state.body);
    if a.apply {
        if let Some(why) = refuse_wrong_worktree(a, &head_ref) {
            eprintln!("pr-heal: refusing to apply: {why}");
            return (EXIT_CWD_REFUSAL, Vec::new());
        }
    }
    let mut findings = match findings_for(a, pr, &head, None) {
        Ok(f) => f,
        Err(msg) => {
            eprintln!("pr-heal: {msg}");
            return (EXIT_READ_ERROR, Vec::new());
        }
    };
    if !a.apply {
        return (report(&findings, true, a.all), Vec::new());
    }

    let reran_keys = apply_rerun(a, &mut findings, &head);
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
                        "the body edit failed ({msg}); add it by hand: {}",
                        edit_body_cmd(&nodes)
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
        return (code, reran_keys);
    }
    // Re-read BEFORE pushing through the shared guarded push. A push over a
    // run in flight cancels it, and that is the harm this verb exists to
    // stop repeating; an unreadable read holds the commit local.
    let ctx = crate::pr_push::PushCtx {
        git_bin: a.git_bin.clone(),
        gh_bin: a.gh_bin.clone(),
        fno_bin: String::new(),
        cwd: a.cwd.clone(),
        stamps_dir: crate::pr_push::default_stamps_dir(),
        force: false,
        // heal never rebases, so its push is always a plain fast-forward.
        lease: None,
    };
    match crate::pr_push::guarded_push(&ctx, &head) {
        crate::pr_push::PushOutcome::Pushed { .. } => {
            println!("pushed once");
            (code, reran_keys)
        }
        crate::pr_push::PushOutcome::InFlight { .. } => {
            println!(
                "run in flight; commit kept local, not pushing; \
                 rerun after fno do pr wait {pr}"
            );
            (EXIT_IN_FLIGHT, reran_keys)
        }
        crate::pr_push::PushOutcome::Unreadable(msg) => {
            println!("could not re-read checks ({msg}); commit kept local, not pushing");
            (EXIT_IN_FLIGHT, reran_keys)
        }
        crate::pr_push::PushOutcome::PushFailed(_) => {
            eprintln!("pr-heal: the fix is committed but the push failed");
            (EXIT_READ_ERROR, reran_keys)
        }
    }
}

#[cfg(test)]
mod tests {
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
    }

    #[test]
    fn one_heal_carries_every_id_on_the_one_verb_line() {
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
    }

    #[test]
    fn a_second_heal_over_its_own_output_writes_the_same_body() {
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
  *pulls/1*) echo '{"head":{"sha":"aaa1","ref":"feature/x-1111"},"body":"b"}'; exit 0 ;;
  *pulls/2*) echo '{"head":{"sha":"bbb2","ref":"feature/x-2222"},"body":"b"}'; exit 0 ;;
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
    fn head_ref_node_ids_follow_the_closure_producers_delimiter_rule() {
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
    }

    #[test]
    fn the_drive_loop_skips_a_claimed_pr_and_rehearses_the_free_one() {
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
        assert!(events.contains("\"would_heal\":1"), "{events}");
    }

    #[test]
    fn the_drive_loop_heals_the_free_pr_from_its_own_worktree() {
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
        let events = log_of(d, "events.jsonl");
        assert!(events.contains("\"healed\":1"), "{events}");
        assert!(events.contains("\"skip_claim_held\":1"), "{events}");
    }

    #[test]
    fn an_unknown_red_reruns_once_then_escalates_as_real() {
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
        let store_before = std::fs::read_to_string(d.join("questions.jsonl")).unwrap_or_default();
        assert!(
            !store_before.contains("fleet_task"),
            "no task before the rerun answers: {store_before}"
        );
        run_heal(&drive_args(d, &[]));
        let store = std::fs::read_to_string(d.join("questions.jsonl")).unwrap_or_default();
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
        let store = std::fs::read_to_string(d.join("questions.jsonl")).unwrap_or_default();
        assert_eq!(
            store.matches(r#""type":"fleet_task""#).count(),
            1,
            "the open task is never re-filed: {store}"
        );
        let events = log_of(d, "events.jsonl");
        assert!(events.contains("\"unknown\":1"), "{events}");
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
        stub_fno(d);
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

    // ── the rerun remedy ──────────────────────────────────────────────────

    #[test]
    fn a_cancelled_run_with_a_run_link_resolves_to_a_rerun() {
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
    }

    #[test]
    fn a_cancelled_check_without_a_run_link_still_escalates() {
        // A link that names no run (a StatusContext, or no link at all)
        // leaves nothing to rerun; the escalation is the honest answer.
        let f = classify(&cancelled_ctx("ci", ""), false);
        assert_eq!(f.signature, "cancelled");
        assert!(
            matches!(&f.remedy, Remedy::Escalate { repro } if repro.contains("cancelled")),
            "{:?}",
            f.remedy
        );
    }

    #[test]
    fn a_cancelled_run_is_rerun_once_per_sha() {
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
    }

    #[test]
    fn a_manual_single_pr_rerun_is_ledgered_for_the_once_per_sha_guard() {
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
    }

    #[test]
    fn a_second_cancelled_verdict_on_the_same_sha_escalates_instead_of_rerunning() {
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
  *pulls/1*) echo '{"head":{"sha":"aaa1","ref":"feature/x-1111"},"base":{"ref":"main"},"mergeable":MERGEABLE,"body":"b"}'; exit 0 ;;
  *pulls/2*) echo '{"head":{"sha":"bbb2","ref":"feature/x-2222"},"base":{"ref":"main"},"mergeable":null,"body":"b"}'; exit 0 ;;
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
    fn a_conflicting_pr_rebases_once_from_its_own_worktree() {
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
    }

    #[test]
    fn the_merge_slot_holder_is_rebased_when_mergeable_reads_null() {
        // `mergeable` reads null while GitHub computes; the merge slot is the
        // other trigger, and the ONLY reason this PR rebases. PR 2 conflicts
        // but holds no slot and has no worktree, so nothing else is rebased.
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        stub_gh_drive_rebase(d, "null");
        stub_git_drive(d);
        stub_cargo(d);
        std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
        stub_fno_push(d, "pr-push: origin/main behind-before=3 behind-after=0 preflight=skipped sha=abc pushed=1", 0, "");
        hold_slot_claim(d, 1);
        run_heal(&drive_args(d, &[]));
        let fno = log_of(d, "fno.log");
        assert_eq!(fno.matches("do pr push").count(), 1, "{fno}");
        let events = log_of(d, "events.jsonl");
        assert!(events.contains("\"rebased\":1"), "{events}");
        assert!(events.contains("\"skip_no_worktree\":1"), "{events}");
    }

    #[test]
    fn a_live_worker_claim_blocks_the_rebase_of_its_pr() {
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
    }

    #[test]
    fn the_rebase_budget_stops_the_run_after_six() {
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
  *pulls/*) echo '{"head":{"sha":"aaa1","ref":"feature/x-1111"},"base":{"ref":"main"},"mergeable":false,"body":"b"}'; exit 0 ;;
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
  *pulls/1*) echo '{"head":{"sha":"aaa1","ref":"pytest-leak-guard"},"base":{"ref":"main"},"mergeable":null,"body":"b"}'; exit 0 ;;
  *pulls/2*) echo '{"head":{"sha":"bbb2","ref":"feature/x-2222"},"base":{"ref":"main"},"mergeable":null,"body":"b"}'; exit 0 ;;
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
    fn a_rerunnable_red_reruns_once_per_sha_and_run() {
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
    }

    #[test]
    fn nine_checks_sharing_one_run_id_cost_one_rerun() {
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
    }

    #[test]
    fn a_second_red_on_the_rerun_key_escalates_as_real() {
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
  *pulls/1*) echo '{"head":{"sha":"aaa1","ref":"feature/x-1111"},"base":{"ref":"main"},"mergeable":null,"body":"b"}'; exit 0 ;;
  *pulls/2*) echo '{"head":{"sha":"bbb2","ref":"feature/x-2222"},"base":{"ref":"main"},"mergeable":null,"body":"b"}'; exit 0 ;;
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
            "{{\"ts\":\"2026-09-17T12:00:00Z\",\"type\":\"pr_heal_flake\",\"source\":\"heal\",\"data\":{{\"key_guard\":\"old:1:ci\",\"key\":\"ci\",\"sha\":\"old\",\"run_id\":\"{run}\",\"check\":\"ci\"{node_json}}}}}"
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
    fn a_green_rerun_records_a_flake_row() {
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
    }

    #[test]
    fn the_third_flake_occurrence_files_one_node() {
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
    }

    #[test]
    fn a_flake_key_with_a_node_never_refiles() {
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
    }

    #[test]
    fn the_playbook_names_rerunnability() {
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
    fn detach_spawns_a_child_names_it_in_a_pid_file_and_journals_the_spawn() {
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
    }

    #[test]
    fn detach_with_a_live_pid_skips_and_journals_in_flight() {
        // A drive loop still running when the next tick fires is skipped,
        // never double-spawned (the in-flight case the pid file exists for).
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let a = parse_args(&detach_args(d)).unwrap();
        std::fs::write(heal_pid_file(&a), format!("{}\n", std::process::id())).unwrap();
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
    }

    #[test]
    fn detach_under_an_armed_incident_spawns_nothing_and_journals_fleet_stop() {
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
    }

    #[test]
    fn detach_with_a_dead_pid_spawns_and_overwrites_the_pid_file() {
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
        std::fs::write(heal_pid_file(&a), format!("{dead}\n")).unwrap();
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
    }

    #[test]
    fn a_two_root_detach_spawns_one_child_carrying_every_root() {
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
            events_dir(&a).join("pr-heal.pid"),
            "one root-free pid file"
        );
        assert_eq!(
            read_pid_file(&heal_pid_file(&a)),
            Some(std::process::id()),
            "one pid file written"
        );
    }

    #[test]
    fn a_two_root_detach_with_a_live_pid_skips_in_flight() {
        // The in-flight guard reads the ONE pid file whatever root asked:
        // the per-root files never let it see a loop started for a root
        // other than its own.
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let other = tempfile::tempdir().unwrap();
        let other_s = other.path().to_str().unwrap();
        let argv = drive_args(d, &["--detach", "--cwd", other_s]);
        let a = parse_args(&argv).unwrap();
        std::fs::write(heal_pid_file(&a), format!("{}\n", std::process::id())).unwrap();
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
    }

    #[test]
    fn a_spent_deadline_skips_every_pr_with_a_receipt() {
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
    }

    #[test]
    fn run_roots_apply_visits_every_root_and_writes_its_row() {
        // One process, one pr_heal_tick row per root, folded to the worst
        // verdict. Instant stubs and a generous budget: no timing dependence
        // (the skip itself is covered by the spent-deadline tests).
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
        a.deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
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
    }

    #[test]
    fn remedy_timeout_never_outruns_the_deadline() {
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
    }

    #[test]
    fn a_single_root_tick_row_carries_the_root() {
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
    }

    #[test]
    fn status_line_unarmed_names_the_arm_command() {
        let a = parse_args(&["--status".to_string()]).unwrap();
        assert_eq!(
            status_line(&a),
            "Heal: unarmed (auto_heal.enabled=false; arm with: fno config set auto_heal.enabled true)"
        );
    }

    #[test]
    fn status_line_armed_without_a_row_says_never_ran() {
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
    }

    #[test]
    fn status_line_armed_with_a_recent_row_prints_counts_and_age() {
        let tmp = tempfile::tempdir().unwrap();
        let events = tmp.path().join("events.jsonl");
        let ts =
            (chrono::Utc::now() - chrono::Duration::minutes(12) - chrono::Duration::seconds(5))
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
    }

    #[test]
    fn status_line_prints_rebased_reran_and_the_acted_prs() {
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
    }

    #[test]
    fn status_line_prints_days_when_the_row_is_older_than_a_day() {
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
    }

    #[test]
    fn status_line_names_a_live_pid_file() {
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
        std::fs::write(heal_pid_file(&a), format!("{}\n", std::process::id())).unwrap();
        assert!(
            status_line(&a).contains(&format!("in-flight {}", std::process::id())),
            "{}",
            status_line(&a)
        );
    }

    #[test]
    fn the_status_line_labels_the_root_its_row_names() {
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
    fn a_stale_per_root_pid_file_with_a_live_pid_holds_the_tick() {
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
    }

    #[test]
    fn a_recycled_pid_that_names_no_healer_does_not_hold_the_tick() {
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
    }

    #[test]
    fn a_spent_budget_skips_every_root_without_paying_the_pre_loop_reads() {
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
    fn detach_without_the_drive_loop_is_a_usage_error() {
        assert_eq!(
            run_heal(&["--detach".to_string()]),
            EXIT_READ_ERROR,
            "--detach without --all --apply has nothing to detach"
        );
    }
}
