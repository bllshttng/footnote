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
/// (ab-351427cb).
/// Usage line per dispatchable verb; the leading token is the verb name and the
/// slice order is the `--help` display order. This MUST cover every routable
/// verb (the `build_request` match arms plus the directly-dispatched specials).
/// `test_rust_client_verbs_match_client_rs` (Python) guards client.rs<->router
/// parity; `print_help_lists_every_routable_verb` (below) guards this display
/// list against that set, so a new verb cannot land without a `--help` entry
/// (ab-351427cb).
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
    // `review-coverage` deliberately has NO entry here: the per-verb --help
    // intercept would print a one-line usage and shadow the verb's own
    // --help, which states the load-bearing contract (no way to assert
    // coverage without the reads, and the strict manifest-less defaults).
];

/// Return the usage line for `verb` (matched on the leading token), or `None`
/// for an unrecognized verb.
pub fn verb_usage(verb: &str) -> Option<&'static str> {
    CLIENT_VERB_USAGE
        .iter()
        .copied()
        .find(|usage| usage.split_whitespace().next() == Some(verb))
}
