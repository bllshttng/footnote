//! The `fno-agents --help` surface: one usage line per dispatchable verb.
//!
//! LIVES OUTSIDE THE BIN so the file-budget guard stays honest: the bin grew
//! past its 5,000-line budget long ago and is shrink-only; this table is the
//! one piece a new verb still has to touch, so it moved to a module named for
//! the question it answers. Display order == slice order == `--help` order.

/// Usage line per dispatchable verb; the leading token is the verb name and the
/// slice order is the `--help` display order. This MUST cover every routable
/// verb (the `build_request` match arms plus the directly-dispatched specials).
/// `test_rust_client_verbs_match_client_rs` (Python) guards client.rs<->router
/// parity; `print_help_lists_every_routable_verb` guards this display
/// list against that set, so a new verb cannot land without a `--help` entry
///.
/// Usage line per dispatchable verb; the leading token is the verb name and the
/// slice order is the `--help` display order. This MUST cover every routable
/// verb (the `build_request` match arms plus the directly-dispatched specials).
/// `test_rust_client_verbs_match_client_rs` (Python) guards client.rs<->router
/// parity; `print_help_lists_every_routable_verb` (below) guards this display
/// list against that set, so a new verb cannot land without a `--help` entry
///.
pub const CLIENT_VERB_USAGE: &[&str] = &[
    "spawn <name> --provider <p> [--substrate pane|thread|headless] [-s <squad>] [-x left|right|up|down] [--cwd <dir>|--fresh|--here] [--force] [--no-wait] --argv -- <cmd...>",
    "ask <name> <message> [--cwd <dir>|--fresh|--here]",
    "list [--all] [--status <live|orphaned|unknown>] [--progress <advancing|awaiting-operator|parked|refused|unknown>]",
    "status",
    // --force is break-glass: it SIGKILLs a wedged lock holder (and would kill
    // a healthy one too). Plain restart is the graceful path; say so here
    // because this line is what `restart --help` prints.
    "restart [--force]  # --force: break-glass SIGKILL of the lockfile holder; plain restart is graceful",
    "reap [--json] [--dry-run]",
    "rename <name> --name <new-label>   -- renames the registry LABEL. The old label keeps resolving as an alias and the harness session is untouched",
    "stop <name> [--force]",
    // retired-ok: help names the existing Claude callee to describe actual behavior, not to teach a direct retired command.
    "rm <name> [--force]   --force drops the registry row even when the row is LIVE or harness teardown fails; a live pane worker that cannot be stopped is still refused; a claude row's harness session is removed too (claude rm <short_id>), and claude removes that session's WORKTREE under its own guards - it keeps a worktree with uncommitted changes and refuses one holding commits it cannot confirm are saved elsewhere; a non-claude bg or headless process survives, a mux-hosted pane is killed with it",
    "loop-check --state <target-state.md> --transcript <transcript.jsonl> --cwd <project-root> [--events <events.jsonl>] [--global-events <global.jsonl>] [--settings <config.toml>] [--ledger <ledger.json>] [--now <rfc3339>] [--gh-bin <path>] [--git-bin <path>]",
    "finalize --state <target-state.md> --cwd <project-root> --reason <TerminationReason> [--transcript <transcript.jsonl>]",
    "reconcile",
    "drive-authority [--json]",
    "trace [options]",
    "registry-json",
    "ping",
    "resume <name> [--print-command] [--message/-m <text>] [--cross-project] [--cwd <existing-checkout>] [--account <id>]",
    "adopt <session-id> [--cross-project]",
    "attach <name>",
    "logs <name> [--follow] [options]",
    "loop run --driver target [options]",
    "report --session-id <uuid> --seq <n> --state working|blocked|done [--reason <text>] [--ttl-ms <n>]",
    "wait --agent <name> --state idle|blocked|done [--timeout-ms <n>] [--json]",
    "subscribe [--agent <name>] [--kinds state,exit] [--json]",
    "digest --session <s> [--since <ts> | --since-epoch <secs>] [--json]",
    "needs [--since-epoch <secs>] [--fires-floor <n>] [--json]",
    "feed [--since-epoch <secs>] [--limit <n>] [--node <id>] [--session <id>] [--json]",
    // `review-coverage` deliberately has NO entry here: the per-verb --help
    // intercept would print a one-line usage and shadow the verb's own
    // --help, which states the load-bearing contract (no way to assert
    // coverage without the reads, and the strict manifest-less defaults).
    //
    // `distress-scan` deliberately has NO entry here either, same reason:
    // it intercepts its own --help (DISTRESS_SCAN_USAGE in distress.rs)
    // with the fuller best-effort-always-exits-0 contract a one-liner would
    // shadow, and it is a hidden verb with no external callers to discover
    // it from a top-level list.
];

/// Return the usage line for `verb` (matched on the leading token), or `None`
/// for an unrecognized verb.
pub fn verb_usage(verb: &str) -> Option<&'static str> {
    CLIENT_VERB_USAGE
        .iter()
        .copied()
        .find(|usage| usage.split_whitespace().next() == Some(verb))
}

/// Full per-verb help for a verb whose contract does not fit the one-line
/// table above; checked before `verb_usage` by the per-verb `--help`
/// intercept in the bin. Same reason `review-coverage` owns its `--help`
/// inline: the load-bearing contract has to live in a string the
/// binary prints, next to the table entry that stays one line for the
/// top-level list.
pub const LOOP_CHECK_USAGE: &str = "\
usage: fno-agents loop-check --state <manifest> --transcript <transcript.jsonl> --cwd <project-root>
       [--driver target|king] [--events <p>] [--global-events <p>] [--settings <p>]
       [--global-settings <p>] [--ledger <p>] [--gh-budget-ledger <p>] [--now <rfc3339>]
       [--author-harness <h>] [--hook-input-stdin] [--gh-bin <p>] [--git-bin <p>]
       [--fno-bin <p>] [--read-timeout-ms <n>]

The stop-hook decision verb: it decides whether a driven session may stop,
and every verdict comes from external truth read fresh on each fire - PR
existence, CI, required-bot review, plan done_probes, budget. The
session's own claim of done is not an input.

--driver selects the arm and the manifest kind --state points at. target
(the default) reads a target manifest (target-state.md) and asks whether
its one deliverable shipped. king reads a king manifest (frontmatter
scope) and asks whether the crown scope drained. The arm is chosen by the
flag, never by sniffing the file, and any other value is refused.

Required: --state, --transcript, --cwd. Unknown flags are tolerated for
shim forward-compat. stdout is one JSON decision; exit 0 = a decision
(verdict allow or block), 2 = bad arguments.
";

/// The fuller help body for `verb`, when one exists above. `None` falls
/// through to the one-line `verb_usage` entry.
pub fn verb_help(verb: &str) -> Option<&'static str> {
    match verb {
        "loop-check" => Some(LOOP_CHECK_USAGE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    /// every flag loop-check's parser accepts appears in its help
    /// body. This is the gate that retires the class: --driver was
    /// load-bearing, accepted, and absent from the one place a user can
    /// look, and nothing caught that. The gate reads parse_args's own
    /// source, so a new flag cannot land undocumented.
    #[test]
    fn loop_check_help_covers_every_accepted_flag() {
        let source = include_str!("loopcheck.rs");
        let start = source
            .find("fn parse_args(")
            .expect("parse_args not found in loopcheck.rs");
        let body = &source[start..];
        let body = &body[..body.find("\n}\n").expect("parse_args has no closing brace")];

        let flag = Regex::new(r#"try_flag_value\(arg, "(--[\w-]+)"|arg == "(--[\w-]+)""#).unwrap();
        let mut flags: Vec<&str> = Vec::new();
        for caps in flag.captures_iter(body) {
            let f = caps
                .get(1)
                .or_else(|| caps.get(2))
                .expect("one alternation matched")
                .as_str();
            if !flags.contains(&f) {
                flags.push(f);
            }
        }
        // Positive control: the extraction must find the known flags, so a
        // broken scan fails loudly instead of passing on an empty set.
        assert!(
            flags.contains(&"--state"),
            "scan missed --state; extraction is broken: {flags:?}"
        );
        assert!(
            flags.contains(&"--driver"),
            "scan missed --driver; extraction is broken: {flags:?}"
        );
        assert!(
            flags.len() >= 16,
            "unexpectedly few flags scanned: {flags:?}"
        );

        for f in &flags {
            assert!(
                LOOP_CHECK_USAGE.contains(f),
                "loop-check accepts {f} but LOOP_CHECK_USAGE never names it"
            );
        }
        // The contract words the node asked for, beyond the flag spellings.
        // Whitespace-normalized: the const line-wraps its prose.
        let one_line: String = LOOP_CHECK_USAGE
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(verb_help("loop-check").is_some());
        assert!(one_line.contains("target (the default)"));
        assert!(one_line.contains("target manifest"));
        assert!(one_line.contains("king manifest"));
    }
}
