//! `fno-agents hook pipe-guard` - refuse a Bash call whose pipe hides the
//! exit or the rows the call was run for, or whose command position runs the
//! third-party Backlog.md CLI.
//!
//! Four readings, each a refusal because the call answers a different
//! question than the one asked:
//!
//! 1. Truncation: a count-or-existence producer (`pgrep`, `ls`, `find`,
//!    `ps aux`, `rg -l`, `grep -l`, `gh run|pr list`, `git worktree list`,
//!    `fno backlog find|list`, `fno agents list|ls`) cut by `head`/`tail`.
//!    A truncated listing answers "how many" or "does it exist" with the
//!    last stage's rows, and the answer looks identical. This ports the
//!    retired Python truncation guard's producers and refusal text
//!    verbatim.
//! 2. Timeout first: with no pipefail, `timeout 120 x | tail` reports
//!    tail's 0 when the kill lands at exit 124, and the killed command
//!    printed nothing - the call reads as a quiet success.
//! 3. `$?` read right after a pipeline: with no pipefail it is the LAST
//!    stage's exit, so `x | tail -3; echo rc=$?` prints rc=0 when x failed
//!    or timeout killed it.
//!
//! `set -o pipefail` (or `setopt PIPE_FAIL`) disarms readings 2 and 3, at
//! subshell depth 0 only: a pipefail set inside `( ... )` ends with that
//! subshell and covers nothing after the `)`.
//!
//! One seam stays fail-open: the lexer splices every substitution body
//! end-to-end after the main tokens, so a pipefail set inside one body is
//! still set when a later body is judged, although bash runs each
//! substitution in its own subshell without it. Zero corpus instances
//! (5000-command replay, 2026-09-22); closing it needs a splice-boundary
//! token in the shared lexer, which both guards treat as a contract.
//!
//! Parse-only: no subprocess, no repository scoping, and it fails OPEN on
//! anything unexpected (a null payload, a non-Bash tool, a blank command,
//! an unbalanced quote). Why a refusal and not prose: prose does not fire
//! at the moment of a tool call; a refusal does. Shim:
//! `hooks/pipe-guard.sh`.

use serde_json::Value;
use std::path::PathBuf;

use super::king_guard::lex;
use super::test_run_guard::{basename, head_of, stages};

/// Prose does not fire at the moment of a tool call; a refusal does.
const TRUNCATION_REASON: &str = "[fno pipe guard] `{cmd}` truncates a count-or-existence read. A truncated listing answers a different question than the one you asked, and the answer looks identical: on 2026-09-03 `pgrep -fl fno-agents | head -4` produced the false claim 'no daemon running' while the daemon was live. For a COUNT use `| wc -l`. For an EXISTENCE claim read the full listing, or narrow the producer's own filter until it is short. Never truncate a zero you intend to trust (scripts/lib/assert-absent.sh: 'assert a positive marker, never an absence').";

const TIMEOUT_REASON: &str = "[fno pipe guard] `{cmd}` puts `timeout` in the first stage of a pipeline. When timeout kills the command (exit 124), the pipeline reports the last stage's exit, 0, and the killed command printed nothing, so the call reads as a quiet success. On 2026-09-16 three `timeout 120 fno backlog update ... | tail -3` calls were killed at 120s and read as clean.\n\nThe Bash tool already bounds every call and reports its own timeout as an error, so drop `timeout` unless it guards other work. Otherwise put `set -o pipefail;` first so a kill reports 124 (pair it with tail: head can end the command with SIGPIPE 141), or redirect: `timeout 120 x > out.txt 2>&1; echo EXIT=$?; tail -3 out.txt`.";

const STATUS_REASON: &str = "[fno pipe guard] `{cmd}` reads `$?` right after a pipeline. With no pipefail, `$?` is the LAST stage's exit, so `x | tail -3; echo rc=$?` prints rc=0 when x failed or timeout killed it. On 2026-09-16 that read produced rc=0 from a killed update and a bug that did not exist.\n\nTo read x's own exit, redirect instead: `x > out.txt 2>&1; echo EXIT=$?; tail -3 out.txt`. Or put `set -o pipefail;` first (pair it with tail, not head). `${PIPESTATUS[0]}` is bash-only and reads empty in zsh.";

/// Bare `backlog` in command position resolves to the third-party
/// Backlog.md CLI, not fno; the remedy is the fno prefix.
const BACKLOG_REASON: &str = "[fno pipe guard] `{cmd}` runs bare `backlog`, which resolves to the third-party Backlog.md CLI at ~/.bun/bin/backlog, not fno. On 2026-09-26 that CLI's guidelines block rewrote AGENTS.md and CLAUDE.md in canonical (627 lines telling agents to run `backlog task create`), and every session in canonical loaded it. Prefix the verb: `fno backlog find|get|idea|update` is the node graph.";

/// Entry: read the payload once, judge, print, always exit 0.
pub fn run(_args: &[String]) -> i32 {
    let payload: Value = serde_json::from_str(super::read_stdin().trim()).unwrap_or(Value::Null);
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let refusal = judge(&payload);
    super::emit_guard_decision(&cwd, "pipe-guard", "Bash", refusal.is_some());
    match refusal {
        Some(reason) => super::emit_block(&reason),
        None => super::emit_allow(),
    }
}

/// The whole verdict for one payload: a refusal, or None to allow. A null
/// payload, a non-Bash tool and a blank command allow.
fn judge(payload: &Value) -> Option<String> {
    if payload.is_null() {
        return None;
    }
    if payload.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return None;
    }
    let cmd = payload
        .get("tool_input")
        .and_then(|ti| ti.get("command"))
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())?;
    decide(cmd)
}

/// True when this token ends one segment and starts the next. Parens stay
/// INLINE: a pipeline whose stages sit across a subshell or escaped parens
/// (`find \( x \) | head`) is one pipe, and splitting on `)` never lets the
/// producer and the cut meet.
fn is_break(tok: &str) -> bool {
    matches!(tok, ";" | ";;" | "&" | "&&" | "||" | "\n")
}

/// The whole verdict for one command string: a refusal, or None to allow.
/// Separated from `run` so tests exercise the same predicate the hook does.
fn decide(command: &str) -> Option<String> {
    let Some(toks) = lex(command) else {
        return None; // unbalanced quotes: cannot tell command position, allow
    };
    let mut pipefail = false;
    let mut depth: usize = 0;
    let mut last_pipeline: Option<Vec<String>> = None;
    let mut seg: Vec<String> = Vec::new();
    let mut saw_set = false;
    for tok in toks {
        if is_break(&tok) {
            if let Some(r) = judge_segment(&seg, &mut last_pipeline, pipefail) {
                return Some(r);
            }
            seg.clear();
            saw_set = false;
            continue;
        }
        if tok == "(" {
            depth += 1;
        } else if tok == ")" {
            depth = depth.saturating_sub(1);
        }
        let base = basename(&tok);
        if base == "set" || base == "setopt" {
            saw_set = true;
        }
        if saw_set && depth == 0 && tok.to_lowercase().replace('_', "") == "pipefail" {
            pipefail = true;
        }
        seg.push(tok);
    }
    judge_segment(&seg, &mut last_pipeline, pipefail)
}

/// The three readers for one finished segment, in order. `pipefail` is the
/// flag as of this segment; the subshell depth walk lives in `decide`.
fn judge_segment(
    seg: &[String],
    last_pipeline: &mut Option<Vec<String>>,
    pipefail: bool,
) -> Option<String> {
    if seg.is_empty() {
        return None;
    }
    // 4. Bare `backlog` in command position: the third-party Backlog.md CLI,
    //    never fno. Every pipeline stage is judged (`printf y | backlog init`),
    //    `head_of` resolves wrappers, assignments and subshell openers, and a
    //    reserved keyword leading the segment (`if backlog ...`) is dropped so
    //    the command behind it is judged; `fno backlog` and `echo backlog` do
    //    not refuse.
    let cmd: Vec<String> = seg
        .iter()
        .skip_while(|t| matches!(t.as_str(), "if" | "while" | "until" | "then" | "do"))
        .cloned()
        .collect();
    if stages(&cmd)
        .iter()
        .any(|stage| head_of(stage, false).is_some_and(|(head, _, _)| head == "backlog"))
    {
        return Some(BACKLOG_REASON.replace("{cmd}", &seg.join(" ")));
    }
    // A group closer's status is its last command's: it keeps the flag.
    let is_closer = seg
        .iter()
        .all(|t| matches!(t.as_str(), "}" | "fi" | "done" | "esac"));
    let stages = stages(seg);
    let reads_status = seg.iter().any(|t| t.contains("$?") || t.contains("${?}"));
    // An assignment that captures a piped substitution (`v=$(x | tail -1)`)
    // sets `$?` to the LAST stage's exit exactly like a bare pipeline.
    let subst_pipe = seg
        .first()
        .is_some_and(|t| t.contains('=') && t.contains("$(") && t.contains('|'));

    // 1. Truncation: a count-or-existence producer cut by head/tail.
    if stages.len() >= 2 {
        let readings = [head_of(&stages[0], false), head_of(&stages[0], true)];
        let producer = readings
            .iter()
            .filter_map(|r| r.as_ref())
            .any(|(h, a, _)| is_producer(h, a));
        let timeout_wrap = readings
            .iter()
            .filter_map(|r| r.as_ref())
            .any(|(_, _, ws)| ws.iter().any(|w| w == "timeout" || w == "gtimeout"));
        if producer && stages.last().is_some_and(|last| truncates(last)) {
            return Some(TRUNCATION_REASON.replace("{cmd}", &seg.join(" ")));
        }
        // 2. Timeout first: the kill lands as the last stage's 0.
        if !pipefail && timeout_wrap {
            return Some(TIMEOUT_REASON.replace("{cmd}", &seg.join(" ")));
        }
    }

    // 3. `$?` right after a pipeline, when no pipefail is set.
    if !is_closer {
        if reads_status && !pipefail {
            if let Some(pl) = last_pipeline {
                return Some(
                    STATUS_REASON
                        .replace("{cmd}", &format!("{} ; {}", pl.join(" "), seg.join(" "))),
                );
            }
        }
        *last_pipeline = if stages.len() >= 2 || subst_pipe {
            Some(seg.to_vec())
        } else {
            None
        };
    }
    None
}

/// The count-or-existence producers of the retired Python guard, matched on
/// the first stage's command position, so a wrapper the Python prefix regex
/// never saw (`env pgrep | head`) is an intended flip of the port.
fn is_producer(head: &str, argv: &[String]) -> bool {
    match head {
        "pgrep" | "ls" | "find" => true,
        "ps" => argv
            .iter()
            .find(|t| !t.starts_with('-'))
            .is_some_and(|t| matches!(t.as_str(), "aux" | "ax" | "ef")),
        "rg" => argv
            .iter()
            .any(|t| matches!(t.as_str(), "-l" | "--files-with-matches" | "--files")),
        "grep" => argv
            .iter()
            .any(|t| t.starts_with('-') && !t.starts_with("--") && t[1..].contains('l')),
        "gh" => {
            matches!(argv.first().map(String::as_str), Some("run" | "pr"))
                && argv.get(1).map(String::as_str) == Some("list")
        }
        "git" => {
            argv.first().map(String::as_str) == Some("worktree")
                && argv.get(1).map(String::as_str) == Some("list")
        }
        "fno" => match (
            argv.first().map(String::as_str),
            argv.get(1).map(String::as_str),
        ) {
            (Some("backlog"), Some("find" | "list")) => true,
            (Some("agents"), Some("list" | "ls")) => true,
            _ => false,
        },
        _ => false,
    }
}

/// True when this head/tail stage CUTS ROWS. `-c` is a byte bound and
/// `-f`/`-F` a follow: none drops a row from a finished listing.
fn truncates(stage: &[String]) -> bool {
    let Some(head) = stage.first().map(|t| basename(t)) else {
        return false;
    };
    if head != "head" && head != "tail" {
        return false;
    }
    for tok in &stage[1..] {
        if tok.starts_with('-') && !tok.starts_with("--") {
            let low = tok[1..].to_lowercase();
            if low.contains('c') || low.contains('f') {
                return false;
            }
        } else if tok == "--bytes"
            || tok == "--follow"
            || tok.starts_with("--bytes=")
            || tok.starts_with("--follow=")
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(cmd: &str) -> String {
        decide(cmd).expect("expected a refusal")
    }

    fn allowed(cmd: &str) {
        assert!(decide(cmd).is_none(), "expected allow: {cmd}");
    }

    #[test]
    fn null_payload_and_non_bash_allow() {
        assert!(judge(&Value::Null).is_none());
        assert!(judge(
            &serde_json::json!({"tool_name": "Glob", "tool_input": {"command": "x | tail -1"}})
        )
        .is_none());
    }

    #[test]
    fn blank_and_unbalanced_allow() {
        assert!(decide("").is_none());
        assert!(decide("   ").is_none());
        // An unbalanced quote never executes: allow.
        assert!(decide("echo \"x | tail; echo $?").is_none());
    }

    #[test]
    fn truncation_denies() {
        let r = denied("pgrep -fl fno-agents | head -4");
        assert!(r.contains("wc -l"), "names the count door: {r}");
        assert!(r.contains("truncates a count-or-existence read"));
        let r = denied("gh run list --limit 50 | head -5");
        assert!(r.contains("truncates a count-or-existence read"));
        denied("cd /tmp && rg -l fno_agents | head -3");
        // The producer rides behind a transparent wrapper: still refused.
        denied("timeout 60 fno backlog find x | head -8");
    }

    #[test]
    fn truncation_allows() {
        allowed("tail -f .fno/last-ci.log");
        allowed("ls | head -c 200");
        allowed("cat README.md | head -20");
        allowed("pgrep -fl fno-agents | tail -F");
        allowed("ls | head -20 | wc -l");
    }

    #[test]
    fn parens_never_split_a_pipeline() {
        // Corpus flip: escaped parens inside a find stage used to end the
        // segment, so the producer and the cut never met in one pipeline.
        denied("find /x -type f \\( -name \"*.ts\" \\) | head -50");
        denied("/usr/bin/find /x -type f \\( -name \"*.ts\" \\) | head -50");
    }

    #[test]
    fn producer_name_in_argument_position_allows() {
        // The retired shell test pinned this shape: `pgrep` as echo's
        // argument is prose, never a producer in command position.
        allowed("echo pgrep | head -4");
    }

    #[test]
    fn deep_substitution_nesting_allows_without_aborting() {
        // A hostile nest lexes to None past the bound; the hook allows
        // instead of overflowing the stack.
        let deep = format!("{}x | tail -1", "$( ".repeat(400));
        allowed(&deep);
    }

    #[test]
    fn timeout_first_denies() {
        let r = denied("timeout 60 fno backlog get x | tail -3");
        assert!(r.contains("exit 124"), "names the kill exit: {r}");
        assert!(r.contains("set -o pipefail"));
        denied("env FOO=1 gtimeout 30 cargo build 2>&1 | tail -5");
        denied("(timeout 120 fno bundle check 2>&1 | tail -15); echo \"BUNDLE_EXIT=$?\"");
    }

    #[test]
    fn status_after_pipeline_denies() {
        let r = denied("(fno bundle check 2>&1 | tail -8); echo \"EXIT=$?\"");
        assert!(r.contains("$?"));
        assert!(r.contains("set -o pipefail"));
        assert!(r.contains("EXIT="));
        denied("cargo build 2>&1 | tail -5\necho \"Exit code: $?\"");
        denied("{ x | tail -3; }; echo $?");
        denied("if x | tail -1; then echo $?; fi");
        denied("x | tail -3 || echo \"failed $?\"");
        denied("v=$(x | tail -1); echo $?");
        // pipefail inside a subshell covers nothing after the `)`.
        denied("(set -o pipefail; a | tail -3); b | tail -3; echo $?");
    }

    #[test]
    fn pipefail_and_intervening_commands_allow() {
        allowed("set -o pipefail; timeout 60 fno backlog get x | tail -3; echo rc=$?");
        allowed("setopt PIPE_FAIL; x | tail -3; echo $?");
        allowed("x | tail -3; y; echo $?");
        allowed("echo $?");
        allowed("timeout 60 x > /tmp/o.txt 2>&1; echo EXIT=$?");
        allowed("timeout 60 x");
        allowed("echo timeout | tail -1");
    }

    #[test]
    fn bare_backlog_denies() {
        let r = denied("backlog task list");
        assert!(r.contains("fno backlog"), "names the remedy: {r}");
        assert!(r.contains("~/.bun/bin/backlog"), "names the impostor: {r}");
        denied("backlog init");
        // A chained, subshelled or wrapped bare call is still bare.
        denied("cd /tmp && backlog task list");
        denied("fno backlog get x; backlog task list");
        denied("(backlog task list)");
        denied("env backlog task list");
        denied("FOO=1 backlog task list");
        denied("if backlog init; then echo hi; fi");
        denied("while backlog task list; do :; done");
        // A later pipeline stage is still command position.
        denied("printf 'y\\n' | backlog init");
        denied("cat x | env backlog task list");
    }

    #[test]
    fn fno_backlog_and_argument_position_allow() {
        allowed("fno backlog find x");
        allowed("fno backlog get done");
        // `backlog` as an argument is prose, never a command position.
        allowed("echo backlog");
        allowed("rg backlog hooks/");
        allowed("git log --oneline -- backlog.md");
        allowed("for b in backlog task; do echo done; done");
        allowed("fno backlog get x | tail -3");
    }
}
