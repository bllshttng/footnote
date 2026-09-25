//! PreToolUse guard: refuse a raw `pytest` or `cargo test` run in a footnote
//! checkout.
//!
//! `fno doctor test` is the only test door with admission: it holds the
//! machine-wide `test:suite` claim, waits behind a live cargo build through
//! the rustc wrapper's build admission, pins PYTHONPATH to the worktree so
//! the right fno is imported, and owns a process group that is always
//! reaped. A raw `pytest` or `cargo test` takes none of that. Measured
//! 2026-09-18: two workers running raw suites crushed the machine, which is
//! what this guard was filed from. Prose does not fire at the moment of a
//! tool call; a refusal does.
//!
//! A refusal is read in SHELL COMMAND POSITION, per pipeline stage, through
//! the same `lex` the king guard uses: transparent wrappers (`env pytest`,
//! `timeout 30 cargo test`), env-assignment prefixes, full paths, every
//! command-substitution body (each one is lexed as its own command), and
//! one level of `bash -c` payloads. The other uv doors are covered because they
//! are the same raw run: `uv run pytest`, `uvx pytest`, `uv tool run
//! pytest`, and `python -m pytest` in both `-m` spellings.
//!
//! Fails OPEN on anything unexpected: a guard that breaks a session on its
//! own bug is worse than the runs it prevents. Scoped to footnote checkouts
//! (`cli/src/fno` plus `hooks/hooks.json` at the repo root); every other
//! repository allows. Scripts that call pytest internally, and pytest fed
//! to `xargs`, are known fail-opens, the accepted shape for this class of
//! guard. Shell shim: `hooks/test-run-guard.sh`.
//!
//! A whole crate suite through a blessed door is still a whole suite:
//! `fno doctor test rust` and the `fno-agents test-run -- cargo test` door
//! are refused too when the argv selects a whole crate, unless the command
//! carries the literal `FNO_TEST_FULL=1`.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::king_guard::lex;
use crate::test_run::cargo_test_selects_whole_suite;

/// Wrappers that are transparent to command position: `sudo pytest ...`
/// still runs the pytest. Shell keywords that open a body are here too, so
/// `do pytest ...; done` inside a `for` resolves to the pytest.
pub(super) const TRANSPARENT: &[&str] = &[
    "nohup",
    "setsid",
    "exec",
    "time",
    "env",
    "command",
    "builtin",
    "nice",
    "ionice",
    "taskpolicy",
    "stdbuf",
    "caffeinate",
    "sudo",
    // `timeout` bounds a process but does not admit it, so it is transparent
    // HERE: `timeout 30 pytest` still leaves the machine with a raw suite.
    "timeout",
    "gtimeout",
    "do",
    "then",
    "else",
    "elif",
    "{",
    "(",
    "!",
];

/// Wrappers whose own positional operand sits BEFORE the wrapped command:
/// `timeout 300 pytest ...` spends `300` before the pytest ever appears.
pub(super) const POSITIONAL_LEAD: &[&str] = &["timeout", "gtimeout"];

/// Wrapper flags that swallow the next token, so the value is not mistaken
/// for the command (`sudo -u me pytest ...`). Read PER WRAPPER.
pub(super) fn wrapper_takes_value(wrapper: &str, flag: &str) -> bool {
    matches!(wrapper, "sudo" if matches!(flag, "-u" | "-g" | "-C" | "-U" | "-p" | "-D" | "-R" | "-T" | "-h"))
        || matches!(wrapper, "nice" if flag == "-n")
        || matches!(wrapper, "ionice" if matches!(flag, "-c" | "-n" | "-p"))
        || matches!(wrapper, "env" if matches!(flag, "-u" | "-S"))
        || matches!(wrapper, "exec" if flag == "-a")
        || matches!(wrapper, "stdbuf" if matches!(flag, "-i" | "-o" | "-e"))
        || matches!(wrapper, "taskpolicy" if flag == "-c")
}

const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh"];

/// uv flags whose NEXT token is a value, never the dispatched command.
/// Anything unlisted is read as boolean, which fails open: `--someday x
/// pytest` would read `x` as the command and allow.
const UV_VALUE_FLAGS: &[&str] = &[
    "--with",
    "--without",
    "--from",
    "--python",
    "--env-file",
    "--config-file",
    "--project",
    "--directory",
    "--default-index",
    "--index",
    "--exclude-newer",
    "--python-preference",
    "-p",
];

const PYTEST_REASON: &str = "[fno test-run guard] `{cmd}` runs pytest outside the suite admission. A raw suite takes no test:suite slot, waits behind no live cargo build, and imports whichever fno is first on PYTHONPATH - on this machine two raw suites at once are the measured crush behind this guard.\n\nRun `fno doctor test <test files>` instead, for example `fno doctor test cli/tests/unit/test_x.py`. It takes the one machine-wide test:suite slot, holds while another suite or cargo build is live, and pins PYTHONPATH to this worktree. A bare `fno doctor test` runs the whole cli/tests tree while it holds that slot.";

const CARGO_REASON: &str = "[fno test-run guard] `{cmd}` runs the crates suite unadmitted. Raw `cargo test` takes no test:suite slot and waits behind no live cargo build.\n\nRun the narrowest target through the door instead: `fno doctor test rust --manifest-path crates/<crate>/Cargo.toml --lib <module>::` for unit tests, or `--test <file stem>` for one integration file. A whole-crate run is refused here too; CI runs every suite on every PR.";

const WHOLE_RUST_REASON: &str = "[fno test-run guard] `{cmd}` runs a whole crate test suite. CI runs every suite on every PR. Here a whole run holds the one machine-wide test:suite slot for many minutes while one-test runs queue behind it.\n\nRun the narrowest target instead: `fno doctor test rust --manifest-path crates/<crate>/Cargo.toml --lib <module>::` for unit tests, or `--test <file stem>` for one integration file. To run the whole suite anyway, prefix the command with `FNO_TEST_FULL=1`; it then waits while targeted runs are queued.";

/// Which blessed door the refusal names.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Pytest,
    Cargo,
    WholeRust,
}

/// Entry: read the payload once, decide, print, always exit 0.
pub fn run(_args: &[String]) -> i32 {
    let payload: Value = serde_json::from_str(super::read_stdin().trim()).unwrap_or(Value::Null);
    let trace = std::env::var_os("FNO_GUARD_TRACE").is_some();
    let allow = |stage: &str| -> i32 {
        if trace {
            eprintln!("test-run-guard: allow at {stage}");
        }
        super::emit_allow()
    };

    // 1. Empty or unparseable payload: not a refusal.
    if payload.is_null() {
        return allow("payload-null");
    }
    // 2. Only Bash is judged.
    if payload.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return allow("tool-not-judged");
    }
    // 3. A call with no command has nothing to judge.
    let Some(cmd) = payload
        .get("tool_input")
        .and_then(|ti| ti.get("command"))
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())
    else {
        return allow("no-command");
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Some(root) = repo_root(&cwd) else {
        super::emit_guard_decision(&cwd, "test-run-guard", "Bash", false);
        return allow("no-repo");
    };

    match decide_at(cmd, Some(&root)) {
        Some(reason) => {
            super::emit_guard_decision(&cwd, "test-run-guard", "Bash", true);
            super::emit_block(&reason)
        }
        None => {
            super::emit_guard_decision(&cwd, "test-run-guard", "Bash", false);
            allow("no-raw-run")
        }
    }
}

/// The whole verdict for one command string against one repository root:
/// the refusal text, or None to allow. `root` of None (no resolvable repo)
/// always allows. Separated from `run` so the tests exercise the same
/// predicate the hook does, with no git subprocess in the loop.
fn decide_at(command: &str, root: Option<&Path>) -> Option<String> {
    let Some(tokens) = lex(command) else {
        return None; // unbalanced quotes: cannot tell command position, allow
    };
    let (kind, shown) = refused_segment(&tokens, 0, false)?;
    let root = root?;
    if !is_footnote_checkout(root) {
        return None;
    }
    let reason = match kind {
        Kind::Pytest => PYTEST_REASON,
        Kind::Cargo => CARGO_REASON,
        Kind::WholeRust => WHOLE_RUST_REASON,
    };
    Some(reason.replace("{cmd}", &shown))
}

pub(crate) fn basename(tok: &str) -> &str {
    tok.rsplit('/').next().unwrap_or(tok)
}

pub(crate) fn is_python(name: &str) -> bool {
    matches!(name, "python" | "python3") || {
        // python3.N point releases name the same interpreter.
        let Some(rest) = name.strip_prefix("python3") else {
            return false;
        };
        let Some(dot) = rest.strip_prefix('.') else {
            return false;
        };
        !dot.is_empty() && dot.bytes().all(|b| b.is_ascii_digit())
    }
}

/// True when argv carries `-m <target>` in either spelling: the split
/// `python -m pytest -q` and the attached `python -mpytest`, which CPython
/// accepts identically.
pub(crate) fn has_dash_m_module(argv: &[String], target: &str) -> bool {
    argv.iter().enumerate().any(|(i, tok)| {
        if tok == "-m" {
            argv.get(i + 1)
                .map(|n| basename(n) == target)
                .unwrap_or(false)
        } else if let Some(attached) = tok.strip_prefix("-m") {
            !attached.is_empty() && basename(attached) == target
        } else {
            false
        }
    })
}

/// Index of the first token that is neither a flag nor a flag value.
fn first_positional(tokens: &[String], value_flags: &[&str]) -> Option<usize> {
    let mut skip = false;
    for (i, tok) in tokens.iter().enumerate() {
        if skip {
            skip = false;
            continue;
        }
        if tok.starts_with('-') && tok.len() > 1 {
            skip = value_flags.contains(&tok.as_str());
            continue;
        }
        return Some(i);
    }
    None
}

/// True when a `uv ... run ...` argv dispatches a raw pytest. uv's own flag
/// and value table keeps growing (`--group` today, another tomorrow), and a
/// guard must not track it, so past `run` (and `tool run`) the WHOLE argv
/// is scanned: any token named pytest, or a python followed by `-m
/// pytest`, refuses. The bias is toward refusing a guard-shaped miss.
fn uv_run_dispatches_pytest(argv: &[String]) -> bool {
    let Some(i) = first_positional(argv, UV_VALUE_FLAGS) else {
        return false;
    };
    let sub = basename(&argv[i]);
    let after_sub: &[String] = if sub == "tool" {
        match first_positional(&argv[i + 1..], UV_VALUE_FLAGS) {
            Some(j) if basename(&argv[i + 1 + j]) == "run" => &argv[i + j + 2..],
            _ => return false,
        }
    } else if sub == "run" {
        &argv[i + 1..]
    } else {
        return false;
    };
    after_sub.iter().any(|t| basename(t) == "pytest") || has_dash_m_module(after_sub, "pytest")
}

/// (basename, rest) for `uvx ...` (uv tool run shorthand).
fn uvx_target(argv: &[String]) -> (Option<String>, Vec<String>) {
    match first_positional(argv, UV_VALUE_FLAGS) {
        Some(m) => (Some(basename(&argv[m]).to_string()), argv[m + 1..].to_vec()),
        None => (None, Vec::new()),
    }
}

/// True when a cargo invocation runs the test suite: the subcommand `test`
/// or its advertised alias `t` (cargo --help lists `test, t`). Cargo's
/// global flag table also grows (`--color always test`), so like the uv
/// door the whole argv is scanned for the word; the bias is toward
/// refusing a guard-shaped miss. A `+toolchain` token is neither a flag
/// nor the subcommand, and `--` ends the scan.
fn cargo_runs_tests(argv: &[String]) -> bool {
    for tok in argv {
        if tok == "--" {
            return false;
        }
        if tok.starts_with('+') {
            continue;
        }
        let base = basename(tok);
        if base == "test" || base == "t" {
            return true;
        }
    }
    false
}

/// Which raw run sits at this command position, if any.
fn refused_head(head: &str, argv: &[String]) -> Option<Kind> {
    if head == "pytest" || head == "py.test" {
        return Some(Kind::Pytest);
    }
    if is_python(head) && has_dash_m_module(argv, "pytest") {
        return Some(Kind::Pytest);
    }
    if head == "uv" && uv_run_dispatches_pytest(argv) {
        return Some(Kind::Pytest);
    }
    if head == "uvx" {
        let (target, _) = uvx_target(argv);
        if target.as_deref() == Some("pytest") {
            return Some(Kind::Pytest);
        }
    }
    if head == "cargo" && cargo_runs_tests(argv) {
        return Some(Kind::Cargo);
    }
    if matches!(head, "fno" | "fno-py") {
        if let Some(cargo_argv) = doctor_test_rust_cargo_argv(argv) {
            if cargo_test_selects_whole_suite(&cargo_argv) {
                return Some(Kind::WholeRust);
            }
        }
    }
    if head == "fno-agents" && argv.first().map(String::as_str) == Some("test-run") {
        if let Some(dd) = argv.iter().position(|t| t == "--") {
            let inner = &argv[dd + 1..];
            if let Some((inner_head, inner_argv, _)) = head_of(inner, false) {
                if inner_head == "cargo" {
                    let mut full = vec!["cargo".to_string()];
                    full.extend(inner_argv);
                    if cargo_test_selects_whole_suite(&full) {
                        return Some(Kind::WholeRust);
                    }
                }
            }
        }
    }
    None
}

/// The cargo argv an `fno doctor test rust` invocation forwards, when `argv`
/// is that door's shape: `doctor test`, then `rust`, with the transport
/// flags `--stream` and `--log`/`--log=<path>` dropped wherever they sit.
/// None for any other shape - the classifier is never fed a guessed argv.
fn doctor_test_rust_cargo_argv(argv: &[String]) -> Option<Vec<String>> {
    let mut rest = argv;
    for expected in ["doctor", "test"] {
        if rest.first().map(String::as_str) != Some(expected) {
            return None;
        }
        rest = &rest[1..];
    }
    let mut tail: Vec<String> = Vec::new();
    let mut saw_rust = false;
    let mut i = 0;
    while i < rest.len() {
        let tok = &rest[i];
        if tok == "--stream" || tok == "--log" || tok.starts_with("--log=") {
            i += if tok == "--log" { 2 } else { 1 };
            continue;
        }
        if tok == "rust" && !saw_rust {
            saw_rust = true;
            i += 1;
            continue;
        }
        tail.push(tok.clone());
        i += 1;
    }
    if !saw_rust {
        return None;
    }
    let mut out = vec!["cargo".to_string(), "test".to_string()];
    out.extend(tail);
    Some(out)
}

/// True when this token ends one command and starts the next. Redirect
/// machinery (`2>&` as one token, `|&`) never separates. `||` separates:
/// the or-list right side is its own command, not an operand of the left.
pub(super) fn is_separator(tok: &str) -> bool {
    matches!(tok, ";" | ";;" | "&" | "&&" | "||" | "\n" | ")")
}

/// Split a token list on command separators.
pub(super) fn segments(tokens: &[String]) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for tok in tokens {
        if is_separator(tok) {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(tok.clone());
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// The command-position token of `segment`, its remaining argv, and the
/// basenames of the wrappers skipped on the way (change readers use them:
/// the pipe guard reads `timeout` here). `greedy` reads an UNLISTED wrapper
/// flag as value-taking (`caffeinate -t 3600 pytest`). Neither reading is
/// safe alone, so the caller reads BOTH and refuses when either lands on a
/// raw run.
pub(super) fn head_of(
    segment: &[String],
    greedy: bool,
) -> Option<(String, Vec<String>, Vec<String>)> {
    let mut i = 0;
    let mut saw_wrapper = "";
    let mut wrappers: Vec<String> = Vec::new();
    while i < segment.len() {
        let tok = &segment[i];
        if tok.bytes().all(|b| b"();<>|&\n".contains(&b)) {
            i += 1;
            continue;
        }
        let base = basename(tok);
        if TRANSPARENT.contains(&base) {
            wrappers.push(base.to_string());
            saw_wrapper = base;
            i += 1;
            if POSITIONAL_LEAD.contains(&base) {
                if let Some(next) = segment.get(i) {
                    if !next.starts_with('-') {
                        i += 1;
                    }
                }
            }
            continue;
        }
        // Only the NAME before the first '=' is validated; the value is free
        // text (`FOO=bar/baz` is an assignment, `https://x` is not).
        let is_assignment = match tok.split_once('=') {
            Some((name, _)) => {
                let mut ch = name.chars();
                matches!(ch.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
                    && ch.all(|c| c.is_ascii_alphanumeric() || c == '_')
            }
            None => false,
        };
        if is_assignment {
            i += 1;
            continue;
        }
        if !saw_wrapper.is_empty() && tok.starts_with('-') {
            // `command -v NAME` / `-V` is a lookup, not a run: the guard has
            // nothing to judge, and `P=$(command -v pytest)` is the common
            // idiom this extends to substitutions.
            if saw_wrapper == "command" && matches!(tok.as_str(), "-v" | "-V") {
                return None;
            }
            let takes_value = greedy || wrapper_takes_value(saw_wrapper, tok);
            i += if takes_value && i + 1 < segment.len() {
                2
            } else {
                1
            };
            continue;
        }
        return Some((base.to_string(), segment[i + 1..].to_vec(), wrappers));
    }
    None
}

/// The script text a shell was handed with -c, or None. Short options
/// bundle: `bash -lc '...'` is the same call as `-c`.
fn shell_payload(head: &str, argv: &[String]) -> Option<String> {
    if !SHELLS.contains(&head) {
        return None;
    }
    argv.iter()
        .enumerate()
        .find(|(_, t)| {
            t.starts_with('-')
                && t.len() >= 2
                && t[1..].ends_with('c')
                && t[1..][..t.len() - 1]
                    .bytes()
                    .all(|b| b.is_ascii_alphabetic())
        })
        .and_then(|(i, _)| argv.get(i + 1).cloned())
}

/// Split a segment on the pipeline operator: each stage is judged in its
/// own command position.
pub(super) fn stages(segment: &[String]) -> Vec<Vec<String>> {
    let mut parts: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for tok in segment {
        if tok == "|" {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
        } else {
            current.push(tok.clone());
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// (kind, command text) of the first raw run in command position, or None.
/// Read per pipeline stage, with both wrapper-flag readings. A `bash -c`
/// payload recurses as its own command text, bounded by depth. A pytest fed
/// to `xargs` is a known fail-open: it is not in this shell's command
/// position.
fn refused_segment(
    tokens: &[String],
    depth: usize,
    inherited_escape: bool,
) -> Option<(Kind, String)> {
    if depth > 2 {
        return None;
    }
    for segment in segments(tokens) {
        for part in stages(&segment) {
            // The whole-suite escape rides the stage text: a `WholeRust`
            // verdict is skipped when this stage carries the literal prefix
            // (or inherited one from a wrapper shell, whose env reaches the
            // payload), and the raw pytest/cargo refusals ignore the prefix.
            let full_escape = inherited_escape || part.iter().any(|t| t == "FNO_TEST_FULL=1");
            for reading in [head_of(&part, false), head_of(&part, true)] {
                let Some((head, argv, _wrappers)) = reading else {
                    continue;
                };
                if let Some(kind) = refused_head(&head, &argv) {
                    if kind == Kind::WholeRust && full_escape {
                        continue;
                    }
                    return Some((kind, part.join(" ")));
                }
                if let Some(payload) = shell_payload(&head, &argv) {
                    let Some(sub) = lex(&payload) else {
                        continue;
                    };
                    if let Some(found) = refused_segment(&sub, depth + 1, full_escape) {
                        return Some(found);
                    }
                }
            }
        }
    }
    None
}

fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .current_dir(cwd)
        .output()
        .ok()?;
    let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

/// True when root is a footnote checkout or worktree of one. Both markers
/// are tracked files, so every worktree carries them; a random other repo
/// the plugin happens to be active in is out of scope.
fn is_footnote_checkout(root: &Path) -> bool {
    root.join("cli").join("src").join("fno").is_dir()
        && root.join("hooks").join("hooks.json").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    /// A repo shaped like this one: cli/src/fno plus hooks/hooks.json.
    fn footnote_root() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("cli").join("src").join("fno")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("hooks")).expect("mkdir");
        std::fs::write(dir.path().join("hooks").join("hooks.json"), "{}").expect("write");
        dir
    }

    fn decide(cmd: &str, root: &Path) -> Option<String> {
        decide_at(cmd, Some(root))
    }

    #[test]
    fn uv_run_pytest_refused_names_doctor_test() {
        let root = footnote_root();
        let refusal = decide("uv run pytest cli/tests/unit/x.py", root.path());
        assert!(refusal.unwrap().contains("fno doctor test"));
    }

    #[test]
    fn bare_pytest_refused() {
        let root = footnote_root();
        assert!(decide("pytest -q cli/tests", root.path()).is_some());
    }

    #[test]
    fn full_path_pytest_refused() {
        let root = footnote_root();
        assert!(decide("/usr/bin/env pytest -q", root.path()).is_some());
    }

    #[test]
    fn python_m_pytest_refused() {
        let root = footnote_root();
        assert!(decide("python3 -m pytest -q", root.path()).is_some());
    }

    #[test]
    fn attached_dash_m_spelling_refused() {
        let root = footnote_root();
        assert!(decide("python -mpytest -q", root.path()).is_some());
    }

    #[test]
    fn env_prefix_refused() {
        let root = footnote_root();
        assert!(decide("FNO_DEBUG=1 pytest -q", root.path()).is_some());
    }

    #[test]
    fn timeout_wrapper_refused() {
        let root = footnote_root();
        assert!(decide("timeout 30 uv run pytest", root.path()).is_some());
    }

    #[test]
    fn uvx_and_uv_tool_run_refused() {
        let root = footnote_root();
        assert!(decide("uvx pytest", root.path()).is_some());
        assert!(decide("uv tool run pytest -q", root.path()).is_some());
    }

    #[test]
    fn uv_run_python_m_pytest_refused() {
        let root = footnote_root();
        assert!(decide("uv run python -m pytest -q", root.path()).is_some());
    }

    #[test]
    fn uvx_from_package_refused() {
        let root = footnote_root();
        assert!(decide("uvx --from pytest-uv pytest -q", root.path()).is_some());
    }

    #[test]
    fn uvx_from_other_package_allows() {
        let root = footnote_root();
        assert!(decide("uvx --from pytest-mock ruff check .", root.path()).is_none());
    }

    #[test]
    fn cargo_toolchain_test_refused() {
        let root = footnote_root();
        assert!(decide("cargo +nightly test", root.path()).is_some());
    }

    #[test]
    fn pipeline_stage_refused() {
        let root = footnote_root();
        assert!(decide("rg pattern sources/ | pytest -q", root.path()).is_some());
    }

    #[test]
    fn bash_c_payload_refused() {
        let root = footnote_root();
        assert!(decide("bash -lc 'cd cli && pytest -q'", root.path()).is_some());
    }

    #[test]
    fn cargo_test_refused_names_doctor_test_rust() {
        let root = footnote_root();
        let refusal = decide("cargo test -p fno", root.path());
        let refusal = refusal.unwrap();
        assert!(refusal.contains("fno doctor test rust"));
        assert!(refusal.contains("--manifest-path crates/<crate>/Cargo.toml"));
        assert!(refusal.contains("--lib"));
        assert!(refusal.contains("--test"));
    }

    #[test]
    fn cargo_test_behind_global_flags_refused() {
        let root = footnote_root();
        assert!(decide("cargo --manifest-path cli/Cargo.toml test", root.path()).is_some());
    }

    #[test]
    fn compound_refuses_only_the_test_half() {
        let root = footnote_root();
        let refusal = decide("cargo build && cargo test", root.path());
        assert!(refusal.unwrap().contains("cargo test"));
    }

    #[test]
    fn doctor_test_allows() {
        let root = footnote_root();
        assert!(decide("fno doctor test cli/tests/unit/x.py", root.path()).is_none());
        assert!(decide(
            "fno doctor test rust --manifest-path crates/fno-agents/Cargo.toml --lib test_run::",
            root.path()
        )
        .is_none());
    }

    #[test]
    fn cargo_build_allows() {
        let root = footnote_root();
        assert!(decide("cargo build --release", root.path()).is_none());
    }

    #[test]
    fn cargo_nextest_not_in_scope() {
        let root = footnote_root();
        assert!(decide("cargo nextest run", root.path()).is_none());
    }

    #[test]
    fn pytest_in_echo_string_allows() {
        let root = footnote_root();
        assert!(decide("echo \"pytest passed\"", root.path()).is_none());
    }

    #[test]
    fn pytest_in_quoted_heredoc_body_allows() {
        let root = footnote_root();
        let cmd = "cat > script.sh <<'EOF'\npytest -q\nEOF\nbash script.sh";
        assert!(decide(cmd, root.path()).is_none());
    }

    #[test]
    fn other_tool_through_uv_allows() {
        let root = footnote_root();
        assert!(decide("uv run ruff check hooks/", root.path()).is_none());
    }

    #[test]
    fn unbalanced_quotes_fail_open() {
        let root = footnote_root();
        assert!(decide("pytest -q 'unclosed", root.path()).is_none());
    }

    #[test]
    fn substitution_wrapped_test_run_refused() {
        // The capture spellings run the suite just the same; the body of a
        // substitution is one more command since the shared lexer changed.
        let root = footnote_root();
        assert!(decide("N=$(pytest -q)", root.path()).is_some());
        assert!(decide("OUT=\"$(uv run pytest -q 2>&1)\"", root.path()).is_some());
        assert!(decide("echo \"$(cargo test)\"", root.path()).is_some());
        assert!(decide("N=`pytest -q`", root.path()).is_some());
    }

    #[test]
    fn quoted_substitution_test_mention_allows() {
        // Quoted and heredoc-transported mentions are inert text.
        let root = footnote_root();
        let cmd = "git commit -m \"$(cat <<'EOF'\nrun pytest -q\nEOF\n)\"";
        assert!(decide(cmd, root.path()).is_none());
        assert!(decide("echo '$(pytest)'", root.path()).is_none());
    }

    #[test]
    fn command_v_lookup_allows() {
        // `command -v` prints where a name resolves and runs nothing.
        let root = footnote_root();
        assert!(decide("command -v pytest", root.path()).is_none());
        assert!(decide("command -V cargo", root.path()).is_none());
        assert!(decide("P=$(command -v pytest)", root.path()).is_none());
        assert!(decide("command pytest -q", root.path()).is_some());
    }

    #[test]
    fn or_list_pytest_refused() {
        // `||` ends a command: the right side is judged in its own command
        // position, never as an operand of the left segment.
        let root = footnote_root();
        let refusal = decide("false || pytest -q", root.path());
        assert!(refusal.unwrap().contains("fno doctor test"));
    }

    #[test]
    fn no_repo_allows() {
        assert!(decide_at("pytest", None).is_none());
    }

    #[test]
    fn non_footnote_repo_allows() {
        let dir = TempDir::new().expect("tempdir");
        assert!(decide("uv run pytest x.py", dir.path()).is_none());
    }

    #[test]
    fn quoted_semicolon_is_not_a_separator() {
        // The refusal bait lives inside a quoted string: splitting there
        // would refuse `git commit -m "..."` for mentioning pytest.
        let root = footnote_root();
        let cmd = "git commit -m \"fix; pytest noted next\"";
        assert!(decide(cmd, root.path()).is_none());
    }

    #[test]
    fn uv_value_flag_then_pytest_refused() {
        let root = footnote_root();
        assert!(decide("uv run --with pytest-xdist pytest -q", root.path()).is_some());
    }

    #[test]
    fn assignment_value_with_punctuation_refused() {
        let root = footnote_root();
        assert!(decide("FOO=bar/baz:1.0-x pytest -q", root.path()).is_some());
    }

    #[test]
    fn non_assignment_word_with_slash_allows() {
        let root = footnote_root();
        assert!(decide("echo https://example.com pytest", root.path()).is_none());
    }

    #[test]
    fn uv_group_flag_then_pytest_refused() {
        let root = footnote_root();
        assert!(decide("uv run --group dev pytest -q", root.path()).is_some());
    }

    #[test]
    fn cargo_color_flag_then_test_refused() {
        let root = footnote_root();
        assert!(decide("cargo --color always test", root.path()).is_some());
    }

    #[test]
    fn cargo_t_alias_refused() {
        let root = footnote_root();
        assert!(decide("cargo t", root.path()).is_some());
    }

    #[test]
    fn cargo_b_alias_allows() {
        let root = footnote_root();
        assert!(decide("cargo b --release", root.path()).is_none());
    }

    #[test]
    fn py_test_entry_point_refused() {
        let root = footnote_root();
        assert!(decide("py.test -q", root.path()).is_some());
    }

    #[test]
    fn nextest_after_typed_flag_allows() {
        // `cargo nextest run` never spells the word test as a bare token.
        let root = footnote_root();
        assert!(decide("cargo nextest run", root.path()).is_none());
    }

    /// AC9-HP, AC10-HP: a whole crate suite is refused through both blessed
    /// doors, in every transport-flag spelling, and names the narrowest
    /// target plus the escape prefix.
    #[test]
    fn whole_rust_suite_refused_at_both_doors() {
        let root = footnote_root();
        let refusal = decide("fno doctor test rust", root.path()).expect("bare rust refuses");
        assert!(refusal.contains("--lib"), "{refusal}");
        assert!(refusal.contains("--test"), "{refusal}");
        assert!(refusal.contains("CI"), "{refusal}");
        assert!(refusal.contains("FNO_TEST_FULL=1"), "{refusal}");
        assert!(
            decide(
                "fno doctor test --stream rust --manifest-path crates/fno/Cargo.toml",
                root.path()
            )
            .is_some(),
            "--stream before rust refuses"
        );
        assert!(
            decide(
                "fno doctor test --log=/tmp/x.log rust --manifest-path crates/fno/Cargo.toml",
                root.path()
            )
            .is_some(),
            "--log= before rust refuses"
        );
        assert!(
            decide(
                "fno-agents test-run --timeout 1800 -- cargo test -q --manifest-path crates/fno/Cargo.toml -- --test-threads 4",
                root.path()
            )
            .is_some(),
            "the direct owner door refuses a whole suite"
        );
    }

    /// The transport flags `--stream` and `--log` are dropped wherever they
    /// sit, so `rust` is found behind them and its cargo tail classified.
    #[test]
    fn doctor_test_transport_flags_are_dropped() {
        let root = footnote_root();
        assert!(
            decide(
                "fno doctor test rust --manifest-path crates/fno/Cargo.toml --stream",
                root.path()
            )
            .is_some(),
            "a trailing --stream never hides the whole-suite run"
        );
        assert!(
            decide(
                "fno doctor test --log /tmp/x.log rust --manifest-path crates/fno/Cargo.toml",
                root.path()
            )
            .is_some(),
            "a leading --log <path> never hides the whole-suite run"
        );
    }

    /// AC11-EDGE: the commands the refusal teaches are never themselves
    /// refused, at either door.
    #[test]
    fn targeted_rust_runs_allow_at_both_doors() {
        let root = footnote_root();
        assert!(decide(
            "fno doctor test rust --manifest-path crates/fno-agents/Cargo.toml --lib test_run::",
            root.path()
        )
        .is_none());
        assert!(decide(
            "fno doctor test rust --manifest-path crates/fno-agents/Cargo.toml --test test_run_lifecycle",
            root.path()
        ).is_none());
        assert!(decide(
            "fno-agents test-run -- cargo test --manifest-path crates/fno-agents/Cargo.toml --lib -- session_activity",
            root.path()
        ).is_none());
    }

    /// AC12-EDGE: the escape prefix opens the whole-suite door, a non-
    /// footnote repository is out of scope, and a raw cargo run with the
    /// prefix is still refused.
    #[test]
    fn whole_rust_suite_with_the_prefix_allows() {
        let root = footnote_root();
        assert!(
            decide("FNO_TEST_FULL=1 fno doctor test rust", root.path()).is_none(),
            "the prefix opens the blessed door"
        );
        assert!(
            decide(
                "FNO_TEST_FULL=1 bash -c 'fno doctor test rust'",
                root.path()
            )
            .is_none(),
            "the prefix on a wrapper shell reaches the payload"
        );
        let other = TempDir::new().expect("tempdir");
        assert!(
            decide("FNO_TEST_FULL=1 fno doctor test rust", other.path()).is_none(),
            "a non-footnote repository allows"
        );
        assert!(
            decide("FNO_TEST_FULL=1 cargo test -q", root.path()).is_some(),
            "the prefix never opens the raw cargo door"
        );
    }

    /// AC13-HP: the pytest refusal names a test-file target and the bare
    /// tree cost; the cargo refusal names the narrowest rust form.
    #[test]
    fn pytest_refusal_names_a_test_file_target() {
        let root = footnote_root();
        let refusal = decide("pytest -q", root.path()).expect("pytest refuses");
        assert!(refusal.contains("fno doctor test cli/tests/"), "{refusal}");
        assert!(refusal.contains("test_x.py"), "{refusal}");
        assert!(refusal.contains("whole cli/tests tree"), "{refusal}");
        let refusal = decide("cargo test -p fno-agents foo", root.path())
            .expect("cargo with a filter refuses");
        assert!(
            refusal.contains("--manifest-path crates/<crate>/Cargo.toml"),
            "{refusal}"
        );
        assert!(refusal.contains("--lib"), "{refusal}");
        assert!(refusal.contains("--test"), "{refusal}");
    }
}
