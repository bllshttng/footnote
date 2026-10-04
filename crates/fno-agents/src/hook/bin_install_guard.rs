//! `fno-agents hook bin-install-guard` - refuse a Bash call that copies a
//! locally built fno-agents or fno-agents-worker onto the deployed copy in
//! the cargo bin dir (`cp`, `mv`, `install`, `ditto`, `rsync`).
//!
//! One reading, one refusal: the destination (last positional, or `-t` /
//! `--target-directory`) lands on the deployed binary's name inside the
//! cargo bin dir. The trap is real: on 2026-09-26 19:45:34Z a worker
//! replayed the CI verb-matrix step
//! (`cp target/debug/fno-agents "$HOME/.cargo/bin/"`) locally, the debug
//! build replaced the live binary in place, and macOS SIGKILLed every
//! launch fleet-wide (exit 137) until the file was swapped to a fresh
//! inode. The refusal names the two safe doors: pin a local build with
//! `FNO_AGENTS_FRONT`, or deploy for real with `fno doctor update`.
//!
//! Parse-only like the pipe guard: no subprocess, and it fails OPEN on
//! anything it cannot resolve (no HOME, a relative destination, a `cd`
//! into the bin dir before the copy). Shim: `hooks/bin-install-guard.sh`.

use serde_json::Value;
use std::path::PathBuf;

use super::lead_guard::lex;
use super::test_run_guard::{basename, head_of, stages};

const REASON: &str = "[fno bin-install guard] `{cmd}` writes a locally built binary onto the deployed fno-agents or fno-agents-worker in the cargo bin dir. On 2026-09-26 19:45:34Z a worker replayed the CI verb-matrix step (`cp target/debug/fno-agents \"$HOME/.cargo/bin/\"`) locally; the debug build replaced the live binary in place and macOS then SIGKILLed every launch fleet-wide (exit 137) until the file was swapped to a fresh inode. For a local run, build in your checkout and pin it: `export FNO_AGENTS_FRONT=\"$PWD/crates/fno-agents/target/debug/fno-agents\"` - readers resolve through it and the deployed copy is untouched. A real deploy goes through `fno doctor update`.";

/// The copy family whose last positional (or `-t`) is a destination.
const COPY_HEADS: &[&str] = &["cp", "mv", "install", "ditto", "rsync"];

/// The deployed binaries the guard protects, by basename.
const BIN_NAMES: &[&str] = &["fno-agents", "fno-agents-worker"];

/// Flags whose next token is a value, so its `755`/`root`/`ssh` never reads
/// as a positional. `-t`/`--target-directory` are handled separately: they
/// ARE the destination for cp/mv/install.
const VALUE_FLAGS: &[&str] = &[
    "-m",
    "-o",
    "-g",
    "-e",
    "-b",
    "--rsync-path",
    "--password-file",
    "--temp-dir",
];

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
    // The guardrail preset owns whether this guard runs at all.
    if !crate::agents_config::guard_enabled(&cwd, "bin-install") {
        return super::emit_allow();
    }
    let refusal = judge(&payload);
    super::emit_guard_decision(&cwd, "bin-install-guard", "Bash", refusal.is_some());
    match refusal {
        Some(reason) => super::emit_block(&reason),
        None => super::emit_allow(),
    }
}

/// The whole verdict for one payload: a refusal, or None to allow. A null
/// payload, a non-Bash tool and a blank command allow.
pub(super) fn judge(payload: &Value) -> Option<String> {
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

/// The whole verdict for one command string: a refusal, or None to allow.
/// The env-reading production path; `decide_in` is the pure core the tests
/// drive (the test runner neutralises HOME and pins CARGO_HOME to a
/// non-HOME sandbox, so the two env values never agree in a test child).
fn decide(command: &str) -> Option<String> {
    let home = std::env::var("HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())?;
    let bin_dir = cargo_bin_dir(&home);
    decide_in(&home, &bin_dir, command)
}

fn decide_in(home: &str, bin_dir: &str, command: &str) -> Option<String> {
    let toks = lex(command)?;
    let mut seg: Vec<String> = Vec::new();
    for tok in toks {
        // Segments split on the full separator set, not just `;`: the
        // incident rode `cargo build ... && cp ...`, and a `cp` behind `&&`
        // is still in command position.
        if matches!(tok.as_str(), ";" | ";;" | "&" | "&&" | "||" | "\n") {
            if let Some(r) = judge_seg(home, bin_dir, &seg) {
                return Some(r);
            }
            seg.clear();
        } else {
            seg.push(tok);
        }
    }
    judge_seg(home, bin_dir, &seg)
}

fn judge_seg(home: &str, bin_dir: &str, seg: &[String]) -> Option<String> {
    for stage in stages(seg) {
        for reading in [head_of(&stage, false), head_of(&stage, true)] {
            if let Some((head, argv, _)) = reading {
                if COPY_HEADS.contains(&head.as_str())
                    && refusal_for(home, bin_dir, &head, &argv).is_some()
                {
                    return Some(REASON.replace("{cmd}", &seg.join(" ")));
                }
                // `cargo install` writes the built binary straight into the
                // cargo bin dir: the same in-place overwrite through a head
                // the copy family does not name.
                if head == "cargo"
                    && argv.first().map(String::as_str) == Some("install")
                    && cargo_install_refusal(&argv[1..]).is_some()
                {
                    return Some(REASON.replace("{cmd}", &seg.join(" ")));
                }
            }
        }
    }
    None
}

/// Flags of `cargo install` whose next token is its value, so the value can
/// be matched (`--path crates/fno-agents`, `--bin fno-agents`) instead of
/// read as a package-name positional.
const CARGO_VALUE_FLAGS: &[&str] = &[
    "--path",
    "-p",
    "--package",
    "--bin",
    "--git",
    "--index",
    "--registry",
    "--target",
];

/// The refusal for one `cargo install` argv (past the `install` token), or
/// None to allow: the command must name one of the deployed binaries as the
/// thing it installs.
fn cargo_install_refusal(argv: &[String]) -> Option<()> {
    let mut i = 0;
    while i < argv.len() {
        let tok = &argv[i];
        let (flag, glued) = match tok.split_once('=') {
            Some((f, v)) => (f, Some(v)),
            None => (tok.as_str(), None),
        };
        if glued.is_none() && !tok.starts_with('-') && BIN_NAMES.contains(&tok.as_str()) {
            return Some(());
        }
        let value = glued.map(str::to_string).or_else(|| {
            argv.get(i + 1)
                .filter(|_| CARGO_VALUE_FLAGS.contains(&flag))
                .cloned()
        });
        if let Some(v) = value {
            let v = v.trim_end_matches('/');
            if BIN_NAMES.contains(&v.to_string().as_str())
                || v.ends_with("/fno-agents")
                || v.ends_with("/fno-agents-worker")
            {
                return Some(());
            }
        }
        if glued.is_none() && CARGO_VALUE_FLAGS.contains(&flag) {
            i += 1;
        }
        i += 1;
    }
    None
}
/// The destination token and the source positionals of one copy-family
/// argv. `-t DIR` / `--target-directory[=]DIR` IS the destination for
/// cp/mv/install (rsync reads `-t` as the times modifier, so its `-t` is a
/// plain flag there and the destination falls to last-positional).
fn dest_of(head: &str, argv: &[String]) -> (Option<String>, Vec<String>) {
    let takes_t = matches!(head, "cp" | "mv" | "install");
    let mut positional: Vec<String> = Vec::new();
    let mut t_dir: Option<String> = None;
    let mut i = 0;
    while i < argv.len() {
        let tok = &argv[i];
        if let Some(v) = tok.strip_prefix("--target-directory=") {
            t_dir = Some(v.to_string());
        } else if takes_t && (tok == "--target-directory" || tok == "-t") {
            if let Some(v) = argv.get(i + 1) {
                t_dir = Some(v.clone());
                i += 1;
            }
        } else if tok.starts_with('-') && tok.len() > 1 {
            if VALUE_FLAGS.contains(&tok.as_str()) {
                i += 1;
            }
        } else {
            positional.push(tok.clone());
        }
        i += 1;
    }
    (t_dir.or_else(|| positional.pop()), positional)
}

/// The cargo bin dir the deployed binaries live in: `$CARGO_HOME/bin` when
/// CARGO_HOME is set, else `$HOME/.cargo/bin`.
fn cargo_bin_dir(home: &str) -> String {
    if let Ok(ch) = std::env::var("CARGO_HOME") {
        if !ch.trim().is_empty() {
            return format!("{}/bin", ch.trim_end_matches('/'));
        }
    }
    format!("{}/.cargo/bin", home.trim_end_matches('/'))
}

/// `~`, `$HOME` and `${HOME}` prefixes resolve against HOME; anything else
/// passes through. The lexer delivers quotes stripped, so the incident's
/// `"$HOME/.cargo/bin/"` arrives as `$HOME/.cargo/bin/`.
fn expand_home(home: &str, tok: &str) -> String {
    for pre in ["~", "$HOME", "${HOME}"] {
        if tok == pre {
            return home.to_string();
        }
        if let Some(rest) = tok.strip_prefix(&format!("{pre}/")) {
            return format!("{home}/{rest}");
        }
    }
    tok.to_string()
}

/// The refusal for one copy-family argv, or None to allow: the destination
/// must sit at the deployed binaries' names inside the cargo bin dir.
fn refusal_for(home: &str, bin_dir: &str, head: &str, argv: &[String]) -> Option<()> {
    let (dest, sources) = dest_of(head, argv);
    let dest = expand_home(home, &dest?);
    let dest_n = dest.trim_end_matches('/');
    if dest_n == bin_dir {
        // Dir form: one of the sources must be one of the protected names.
        return sources
            .iter()
            .any(|s| BIN_NAMES.contains(&basename(s)))
            .then_some(());
    }
    let prefix = format!("{bin_dir}/");
    if dest_n.starts_with(&prefix) {
        // A trailing `*` is a shell glob the shell expands to the live name
        // before exec, so `~/.cargo/bin/fno-agents*` IS the binary.
        let name = basename(dest_n);
        let name = name.strip_suffix('*').unwrap_or(name);
        if BIN_NAMES.contains(&name) {
            return Some(());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_HOME: &str = "/u/tester";
    const BIN_DIR: &str = "/u/tester/.cargo/bin";

    /// The pure core the tests drive: the production `decide` reads HOME and
    /// CARGO_HOME, and the test runner pins those to a sandbox where the two
    /// never agree.
    fn denied(cmd: &str) -> String {
        decide_in(TEST_HOME, BIN_DIR, cmd).unwrap_or_else(|| panic!("expected a refusal: {cmd}"))
    }

    fn allowed(cmd: &str) {
        assert!(
            decide_in(TEST_HOME, BIN_DIR, cmd).is_none(),
            "expected allow: {cmd}"
        );
    }

    #[test]
    fn null_payload_and_non_bash_allow() {
        assert!(judge(&Value::Null).is_none());
        assert!(judge(
            &serde_json::json!({"tool_name": "Glob", "tool_input": {"command": "cp x ~/.cargo/bin/"}})
        )
        .is_none());
    }

    #[test]
    fn incident_command_denies() {
        // The exact 19:45:34Z replay, chained form.
        denied("cargo build --manifest-path crates/fno-agents/Cargo.toml --bin fno-agents && cp crates/fno-agents/target/debug/fno-agents \"$HOME/.cargo/bin/\"");
        // Bare form, trailing slash and tilde spellings.
        denied("cp target/debug/fno-agents \"$HOME/.cargo/bin/\"");
        denied("cp target/debug/fno-agents ~/.cargo/bin/");
        denied("mv target/debug/fno-agents ~/.cargo/bin/fno-agents");
        denied("install -m 755 target/debug/fno-agents ~/.cargo/bin/fno-agents");
        denied("ditto target/debug/fno-agents ~/.cargo/bin/fno-agents");
        denied("rsync -a target/debug/fno-agents \"$HOME/.cargo/bin/\"");
        denied("cp -t \"$HOME/.cargo/bin\" target/debug/fno-agents");
        denied("env cp target/debug/fno-agents ~/.cargo/bin/");
        // A glob the shell expands to the live binary name is still the
        // binary.
        denied("cp x ~/.cargo/bin/fno-agents*");
        let r = denied("cp x ~/.cargo/bin/fno-agents-worker");
        assert!(r.contains("FNO_AGENTS_FRONT"), "names the pin door: {r}");
        assert!(
            r.contains("fno doctor update"),
            "names the deploy door: {r}"
        );
    }

    #[test]
    fn cargo_install_denies() {
        denied("cargo install --path crates/fno-agents");
        denied("cargo install --path crates/fno-agents --locked");
        denied("cargo install fno-agents");
        denied("cargo install fno-agents-worker");
        denied("cargo install --bin fno-agents --path crates/fno");
        denied("cargo install --package=fno-agents");
        // A repair rename or a different crate still installs.
        allowed("cargo install --list");
        allowed("cargo install sd");
        allowed("cargo install --path crates/fno");
        allowed("cargo install fno");
    }

    #[test]
    fn everyday_copies_allow() {
        allowed("cp target/debug/fno-agents \"$CLAUDE_JOB_DIR/tmp/\"");
        allowed("cp README.md /tmp/");
        allowed("rsync -a src/ dst/");
        // A repair move renames the deployed copy away; it is not the trap.
        allowed("mv ~/.cargo/bin/fno-agents ~/.cargo/bin/fno-agents.bak");
        // Same dir, a different name: not one of the protected binaries.
        allowed("cp x ~/.cargo/bin/other-tool");
        // mkdir/cd do not write the binary; a copy elsewhere is fine.
        allowed("install -d ~/.cargo/bin");
        allowed("cd ~/.cargo/bin && cp x /tmp/fno-agents");
        // Prose, never command position.
        allowed("echo cp x ~/.cargo/bin/");
    }
}
