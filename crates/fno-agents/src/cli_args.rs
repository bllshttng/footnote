//! Typed argv for the migrated `fno-agents` verbs (x-861c).
//!
//! One derive declaration per flag: for the verbs it owns, this module IS the
//! flag registry, and its help strings are the only copy. Parsers run through
//! `try_parse_from`, so the caller keeps process ownership (clap never exits
//! inside library code) and maps failures through [`refusal_line`] into the
//! Footnote contract: one command-qualified line on stderr, exit 2.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// The shared machine-output group (x-81b6 contracts build on this): one
/// declaration of `--json`/`-J`, flattened into every verb that emits it.
#[derive(Args, Debug)]
pub struct JsonOnly {
    /// Emit machine-readable JSON on stdout instead of human text
    #[arg(short = 'J', long)]
    pub json: bool,
}

/// `fno-agents restart`: swap a (possibly stale) daemon for the current
/// binary. `--json` is the machine surface the Python adapter invokes with
/// (x-67b8): text summaries stay on stdout, one JSON object replaces them.
#[derive(Parser, Debug)]
#[command(name = "fno-agents restart", no_binary_name = true)]
pub struct RestartArgs {
    /// Break-glass: SIGKILL the lockfile holder before any probe; plain restart drains gracefully
    #[arg(long)]
    pub force: bool,
    #[command(flatten)]
    pub json: JsonOnly,
}

/// `fno-agents scratch`: evals-loop scratch surgery over the jobs journal.
#[derive(Parser, Debug)]
#[command(name = "fno-agents scratch", no_binary_name = true)]
pub struct ScratchArgs {
    #[command(subcommand)]
    pub cmd: ScratchCmd,
}

#[derive(Subcommand, Debug)]
pub enum ScratchCmd {
    /// Rank stale scratch work and execute (or dry-run) the node moves
    Sweep(SweepArgs),
    /// Render the ranked human table from the journal (read-only)
    Report(ReportArgs),
}

/// Sweep options. An absent option keeps its configuration/default value; a
/// PRESENT option with a missing or malformed value is a refusal, never a
/// silent default.
#[derive(Args, Debug)]
pub struct SweepArgs {
    /// Sweep window in days (overrides the evals config block)
    #[arg(long, value_name = "N")]
    pub since_days: Option<i64>,
    /// Minimum divergence score a finding needs to surface (overrides the evals config block)
    #[arg(long, value_name = "N")]
    pub threshold: Option<usize>,
    /// Jobs directory to sweep (default: the state root's jobs dir)
    #[arg(long, value_name = "PATH")]
    pub jobs_dir: Option<PathBuf>,
    /// Print the planned node moves without executing them
    #[arg(long)]
    pub dry_run: bool,
    #[command(flatten)]
    pub json: JsonOnly,
}

/// Report options (read-only; narrower than sweep on purpose).
#[derive(Args, Debug)]
pub struct ReportArgs {
    /// Ranking window in days
    #[arg(long, value_name = "N")]
    pub since_days: Option<i64>,
    #[command(flatten)]
    pub json: JsonOnly,
}

/// `fno-agents review-summary`: the reviewed-at display line's inputs. All
/// three are required; the caller turns a parse failure into the verb's
/// deliberate silence (print nothing, exit 0), never into a claim.
#[derive(Parser, Debug)]
#[command(name = "fno-agents review-summary", no_binary_name = true)]
pub struct ReviewSummaryArgs {
    /// Path to the .fno/events.jsonl ledger to read
    #[arg(long, value_name = "PATH")]
    pub events: PathBuf,
    /// Branch name the attestation must match
    #[arg(long, value_name = "BRANCH")]
    pub branch: String,
    /// Head sha (prefix ok, min 7) the attestation must be pinned to
    #[arg(long, value_name = "SHA")]
    pub head: String,
}

/// One command-qualified refusal line for a parse failure. The caller prints
/// it to stderr and exits 2; clap's own multi-line usage block never reaches
/// the operator.
pub fn refusal_line(cmd: &str, err: &clap::Error) -> String {
    let rendered = err.render().to_string();
    let first = rendered
        .lines()
        .next()
        .unwrap_or("invalid arguments")
        .trim()
        .trim_start_matches("error: ")
        .trim();
    format!("{cmd}: {first}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// AC1-REGISTRY: every canonical long flag on `path`'s command carries a
    /// nonblank help string, and the command's long names are unique. A
    /// failure names the command path and the offending flag.
    fn assert_flag_registry(cmd: &clap::Command, path: &str) {
        let mut seen = std::collections::BTreeSet::new();
        for arg in cmd.get_arguments() {
            if let Some(long) = arg.get_long() {
                assert!(
                    seen.insert(long.to_string()),
                    "{path} declares --{long} twice"
                );
                if arg.is_hide_set() {
                    continue; // hidden compatibility spellings own no help copy
                }
                let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
                assert!(
                    !help.trim().is_empty(),
                    "{path} --{long}: empty help; the registry help is the only copy"
                );
            }
        }
    }

    #[test]
    fn registry_flags_have_help_and_no_duplicates() {
        assert_flag_registry(&RestartArgs::command(), "fno-agents restart");
        assert_flag_registry(&ScratchArgs::command(), "fno-agents scratch");
        assert_flag_registry(&ReviewSummaryArgs::command(), "fno-agents review-summary");
        for sub in ScratchArgs::command().get_subcommands() {
            assert_flag_registry(sub, &format!("fno-agents scratch {}", sub.get_name()));
        }
    }

    #[test]
    fn registry_covers_every_migrated_verb() {
        // The registry test is the inventory: a migrated verb joins here or
        // the migration is not done.
        for sub in ScratchArgs::command().get_subcommands() {
            assert!(
                sub.get_about()
                    .map(|a| !a.to_string().trim().is_empty())
                    .unwrap_or(false),
                "fno-agents scratch {}: empty about",
                sub.get_name()
            );
        }
    }

    #[test]
    fn restart_parses_the_python_adapter_invocation() {
        // x-67b8: this exact argv (post-verb) is what cli/src/fno/restart.py
        // spawns; the dispatch strips the verb before the parser runs.
        let a = RestartArgs::try_parse_from(["--json"]).expect("--json parses");
        assert!(a.json.json);
        assert!(!a.force);
        let a = RestartArgs::try_parse_from(["--force"]).expect("--force parses");
        assert!(a.force);
        assert!(!a.json.json);
        let a = RestartArgs::try_parse_from(["-J"]).expect("-J is the json alias");
        assert!(a.json.json);
    }

    #[test]
    fn scratch_accepts_separated_and_equals_forms() {
        let a = ScratchArgs::try_parse_from([
            "sweep",
            "--since-days",
            "14",
            "--threshold=7",
            "--jobs-dir",
            "/tmp/j",
            "--dry-run",
            "--json",
        ])
        .expect("separated + equals forms parse");
        match a.cmd {
            ScratchCmd::Sweep(s) => {
                assert_eq!(s.since_days, Some(14));
                assert_eq!(s.threshold, Some(7));
                assert_eq!(s.jobs_dir, Some(PathBuf::from("/tmp/j")));
                assert!(s.dry_run && s.json.json);
            }
            other => panic!("sweep expected, got {other:?}"),
        }
    }

    #[test]
    fn scratch_malformed_numeric_is_a_refusal_not_a_default() {
        let err = ScratchArgs::try_parse_from(["sweep", "--threshold", "abc"])
            .expect_err("malformed numeric refuses");
        let line = refusal_line("fno-agents scratch sweep", &err);
        assert!(
            line.starts_with("fno-agents scratch sweep: "),
            "one command-qualified line: {line}"
        );
        assert!(line.contains("--threshold"), "names the flag: {line}");
        let err = ScratchArgs::try_parse_from(["sweep", "--since-days"])
            .expect_err("missing value refuses");
        assert!(refusal_line("fno-agents scratch sweep", &err).contains("--since-days"));
    }

    #[test]
    fn scratch_unknown_flag_refuses() {
        let err =
            ScratchArgs::try_parse_from(["report", "--dry-run"]).expect_err("report is narrow");
        assert!(refusal_line("fno-agents scratch report", &err).contains("--dry-run"));
    }

    #[test]
    fn review_summary_requires_all_three() {
        let a = ReviewSummaryArgs::try_parse_from([
            "--events=e.jsonl",
            "--branch",
            "feature/x",
            "--head",
            "abc1234",
        ])
        .expect("all three parse, equals form included");
        assert_eq!(a.events, PathBuf::from("e.jsonl"));
        assert_eq!(a.branch, "feature/x");
        assert_eq!(a.head, "abc1234");
        for partial in [
            vec![],
            vec!["--events", "e.jsonl"],
            vec!["--events", "e.jsonl", "--branch", "b"],
        ] {
            let argv: Vec<&str> = partial;
            assert!(
                ReviewSummaryArgs::try_parse_from(argv).is_err(),
                "a partial triplet is a parse failure (the caller stays silent)"
            );
        }
    }

    #[test]
    fn duplicate_scalar_flag_refuses() {
        let err = RestartArgs::try_parse_from(["--force", "--force"])
            .expect_err("duplicate scalar refuses");
        assert!(refusal_line("fno-agents restart", &err).contains("--force"));
    }
}
