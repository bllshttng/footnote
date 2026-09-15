//! Typed argv for the migrated `fno-agents` verbs.
//!
//! One derive declaration per flag: for the verbs it owns, this module IS the
//! flag registry, and its help strings are the only copy. Parsers run through
//! `try_parse_from`, so the caller keeps process ownership (clap never exits
//! inside library code) and maps failures through [`refusal_line`] into the
//! Footnote contract: one command-qualified line on stderr, exit 2.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// The shared machine-output group (contracts build on this): one
/// declaration of `--json`/`-J`, flattened into every verb that emits it.
#[derive(Args, Debug)]
pub struct JsonOnly {
    /// Emit machine-readable JSON on stdout instead of human text
    #[arg(short = 'J', long)]
    pub json: bool,
}

/// `fno-agents restart`: swap a (possibly stale) daemon for the current
/// binary. `--json` is the machine surface the Python adapter invokes with
/// text summaries stay on stdout, one JSON object replaces them.
#[derive(Parser, Debug)]
#[command(name = "fno-agents restart", no_binary_name = true)]
pub struct RestartArgs {
    /// Break-glass: SIGKILL the lockfile holder before any probe; plain restart drains gracefully
    #[arg(long)]
    pub force: bool,
    /// Chain-only: swap only when the running daemon measures drifted; quiet exit 0 on fresh, down, or unknown
    #[arg(long)]
    pub if_drifted: bool,
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

/// `fno-agents review-summary`: the reviewed-at display line's inputs.
/// Branch and head are required; the caller turns a parse failure into the
/// verb's deliberate silence (print nothing, exit 0), never into a claim.
#[derive(Parser, Debug)]
#[command(name = "fno-agents review-summary", no_binary_name = true)]
pub struct ReviewSummaryArgs {
    /// Path to the .fno/events.jsonl ledger to read
    #[arg(long, value_name = "PATH")]
    pub events: Option<PathBuf>,
    /// Branch name the attestation must match
    #[arg(long, value_name = "BRANCH")]
    pub branch: String,
    /// Head sha (prefix ok, min 7) the attestation must be pinned to
    #[arg(long, value_name = "SHA")]
    pub head: String,
}

/// The spawn head's axis flags, parsed once and consumed by both the client's
/// spawn dispatch and the spawn-overlay inspection verb: route,
/// provider, harness, model, effort, account, substrate. Tokens before the
/// provider argv fence that are not axis flags belong to the rest of the
/// spawn grammar and are ignored here; tokens after the fence are opaque
/// provider payload and never parsed.
#[derive(Parser, Debug, Default, PartialEq, Eq)]
#[command(
    name = "fno-agents spawn axes",
    no_binary_name = true,
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct SpawnAxes {
    /// Vendor/model route (vendor/model); the vendor pin outranks --provider
    #[arg(long)]
    pub route: Option<String>,
    /// Model vendor (routing is applied by the fno CLI seam)
    #[arg(short = 'P', long)]
    pub provider: Option<String>,
    /// CLI binary axis
    #[arg(short = 'H', long)]
    pub harness: Option<String>,
    /// Model name handed to the provider CLI's own --model
    #[arg(short = 'm', long)]
    pub model: Option<String>,
    /// Reasoning effort for the spawned worker
    #[arg(long)]
    pub effort: Option<String>,
    /// Per-spawn account selection
    #[arg(long)]
    pub account: Option<String>,
    /// Session substrate (pane | thread | headless; bg is a deprecated alias)
    #[arg(long)]
    pub substrate: Option<String>,
}

impl SpawnAxes {
    /// The axis flag spellings this schema owns, short aliases included.
    pub const AXIS_FLAGS: &'static [&'static str] = &[
        "--route",
        "--provider",
        "-P",
        "--harness",
        "-H",
        "--model",
        "-m",
        "--effort",
        "--account",
        "--substrate",
    ];

    /// Parse the axis flags out of a spawn argv, stopping at the provider
    /// argv fence (`--argv` or bare `--`). Non-axis tokens belong to the rest
    /// of the spawn grammar and are ignored here, but the axes themselves are
    /// strict: a valueless axis flag is an error in the voice the old cursor
    /// parser used, and a repeated axis flag is refused so the client and the
    /// overlay can never read different values from the same argv.
    pub fn scan(toks: &[String]) -> Result<(SpawnAxes, usize), String> {
        let fence = toks.iter().position(|t| t == "--argv" || t == "--");
        let head_len = fence.unwrap_or(toks.len());
        let head = &toks[..head_len];
        // Collect only the axis flag/value pairs, so the strict clap parse
        // never sees the non-axis flags the rest of the grammar owns.
        let mut pairs: Vec<String> = Vec::new();
        let mut i = 0;
        while i < head.len() {
            if Self::AXIS_FLAGS.contains(&head[i].as_str()) {
                if i + 1 >= head.len() || Self::looks_like_flag(&head[i + 1]) {
                    return Err(format!("{} needs a value", head[i]));
                }
                pairs.push(head[i].clone());
                pairs.push(head[i + 1].clone());
                i += 2;
            } else {
                i += 1;
            }
        }
        let cmd = <Self as clap::CommandFactory>::command();
        let matches = cmd
            .try_get_matches_from(pairs)
            .map_err(|e| refusal_line("fno-agents spawn", &e))?;
        let axes = SpawnAxes {
            route: matches.get_one::<String>("route").cloned(),
            provider: matches.get_one::<String>("provider").cloned(),
            harness: matches.get_one::<String>("harness").cloned(),
            model: matches.get_one::<String>("model").cloned(),
            effort: matches.get_one::<String>("effort").cloned(),
            account: matches.get_one::<String>("account").cloned(),
            substrate: matches.get_one::<String>("substrate").cloned(),
        };
        Ok((axes, head_len))
    }

    fn looks_like_flag(tok: &str) -> bool {
        tok.starts_with('-') && tok != "-"
    }

    /// Drop the axis flag/value pairs from `toks`, keeping the fence and the
    /// provider payload byte-exact. `scan` must have accepted the same argv
    /// first: every surviving axis flag is paired with its value.
    pub fn strip_axes(toks: &[String], fence: usize) -> Vec<String> {
        let mut out = Vec::with_capacity(toks.len());
        let mut i = 0;
        while i < toks.len() {
            if i >= fence {
                out.extend_from_slice(&toks[i..]);
                break;
            }
            if Self::AXIS_FLAGS.contains(&toks[i].as_str()) {
                i += 2; // flag plus value; scan refused a missing value already
                continue;
            }
            out.push(toks[i].clone());
            i += 1;
        }
        out
    }
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
        // this exact argv (post-verb) is what cli/src/fno/restart.py
        // spawns; the dispatch strips the verb before the parser runs.
        let a = RestartArgs::try_parse_from(["--json"]).expect("--json parses");
        assert!(a.json.json);
        assert!(!a.force);
        let a = RestartArgs::try_parse_from(["--force"]).expect("--force parses");
        assert!(a.force);
        assert!(!a.json.json);
        let a = RestartArgs::try_parse_from(["--if-drifted"]).expect("--if-drifted parses");
        assert!(a.if_drifted);
        assert!(!a.force);
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
        assert_eq!(a.events, Some(PathBuf::from("e.jsonl")));
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

    #[test]
    fn spawn_axes_parse_all_supported_spellings() {
        let toks: Vec<String> = ["--provider", "zai", "--model=glm", "-m", "luna"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (axes, fence) = SpawnAxes::scan(&toks).expect("mixed spellings parse");
        assert_eq!(axes.provider.as_deref(), Some("zai"));
        assert_eq!(axes.model.as_deref(), Some("luna"));
        assert_eq!(fence, toks.len());
    }

    #[test]
    fn spawn_axes_fence_keeps_provider_payload_opaque() {
        let toks: Vec<String> = [
            "--model", "luna", "--argv", "--", "claude", "--model", "sonnet",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (axes, fence) = SpawnAxes::scan(&toks).expect("fenced argv parses");
        assert_eq!(axes.model.as_deref(), Some("luna"));
        assert_eq!(fence, 2);
        // strip_axes keeps the fence and the payload byte-exact.
        let stripped = SpawnAxes::strip_axes(&toks, fence);
        assert_eq!(
            stripped,
            ["--argv", "--", "claude", "--model", "sonnet"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn spawn_axes_missing_value_refuses() {
        for bad in [
            vec!["--model"],
            vec!["--model", "--force"],
            vec!["-P", "--argv", "--", "claude"],
        ] {
            let toks: Vec<String> = bad.iter().map(|s| s.to_string()).collect();
            let err = SpawnAxes::scan(&toks).expect_err("missing value refuses");
            assert!(err.ends_with("needs a value"), "{err}");
        }
    }

    #[test]
    fn spawn_axes_ignores_non_axis_tokens() {
        let toks: Vec<String> = [
            "--name",
            "wk",
            "--portal",
            "1",
            "--provider",
            "zai",
            "--cwd",
            "/x",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (axes, _) = SpawnAxes::scan(&toks).expect("non-axis tokens are ignored");
        assert_eq!(axes.provider.as_deref(), Some("zai"));
        assert_eq!(axes.model, None);
    }
}
