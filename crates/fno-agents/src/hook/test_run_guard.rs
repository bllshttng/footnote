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
//! `timeout 30 cargo test`), env-assignment prefixes, full paths, and one
//! level of `bash -c` payloads. The other uv doors are covered because they
//! are the same raw run: `uv run pytest`, `uvx pytest`, `uv tool run
//! pytest`, and `python -m pytest` in both `-m` spellings.
//!
//! Fails OPEN on anything unexpected: a guard that breaks a session on its
//! own bug is worse than the runs it prevents. Scoped to footnote checkouts
//! (`cli/src/fno` plus `hooks/hooks.json` at the repo root); every other
//! repository allows. Scripts that call pytest internally, and pytest fed
//! to `xargs`, are known fail-opens, the accepted shape for this class of
//! guard. Shell shim: `hooks/test-run-guard.sh`.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::king_guard::lex;

/// Wrappers that are transparent to command position: `sudo pytest ...`
/// still runs the pytest. Shell keywords that open a body are here too, so
/// `do pytest ...; done` inside a `for` resolves to the pytest.
const TRANSPARENT: &[&str] = &[
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
const POSITIONAL_LEAD: &[&str] = &["timeout", "gtimeout"];

/// Wrapper flags that swallow the next token, so the value is not mistaken
/// for the command (`sudo -u me pytest ...`). Read PER WRAPPER.
fn wrapper_takes_value(wrapper: &str, flag: &str) -> bool {
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

/// Cargo global flags whose NEXT token is a value, never the subcommand.
const CARGO_VALUE_FLAGS: &[&str] = &[
    "-C",
    "--config",
    "-Z",
    "--manifest-path",
    "--target",
    "-j",
    "--jobs",
];

const PYTEST_REASON: &str = "[fno test-run guard] `{cmd}` runs pytest outside the suite admission. A raw suite takes no test:suite slot, waits behind no live cargo build, and imports whichever fno is first on PYTHONPATH - on this machine two raw suites at once are the measured crush behind this guard.\n\nRun `fno doctor test [paths...]` instead: it takes the test:suite claim, holds while another suite or cargo build is live, and pins PYTHONPATH to this worktree.";

const CARGO_REASON: &str = "[fno test-run guard] `{cmd}` runs the crates suite unadmitted. Raw `cargo test` takes no test:suite slot and waits behind no live cargo build.\n\nRun `fno doctor test rust` instead: it admits the crates suite under the same claim and bounds it to one run on this machine.";

/// Which blessed door the refusal names.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Pytest,
    Cargo,
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
        emit_telemetry(&cwd, false);
        return allow("no-repo");
    };

    match decide_at(cmd, Some(&root)) {
        Some(reason) => {
            emit_telemetry(&cwd, true);
            super::emit_block(&reason)
        }
        None => {
            emit_telemetry(&cwd, false);
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
    let (kind, shown) = refused_segment(&tokens, 0)?;
    let root = root?;
    if !is_footnote_checkout(root) {
        return None;
    }
    let reason = match kind {
        Kind::Pytest => PYTEST_REASON,
        Kind::Cargo => CARGO_REASON,
    };
    Some(reason.replace("{cmd}", &shown))
}

fn basename(tok: &str) -> &str {
    tok.rsplit('/').next().unwrap_or(tok)
}

fn is_python(name: &str) -> bool {
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
fn has_dash_m_module(argv: &[String], target: &str) -> bool {
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

/// (basename, rest) of the command `uv run ...` or `uv tool run ...`
/// dispatches. rest is the argv AFTER the dispatched command, so a
/// `python -m pytest` behind `uv run python` is still visible.
fn uv_target(argv: &[String]) -> (Option<String>, Vec<String>) {
    let Some(i) = first_positional(argv, UV_VALUE_FLAGS) else {
        return (None, Vec::new());
    };
    let sub = basename(&argv[i]);
    let mut rest: Vec<String> = argv[i + 1..].to_vec();
    let sub = if sub == "tool" {
        let Some(j) = first_positional(&rest, UV_VALUE_FLAGS) else {
            return (Some(sub.to_string()), Vec::new());
        };
        let s = basename(&rest[j]).to_string();
        rest = rest[j + 1..].to_vec();
        s
    } else {
        sub.to_string()
    };
    if sub != "run" {
        return (Some(sub), rest);
    }
    match first_positional(&rest, UV_VALUE_FLAGS) {
        Some(m) => (Some(basename(&rest[m]).to_string()), rest[m + 1..].to_vec()),
        None => (Some("run".to_string()), Vec::new()),
    }
}

/// (basename, rest) for `uvx ...` (uv tool run shorthand).
fn uvx_target(argv: &[String]) -> (Option<String>, Vec<String>) {
    match first_positional(argv, UV_VALUE_FLAGS) {
        Some(m) => (Some(basename(&argv[m]).to_string()), argv[m + 1..].to_vec()),
        None => (None, Vec::new()),
    }
}

/// The first positional of a cargo invocation: `cargo -C wt test` -> test.
/// A `+toolchain` token is neither a flag nor the subcommand.
fn cargo_subcommand(argv: &[String]) -> Option<String> {
    let mut skip = false;
    for tok in argv {
        if skip {
            skip = false;
            continue;
        }
        if tok == "--" {
            return None;
        }
        if tok.starts_with('+') {
            continue;
        }
        if tok.starts_with('-') && tok.len() > 1 {
            skip = CARGO_VALUE_FLAGS.contains(&tok.as_str());
            continue;
        }
        return Some(basename(tok).to_string());
    }
    None
}

/// Which raw run sits at this command position, if any.
fn refused_head(head: &str, argv: &[String]) -> Option<Kind> {
    if head == "pytest" {
        return Some(Kind::Pytest);
    }
    if is_python(head) && has_dash_m_module(argv, "pytest") {
        return Some(Kind::Pytest);
    }
    if head == "uv" {
        let (target, rest) = uv_target(argv);
        if target.as_deref() == Some("pytest") {
            return Some(Kind::Pytest);
        }
        if target.map(|t| is_python(&t)).unwrap_or(false) && has_dash_m_module(&rest, "pytest") {
            return Some(Kind::Pytest);
        }
    }
    if head == "uvx" {
        let (target, _) = uvx_target(argv);
        if target.as_deref() == Some("pytest") {
            return Some(Kind::Pytest);
        }
    }
    if head == "cargo" && cargo_subcommand(argv).as_deref() == Some("test") {
        return Some(Kind::Cargo);
    }
    None
}

/// True when this token ends one command and starts the next. Redirect
/// machinery (`2>&` as one token, `|&`) never separates.
fn is_separator(tok: &str) -> bool {
    matches!(tok, ";" | ";;" | "&" | "&&" | "\n" | ")")
}

/// Split a token list on command separators.
fn segments(tokens: &[String]) -> Vec<Vec<String>> {
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

/// The command-position token of `segment`, plus its remaining argv.
/// `greedy` reads an UNLISTED wrapper flag as value-taking (`caffeinate -t
/// 3600 pytest`). Neither reading is safe alone, so the caller reads BOTH
/// and refuses when either lands on a raw run.
fn head_of(segment: &[String], greedy: bool) -> Option<(String, Vec<String>)> {
    let mut i = 0;
    let mut saw_wrapper = "";
    while i < segment.len() {
        let tok = &segment[i];
        if tok.bytes().all(|b| b"();<>|&\n".contains(&b)) {
            i += 1;
            continue;
        }
        let base = basename(tok);
        if TRANSPARENT.contains(&base) {
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
        let is_assignment = {
            let mut ch = tok.chars();
            matches!(ch.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
                && ch.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '=')
                && tok.contains('=')
        };
        if is_assignment {
            i += 1;
            continue;
        }
        if !saw_wrapper.is_empty() && tok.starts_with('-') {
            let takes_value = greedy || wrapper_takes_value(saw_wrapper, tok);
            i += if takes_value && i + 1 < segment.len() {
                2
            } else {
                1
            };
            continue;
        }
        return Some((base.to_string(), segment[i + 1..].to_vec()));
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

/// (kind, command text) of the first raw run in command position, or None.
/// Read per pipeline stage, with both wrapper-flag readings. A `bash -c`
/// payload recurses as its own command text, bounded by depth. A pytest fed
/// to `xargs` is a known fail-open: it is not in this shell's command
/// position.
fn refused_segment(tokens: &[String], depth: usize) -> Option<(Kind, String)> {
    if depth > 2 {
        return None;
    }
    for segment in segments(tokens) {
        // Split the segment on the pipeline operator: each stage is judged
        // in its own command position.
        let mut parts: Vec<Vec<String>> = Vec::new();
        let mut current: Vec<String> = Vec::new();
        for tok in &segment {
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
        for part in &parts {
            for reading in [head_of(part, false), head_of(part, true)] {
                let Some((head, argv)) = reading else {
                    continue;
                };
                if let Some(kind) = refused_head(&head, &argv) {
                    return Some((kind, part.join(" ")));
                }
                if let Some(payload) = shell_payload(&head, &argv) {
                    let Some(sub) = lex(&payload) else {
                        continue;
                    };
                    if let Some(found) = refused_segment(&sub, depth + 1) {
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

/// One `guard_decision` row into the space events file, the bounded
/// appender `emit_to_both` uses (as the king guard does).
fn emit_telemetry(cwd: &Path, denied: bool) {
    let path = crate::paths::events_path(cwd);
    let event = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "type": "guard_decision",
        "data": {"guard": "test-run-guard", "decision": if denied { "block" } else { "allow" }, "tool": "Bash"},
        "source": "hook"
    });
    let _ = crate::claims::append_event_line(&path, &event, Duration::from_secs(2));
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
        assert!(refusal.unwrap().contains("fno doctor test rust"));
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
        assert!(decide("fno doctor test rust", root.path()).is_none());
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
}
