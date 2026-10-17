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

use crate::heal_sweep::{
    checks_digest, last_sweep_states, open_pr_numbers, open_pr_sweeps, PrSweepState,
};
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

/// The `GhProbe` seam over heal's own resolved gh binary, so heal's job-log
/// reads ride the pr_status cache every caller shares.
struct HealProbe<'a> {
    gh_bin: &'a str,
}

impl crate::pr_status_facts::GhProbe for HealProbe<'_> {
    fn run_gh(
        &self,
        cwd: &std::path::Path,
        args: &[String],
    ) -> Result<(bool, String, String), String> {
        let owned: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = crate::pr_push::run_labeled("pr-heal", self.gh_bin, &owned, cwd, READ_TIMEOUT)?;
        Ok(out)
    }
}

/// heal's job-log read through the shared cache: same `{owner}/{repo}`
/// placeholder path, but a log one status read already fetched costs no
/// second download. `slug_key` keys the row to the repo's origin slug. An
/// unresolvable slug falls back to the placeholder read exactly as before.
fn cached_job_log(a: &Args, slug_key: &str, job_id: &str) -> String {
    let (owner, repo) = match repo_slug_parts(slug_key) {
        Some(parts) => parts,
        None => {
            return gh_api(
                a,
                &format!("repos/{{owner}}/{{repo}}/actions/jobs/{job_id}/logs"),
                &[],
            )
            .unwrap_or_else(|e| format!("log unavailable: {e}"));
        }
    };
    let probe = HealProbe { gh_bin: &a.gh_bin };
    crate::pr_status::job_log(&probe, &a.cwd, slug_key, &owner, &repo, job_id)
        .unwrap_or_else(|e| format!("log unavailable: {e}"))
}

fn repo_slug_parts(slug_key: &str) -> Option<(String, String)> {
    if slug_key.is_empty() {
        return None;
    }
    let mut split = slug_key.splitn(2, "--");
    let owner = split.next()?.to_string();
    let repo = split.next()?.to_string();
    Some((owner, repo))
}

/// The repo's origin slug key (`owner--repo`), read once per invocation.
fn origin_slug_key(a: &Args) -> Option<String> {
    let url = crate::pr_push::run_labeled(
        "pr-heal",
        &a.git_bin,
        &["remote", "get-url", "origin"],
        &a.cwd,
        READ_TIMEOUT,
    )
    .ok()?
    .1
    .trim()
    .to_string();
    crate::merge_gates::repo_slug_from_origin(&url).map(|slug| slug.replace('/', "--"))
}

fn read_checks(a: &Args, head: &str) -> Result<Value, String> {
    crate::pr_push::read_checks(&a.gh_bin, &a.cwd, head)
}

fn gh_api_pages_conditional(a: &Args, path: &str) -> Result<Vec<Value>, String> {
    crate::pr_push::gh_api_pages_conditional(&a.gh_bin, &a.cwd, path)
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
    /// `gh run rerun <id>` (full rerun) for a cancelled-run remedy: it reached
    /// no verdict, so rerunning it IS reaching a verdict. `--failed` reruns
    /// only `failure` conclusions; a cancelled run has none. At most once per
    /// (head sha, run id); a second red verdict on the same pair escalates.
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
        // Measured: every `unknown` heal reported was a CANCELLED check whose
        // log carried one line (3 of 15 red open PRs, 2026-09-16). A cancelled
        // run concluded nothing, so a rerun is how it reaches a verdict. No
        // run id in the link: nothing to rerun, so the row escalates.
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
        plan: "edit-body: write ONE Fixes line naming every node the branch names",
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
        // The remedy mirrors the GATE, not ruff's fuller surface: `uv run
        // ruff check --no-respect-gitignore src/`, no `ruff format`, never
        // `tests/`. Wider rewrites files the gate does not read -- `ruff
        // format` over this tree touches more than a thousand.
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
    /// What one pre-loop read (the `gh` listing, the worktree scan) may
    /// block. The root gate in [`run_roots_apply`] never starts a root
    /// whose remaining slice cannot fit one read, so the listing -- the
    /// one network read, and the one that overran in the field -- ends by
    /// the deadline. The later local reads can still overrun once; a root
    /// that does leaves no tick row and leads the next tick. Parsed as
    /// [`crate::pr_push::READ_TIMEOUT`]; tests shrink it so an overrun is
    /// provable in seconds, never minutes.
    read_bound: std::time::Duration,
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
        read_bound: crate::pr_push::READ_TIMEOUT,
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
    /// Some(why) when the PR is from outside; the run skips, fail
    /// closed. An unreadable origin read lands here too.
    outside: Option<String>,
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
    let outside = match crate::pr_admission::outside_reason(&v) {
        Ok(inside) => inside,
        Err(msg) => Some(msg),
    };
    if head.is_empty() {
        return Err("pr json carried no head sha".to_string());
    }
    Ok(PrState {
        head,
        head_ref,
        body,
        base_ref,
        mergeable,
        outside,
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
    pre_read: Option<Value>,
) -> Result<Vec<Finding>, String> {
    let checks = match pre_read {
        Some(v) => v,
        None => read_checks(a, head)?,
    };
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
    let slug_key = origin_slug_key(a).unwrap_or_default();
    for row in failing_rows(&checks) {
        let check = row["name"].as_str().unwrap_or("").to_string();
        let log = if let Some(timeout) = row.get("timeout").and_then(|v| v.as_str()) {
            timeout.to_string()
        } else {
            match job_id(row["link"].as_str().unwrap_or("")) {
                // A log the API cannot serve (expired retention, a commit
                // status with no job) is REPORTED as unreadable, never
                // dropped: a check heal cannot read is still red.
                Some(id) => cached_job_log(a, &slug_key, &id),
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

/// The ids one closure line claims: the keyword and its optional colon
/// stripped, tokens split on whitespace and comma, malformed tokens making
/// the line prose. The grammar `pr_closure::parse` reads.
fn closure_line_ids(line: &str) -> Vec<String> {
    crate::org_board::pr_closure::line_ids(line)
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
    for id in body.lines().flat_map(closure_line_ids) {
        if !union.contains(&id) {
            union.push(id);
        }
    }
    union
}

/// The body with every CLAIMING closure line removed - the base the one new
/// line is appended to, so no stale line can outrank it. A keyword-led line
/// whose tokens void it ("Fixes the thing.") is prose: the sweep leaves it.
fn body_without_trailer_lines(body: &str) -> String {
    body.lines()
        .filter(|line| closure_line_ids(line).is_empty())
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

/// Edit the PR body so exactly ONE closure line names every node the
/// heal covers plus every id the body's existing closure lines held. No
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
        let code = if a.roots.len() > 1 {
            run_roots_apply(&a, a.dry_run)
        } else {
            run_all_apply(&a, a.dry_run)
        };
        // The writer deletes its own pid file: a clean loop leaves no
        // top-level litter behind (crashed loops are covered by the sweep).
        let pid_file = heal_pid_file(&a);
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            if text.trim() == std::process::id().to_string() {
                let _ = std::fs::remove_file(&pid_file);
            }
        }
        return code;
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
            &std::collections::BTreeMap::new(),
            0.0,
        );
    }
    code
}

/// One heal per red open PR, report-only. Uses the REST listing for the same
/// reason [`read_checks`] does: `gh pr list` is GraphQL and gets routed away.
fn run_all(a: &Args) -> i32 {
    let pages =
        match gh_api_pages_conditional(a, "repos/{owner}/{repo}/pulls?state=open&per_page=100") {
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

// ── the drive loop (--all --apply) ───────────────────────────────────────────

/// Node ids a PR head ref names, as delimiter-bounded segments. Re-exported
/// from the shared `org_board::prs` predicate, so the heal drive loop, the
/// board, and the merge owner all read one rule.
pub(crate) use crate::org_board::prs::branch_node_ids;

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

/// The push verb's conflict exit: the integrate leg refused because the branch
/// is not safely rebasable (needs_resolver, refused, or a merge the verb
/// aborted). The verb's other exit-3 refusals (protected branch, dirty tree) are NOT conflicts.
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
    let fields = crate::tick_ledger::tick_data(
        "heal",
        crate::tick_ledger::SCHED_LAUNCHD,
        acted as u64,
        skip_reason,
        if detail.is_empty() {
            None
        } else {
            Some(detail)
        },
        600,
    );
    if let Err(e) = crate::events::EventEmitter::new(journal_path(a), "pr-heal").emit_fields(
        "control_plane_tick",
        fields.as_object().cloned().unwrap_or_default(),
    ) {
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

/// The pid file for THE drive loop; the pid-file family lives in `heal_pid`.
fn heal_pid_file(a: &Args) -> std::path::PathBuf {
    crate::heal_pid::pid_file(&events_dir(a))
}

// Test-only reader since the guard switched to live pids.
#[cfg_attr(not(test), allow(dead_code))]
fn read_pid_file(path: &std::path::Path) -> Option<u32> {
    crate::heal_pid::read_pid_file(path)
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

/// Live pids across the state root's `heal/` subfolder and the legacy
/// top-level location; the family lives in `heal_pid`.
fn live_heal_pids(dir: &std::path::Path) -> Vec<u32> {
    crate::heal_pid::live_pids(dir)
}

/// The one `Heal:` readout line. `--status` prints it; `_install.py` shells
/// this verb rather than re-reading the journal in Python.
/// The one `Heal:` readout for `pr_watch::status`: the arm bit and the
/// journal are the only inputs `status_line` reads.
pub(crate) fn status_readout(armed: bool, events_file: &std::path::Path) -> String {
    let a = Args {
        pr: None,
        apply: false,
        all: false,
        playbook: false,
        dry_run: false,
        detach: false,
        status: false,
        armed,
        gh_bin: "gh".to_string(),
        git_bin: "git".to_string(),
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        roots: Vec::new(),
        deadline: None,
        read_bound: crate::pr_push::READ_TIMEOUT,
        bin_dir: String::new(),
        claims_root: String::new(),
        events_file: events_file.display().to_string(),
    };
    status_line(&a)
}

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

/// PRs whose conflicting-trigger refresh already concluded in the last 24
/// hours, out of the `pr_heal_pr` journal: a real push (`rebased`) or a
/// push that moved nothing (`skip_rebase_noop`). A still-conflicting
/// rebase is deliberately absent: the owner may resolve and need the
/// retry, so it spends the per-run budget instead. Rows filter on `root`,
/// so one checkout's PR 12 never caps another's, and on the 24-hour
/// window. The drive loop's one store read per run; the conflicting
/// trigger's per-PR cap filters it per PR.
fn journal_refreshed_prs(path: &std::path::Path, root: &str) -> std::collections::HashSet<String> {
    let _ = crate::event_store::import_all(path);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default();
    let Ok(rows) = crate::event_store::query_events(
        path,
        &crate::event_store::EventQuery {
            types: vec!["pr_heal_pr".to_string()],
            since_ms: Some(now_ms - 24 * 60 * 60 * 1000),
            ..Default::default()
        },
    ) else {
        return Default::default();
    };
    rows.iter()
        .filter_map(|r| serde_json::from_str::<Value>(&r.line).ok())
        .filter(|row| row.pointer("/data/root").and_then(Value::as_str) == Some(root))
        .filter(|row| {
            matches!(
                row.pointer("/data/action").and_then(Value::as_str),
                Some("rebased") | Some("skip_rebase_noop")
            )
        })
        .filter_map(|row| {
            row.pointer("/data/pr")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
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
    let slug_key = origin_slug_key(a).unwrap_or_default();
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
                Some(id) => strip_timestamps(&cached_job_log(a, &slug_key, &id)),
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
    pr_states: &std::collections::BTreeMap<String, PrSweepState>,
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
    // probe asserts on even when a run acted on nothing. The single-PR
    // apply path calls with an empty counts map, so the required trio is
    // defaulted here rather than trusted from the caller.
    fields.insert(
        "seen".to_string(),
        serde_json::json!(counts.get("seen").copied().unwrap_or(0)),
    );
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
    // The next sweep's skip baseline; omitted when empty so a row that
    // records no world never displaces the last real one.
    if !pr_states.is_empty() {
        fields.insert("pr_states".to_string(), serde_json::json!(pr_states));
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

/// The roots in heal order: least-recently-healed first. The journal's last
/// `pr_heal_tick` row per root is the record (rows land in run order), and a
/// root the journal never names -- skipped last tick, or new to `--cwd` --
/// sorts before every healed root, so a root one tick starved leads the
/// next. Ties keep the `--cwd` order (the sort is stable). A skipped root
/// writes no row, which is exactly what makes the journal the rotation
/// state: no second record to keep in step.
fn roots_in_heal_order(a: &Args) -> Vec<std::path::PathBuf> {
    if a.roots.len() < 2 {
        return a.roots.clone();
    }
    let path = journal_path(a);
    let _ = crate::event_store::import_all(&path);
    let rows = crate::event_store::query_events(
        &path,
        &crate::event_store::EventQuery {
            types: vec!["pr_heal_tick".to_string()],
            ..Default::default()
        },
    )
    .unwrap_or_default();
    let mut last_seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        if let Some(root) = serde_json::from_str::<Value>(&row.line)
            .ok()
            .and_then(|line| {
                line.get("data")
                    .and_then(|d| d.get("root"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
        {
            last_seen.insert(root, i);
        }
    }
    let mut order = a.roots.clone();
    // Never-healed ranks 0, healed ranks above every never-healed root by
    // construction (row index + 1); the stable sort keeps `--cwd` order
    // inside each rank.
    order.sort_by_key(|root| {
        last_seen
            .get(root.to_string_lossy().as_ref())
            .map(|i| i + 1)
            .unwrap_or(0)
    });
    order
}

/// The multi-root drive loop: one process heals every `--cwd` root, so the
/// tick pays one spawn and one pid file. Roots run least-recently-healed
/// first ([`roots_in_heal_order`]). Each root runs with an even share of
/// what is left of [`DRIVE_BUDGET`]; a root that spends less passes its
/// unspent time on. The gate is admission, not preemption: a root whose
/// remaining slice cannot fit one pre-loop read (`read_bound`) never
/// starts, so the listing it would have paid cannot eat the roots behind
/// it. The later local reads (worktree scan, main HEAD) can still overrun
/// once past a spent slice; the root that does leaves no tick row, and the
/// journal order hands it the first slot next tick. Each root's
/// `run_all_apply` writes its own `pr_heal_tick` row, and `root` on the
/// row names whose.
fn run_roots_apply(a: &Args, dry_run: bool) -> i32 {
    // Scope the sweep to roots that are real local worktrees: a sidecar's
    // cwd with no clone here (a project this machine has never checked
    // out) still arrives as a `--cwd`, and driving it pays a full
    // open-pulls read for nothing to heal.
    let mut a = a.clone();
    for root in &a.roots {
        if !root.join(".git").exists() {
            eprintln!("pr-heal: skipping {} (no local worktree)", root.display());
        }
    }
    a.roots.retain(|root| root.join(".git").exists());
    let a = &a;
    let deadline = a
        .deadline
        .unwrap_or_else(|| std::time::Instant::now() + DRIVE_BUDGET);
    let mut worst = EXIT_CLEAN;
    let mut roots_left = a.roots.len();
    for root in roots_in_heal_order(a) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining < a.read_bound {
            // Out of budget, or too little left to run even the listing
            // read: the root's PRs wait for the next tick, and it pays no
            // pre-loop read that would overrun into the next root's share.
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
    let pages =
        match gh_api_pages_conditional(a, "repos/{owner}/{repo}/pulls?state=open&per_page=100") {
            Ok(p) => p,
            Err(msg) => {
                eprintln!("pr-heal: {msg}");
                return EXIT_READ_ERROR;
            }
        };
    let worktrees = worktrees_by_branch(&a.git_bin, &a.cwd);
    // One main-HEAD read per invocation, shared by every PR that reaches
    // classification -- computed on first need, so an all-unchanged sweep
    // (the idle-sweep case this skip exists for) never pays it.
    let mut inherited_cache: Option<Option<Vec<String>>> = None;
    let mut inherited_note = false;
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
        // The journal is machine-global: `root` scopes the per-PR cap to
        // this checkout, the same field the tick row writes.
        fields.insert(
            "root".to_string(),
            serde_json::json!(a.cwd.to_string_lossy()),
        );
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
    // ever reran, every (sha, run id, check) already recorded as a flake, and
    // the last sweep's per-PR world the skip gate compares against.
    let rerun_keys_ever = journal_rerun_keys(&journal_path(a));
    let flake_guards = journal_flake_guards(&journal_path(a));
    let prior_states = last_sweep_states(&journal_path(a), &a.cwd.to_string_lossy().as_ref());
    let refreshed_prs_24h =
        journal_refreshed_prs(&journal_path(a), a.cwd.to_string_lossy().as_ref());
    let mut sweep_states: std::collections::BTreeMap<String, PrSweepState> =
        std::collections::BTreeMap::new();
    let mut worst = EXIT_CLEAN;
    // The root's slice of the drive budget: past it, every PR not yet
    // reached gets one receipt and waits for the next tick.
    let deadline = a
        .deadline
        .unwrap_or_else(|| std::time::Instant::now() + DRIVE_BUDGET);
    for (pr, listing_head, listing_row) in open_pr_sweeps(&pages) {
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
        // The unchanged-PR skip: one checks read keyed on the listing's free
        // head sha; when sha AND digest both match the last sweep's record,
        // the pulls read, the claim gate, the job logs and the flake scan
        // are repeated work. Doors that stay open: a live rebase trigger, a
        // head with a rerun in its history (the rerun ladder's second
        // verdict needs the re-classification), and any dry run, whose
        // exit-0 contract means nothing red.
        let mut digest_read = listing_head.as_deref().and_then(|h| read_checks(a, h).ok());
        let digest = digest_read.as_ref().and_then(checks_digest);
        if let (Some(h), Some(dg)) = (&listing_head, &digest) {
            sweep_states.insert(
                pr.clone(),
                PrSweepState {
                    head: h.clone(),
                    digest: dg.clone(),
                },
            );
        }
        let unchanged = match (
            listing_head.as_deref(),
            digest.as_deref(),
            prior_states.get(&pr),
        ) {
            (Some(h), Some(dg), Some(prior)) => {
                prior.head == h
                    && prior.digest == dg
                    && listing_row.get("mergeable").and_then(Value::as_bool) != Some(false)
                    && merge_slot_holder_pr(
                        listing_row
                            .pointer("/base/ref")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        claims_root,
                        &a.cwd,
                    )
                    .is_none()
                    && !rerun_keys_ever
                        .iter()
                        .any(|k| k.as_str().starts_with(format!("{h}:").as_str()))
            }
            _ => false,
        };
        if unchanged && !dry_run {
            println!("skip unchanged: head sha and checks digest match the last sweep");
            bump(&mut counts, "skip_unchanged");
            receipt(
                &pr,
                "skip_unchanged",
                "head sha and checks digest match the last sweep",
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
        if let Some(why) = &state.outside {
            bump(&mut counts, "skip_outside");
            receipt(&pr, "skip_outside", why);
            continue;
        }
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
        // The merge-slot read rides along here even for a conflicting PR:
        // the 24h cap exempts the slot holder (it is next to merge), so
        // both triggers need the answer.
        let slot_holds_this_pr = merge_slot_holder_pr(&state.base_ref, claims_root, &a.cwd)
            .is_some_and(|n| n.to_string() == pr);
        let trigger = if state.mergeable == Some(false) {
            Some("conflicting".to_string())
        } else if slot_holds_this_pr {
            Some("merge-slot".to_string())
        } else {
            None
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
            // The 24h cap bounds the conflicting trigger only: the slot
            // holder is next to merge, so its refresh never ages out.
            if trigger == "conflicting" && !slot_holds_this_pr && refreshed_prs_24h.contains(&pr) {
                println!(
                    "skip rebase_cap: PR {pr} was rebased within the last 24h; the owner resolves it"
                );
                bump(&mut counts, "skip_rebase_cap");
                receipt(
                    &pr,
                    "skip_rebase_cap",
                    "one conflicting refresh per PR per 24h; the owner resolves it",
                );
                continue;
            }
            if dry_run {
                println!("would rebase: PR {pr} ({trigger})");
                bump(&mut counts, "would_rebase");
                continue;
            }
            let bin = seam_bin(a, "fno");
            let push = run(&bin, &["do", "pr", "push"], wt, remedy_timeout(a));
            match push {
                // A push that moved nothing restarted no CI: the current
                // sha's checks stay live, so classification reads them
                // below. The per-run budget is unspent, no acted row is
                // written, and the conflict task stays open.
                Ok((true, out, _)) if out.contains("pushed=0") => {
                    println!("skip rebase_noop: PR {pr} {trigger}; nothing to push");
                    bump(&mut counts, "skip_rebase_noop");
                    receipt(
                        &pr,
                        "skip_rebase_noop",
                        &format!("{trigger}; nothing to push"),
                    );
                    // No new sha: the checks classified below are live.
                }
                Ok((true, out, _)) => {
                    rebased_this_run += 1;
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
                    // A conflict spends the per-run budget, never the 24h
                    // cap: the owner may resolve and need the retry this
                    // tick, and the journal cannot tell a resolved tree
                    // from a stuck one.
                    rebased_this_run += 1;
                    let files = conflict_files(&err);
                    println!(
                        "skip rebase_conflict: PR {pr} conflicts rebasing onto origin/main; the owner decides"
                    );
                    bump(&mut counts, "skip_rebase_conflict");
                    receipt(&pr, "skip_rebase_conflict", &files);
                    let key = format!("PR {pr} rebase conflict");
                    let at = (!files.is_empty())
                        .then(|| format!(" in: {files}"))
                        .unwrap_or_default();
                    // The verb's own door rides in its stderr; quoting it keeps the question on the integrate leg.
                    let seg = err.split("). ").nth(1);
                    let door = seg.unwrap_or("resolve by hand, then re-run `fno do pr push`");
                    let text = format!("heal: {key} onto origin/main{at}. {door}");
                    let run = if err.contains("already merges origin/main") {
                        format!("fno do pr push --pr {pr}")
                    } else {
                        format!("fno do pr rebase {pr}")
                    };
                    if file_task(a, &key, &text, &run, &head_ref) {
                        println!("escalated: PR {pr} rebase conflict is now a fleet task");
                    }
                    continue;
                }
                other => {
                    rebased_this_run += 1;
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
        // The skip's own read rides along when the live head matches the
        // digest's sha; findings_for reuses it instead of a second read.
        let pre_read = if listing_head.as_deref() == Some(state.head.as_str()) {
            digest_read.take()
        } else {
            None
        };
        // A failed shared read classifies with an empty set, same as the
        // per-PR read it replaces; the note (once per run) names it.
        let cached = inherited_cache
            .get_or_insert_with(|| crate::loopcheck::main_head_failing_checks(&a.gh_bin, &a.cwd));
        if cached.is_none() && !inherited_note {
            println!(
                "note: could not read main's HEAD, so no row can be shown as inherited; \
                 a failure below may be main's rather than this PR's"
            );
            inherited_note = true;
        }
        let empty_inherited: Vec<String> = Vec::new();
        let findings = match findings_for(
            a,
            &pr,
            &head,
            Some(cached.as_ref().unwrap_or(&empty_inherited)),
            pre_read,
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
    let fno_bin = seam_bin(a, "fno");
    let steward_io = crate::pr_steward::RealIo {
        gh_bin: a.gh_bin.as_str(),
        fno_bin: &fno_bin,
        cwd: std::path::Path::new(&a.cwd),
    };
    // gh_api_pages hands back pages of pull rows; the steward walks rows.
    let pulls = crate::heal_sweep::pull_rows(&pages);
    bump(
        &mut counts,
        crate::pr_steward::run_pass(&steward_io, steward_io.cwd, &pulls, dry_run),
    );
    emit_tick_event(
        a,
        &counts,
        unknown_n,
        dry_run,
        &reran_keys,
        &acted_prs,
        &sweep_states,
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
    if let Some(why) = &state.outside {
        println!("skip outside: {why}");
        return (EXIT_CLEAN, Vec::new());
    }
    let (head, head_ref, body) = (state.head, state.head_ref, state.body);
    if a.apply {
        if let Some(why) = refuse_wrong_worktree(a, &head_ref) {
            eprintln!("pr-heal: refusing to apply: {why}");
            return (EXIT_CWD_REFUSAL, Vec::new());
        }
    }
    let mut findings = match findings_for(a, pr, &head, None, None) {
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
mod tests;
