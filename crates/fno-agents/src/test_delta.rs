//! Branch-local test-count changes for the PR description.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Default, Debug, PartialEq, Eq)]
struct LanguageDelta {
    added: u64,
    removed: u64,
}

#[derive(Default, Debug, PartialEq, Eq)]
struct TestDelta {
    python: LanguageDelta,
    rust: LanguageDelta,
    shell: LanguageDelta,
}

impl TestDelta {
    fn from_diff(diff: &str) -> Self {
        let mut result = Self::default();
        // The `+++ b/` header of the hunk in flight, so a matcher only sees
        // its own language: a `# T1` comment in a Python file is prose, a
        // `test_`-prefixed function in Rust is not a shell declaration.
        let mut ext = "";
        for line in diff.lines() {
            if let Some(path) = line.strip_prefix("+++ b/") {
                ext = path.rsplit('.').next().unwrap_or("");
                continue;
            }
            let (added, source) = match line.as_bytes().first() {
                Some(b'+') if !line.starts_with("+++") => (true, &line[1..]),
                Some(b'-') if !line.starts_with("---") => (false, &line[1..]),
                _ => continue,
            };
            let code = source.trim_start();
            let delta = if ext == "py"
                && (code.starts_with("def test_") || code.starts_with("async def test_"))
            {
                Some(&mut result.python)
            } else if ext == "rs" && (code.trim() == "#[test]" || code.starts_with("#[tokio::test"))
            {
                Some(&mut result.rust)
            } else if ext == "sh"
                && (code.starts_with("@test ")
                    || code.starts_with("test_") && code.contains("() {")
                    || is_shell_case_declaration(code))
            {
                Some(&mut result.shell)
            } else {
                None
            };
            if let Some(delta) = delta {
                if added {
                    delta.added += 1;
                } else {
                    delta.removed += 1;
                }
            }
        }
        result
    }

    fn markdown(&self) -> String {
        let added = self.python.added + self.rust.added + self.shell.added;
        let removed = self.python.removed + self.rust.removed + self.shell.removed;
        format!(
            "| Language | Added | Removed | Net |\n|---|---:|---:|---:|\n| Python | {} | {} | {} |\n| Rust | {} | {} | {} |\n| Shell | {} | {} | {} |\n| **Total** | **{}** | **{}** | **{:+}** |",
            self.python.added,
            self.python.removed,
            net(&self.python),
            self.rust.added,
            self.rust.removed,
            net(&self.rust),
            self.shell.added,
            self.shell.removed,
            net(&self.shell),
            added,
            removed,
            added as i64 - removed as i64,
        )
    }
}

fn net(delta: &LanguageDelta) -> i64 {
    delta.added as i64 - delta.removed as i64
}

/// The second shell declaration dialect: a `# T<digits>` case header. The
/// repo's shell suites declare cases as numbered headers instead of (or
/// beside) `test_...() {` functions, so a suite ported off shell was
/// invisible to this counter and every shell-to-Rust port read as net
/// growth. Matches `# T1: ...`, `# T13 (x-f209): ...` and a bare `# T1`.
fn is_shell_case_declaration(code: &str) -> bool {
    let b = code.as_bytes();
    b.len() > 3 && b.starts_with(b"# T") && b[3].is_ascii_digit()
}

/// Net test declarations across all three census languages. Shell counts on
/// both sides since the case-header matcher: a shell suite ported to Rust
/// nets to its true delta instead of reading as growth (a port is not
/// growth), so a port's deletions pay for its additions like any other
/// language's.
fn over_cap(delta: &TestDelta, cap: i64) -> Option<i64> {
    let net = net(&delta.python) + net(&delta.rust) + net(&delta.shell);
    (net > cap).then_some(net)
}

fn diff_range(git_bin: &str, dir: &Path, range: &str) -> Result<String, String> {
    let output = Command::new(git_bin)
        .args(["diff", "--unified=0", range, "--", "*.py", "*.rs", "*.sh"])
        .current_dir(dir)
        .output()
        .map_err(|err| format!("could not run git diff: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "git diff failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Why the shrink-only gate stopped the push: the delta rose over the cap
/// (a refusal, the worker fixes the tree), or the delta could not be read
/// (an infra failure, not a refusal).
#[derive(Debug, PartialEq, Eq)]
pub enum ShrinkGate {
    OverCap(String),
    Diff(String),
}

/// The `[test] max_net_new` cap, read through the same worktree ->
/// canonical -> global candidate chain the other config readers use, so a
/// linked feature worktree finds a cap that lives only in the canonical
/// checkout's project config. Unset (or unparseable) means no cap: adding
/// tests is the normal state of a repo that never opted into one.
pub fn max_net_new(dir: &Path) -> Option<i64> {
    let mut roots: Vec<PathBuf> = vec![crate::paths::worktree_repo_root(dir)];
    if let Some(canonical) = crate::paths::canonical_repo_root(dir) {
        roots.push(canonical);
    }
    let mut candidates: Vec<PathBuf> = roots
        .into_iter()
        .map(|root| root.join(".fno").join("config.toml"))
        .collect();
    if let Some(home) = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|h| Path::new(&h).join(".fno").join("config.toml"))
    {
        candidates.push(home);
    }
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(parsed) = toml::from_str::<toml::Value>(&text) else {
            continue;
        };
        if let Some(cap) = parsed
            .get("test")
            .and_then(|t| t.get("max_net_new"))
            .and_then(toml::Value::as_integer)
        {
            return Some(cap);
        }
    }
    None
}

/// The test cap gate the push path shares with CI. The cap is the caller's
/// resolved `[tests] max_net_new` (`None` = no cap: the diff is not even
/// read). The OverCap message carries the table so a worker fixes the delta
/// locally instead of learning the cap from a red main one full round later.
/// The caller's git binary rides in: the push path passes its configured
/// `--git-bin`, so a stubbed environment stays coherent.
pub fn shrink_only_gate(
    git_bin: &str,
    dir: &Path,
    base: &str,
    cap: Option<i64>,
) -> Result<(), ShrinkGate> {
    let Some(cap) = cap else {
        return Ok(());
    };
    let range = format!("{base}...HEAD");
    let diff = diff_range(git_bin, dir, &range).map_err(ShrinkGate::Diff)?;
    let delta = TestDelta::from_diff(&diff);
    if let Some(net) = over_cap(&delta, cap) {
        return Err(ShrinkGate::OverCap(format!(
            "net {net:+} test declarations against {base} exceeds the cap {cap} \
             ([test] max_net_new in .fno/config.toml). Delete a test that guards \
             no contract of its own:\n{}",
            delta.markdown()
        )));
    }
    Ok(())
}

/// `fno-agents test-delta --base <ref>` prints test declaration changes on
/// the current branch. Python test functions, Rust test attributes, and shell
/// test declarations are counted from the committed merge-base diff. The cap
/// is `--max-net`, else the configured `[tests] max_net_new` (unset = no
/// cap).
pub fn run_test_delta(args: &[String]) -> i32 {
    let Some(base_pos) = args.iter().position(|arg| arg == "--base") else {
        eprintln!("usage: fno-agents test-delta --base <ref> [--max-net <n>]");
        return 2;
    };
    let Some(base) = args.get(base_pos + 1).filter(|base| !base.starts_with('-')) else {
        eprintln!("test-delta: --base requires a ref");
        return 2;
    };
    let cap = match args.iter().position(|arg| arg == "--max-net") {
        Some(pos) => match args
            .get(pos + 1)
            .and_then(|value| value.parse::<i64>().ok())
        {
            Some(cap) => Some(cap),
            None => {
                eprintln!("usage: fno-agents test-delta --base <ref> [--max-net <n>]");
                return 2;
            }
        },
        None => max_net_new(Path::new(".")),
    };
    let range = format!("{base}...HEAD");
    let diff = match diff_range("git", Path::new("."), &range) {
        Ok(diff) => diff,
        Err(msg) => {
            eprintln!("test-delta: {msg}");
            return 1;
        }
    };
    let delta = TestDelta::from_diff(&diff);
    println!("{}", delta.markdown());
    if let Some(cap) = cap {
        if let Some(net) = over_cap(&delta, cap) {
            eprintln!(
                "test-delta: net {net:+} test declarations against {base} exceeds the cap {cap}. Delete a test that guards no contract of its own."
            );
            return 1;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_added_removed_and_net_test_declarations_by_language() {
        let diff = concat!(
            "+++ b/tests/test_added.py\n",
            "--- a/tests/test_added.py\n",
            "+def test_new_case():\n",
            "-def test_old_case():\n",
            // A `# T<digit>` comment in a Python file is prose, never a shell
            // case: the matcher scopes every language to its own extension.
            "+# T3 in a python file is prose, not a shell case\n",
            "+++ b/tests/lib.rs\n",
            "--- a/tests/lib.rs\n",
            "+#[test]\n",
            "+#[tokio::test]\n",
            "+#[tokio::test(flavor = \"multi_thread\")]\n",
            "-#[test]\n",
            "+++ b/tests/suite.sh\n",
            "--- a/tests/suite.sh\n",
            "+@test \"shell case\" {\n",
            "+# T2: a new numbered case in a shell suite\n",
            "-test_old_shell() {\n",
            "-test_old_shell_two() {\n",
            "-# T1: a Notification payload with a message yields one recorded call\n",
            "-# T13 (x-f209): FNO_SERVER carries the server axis now\n",
            "+not_a_test() {\n",
            "+prose mentions T3 without a header, never counts\n",
        );
        let delta = TestDelta::from_diff(diff);
        assert_eq!(
            delta.python,
            LanguageDelta {
                added: 1,
                removed: 1
            }
        );
        assert_eq!(
            delta.rust,
            LanguageDelta {
                added: 3,
                removed: 1
            }
        );
        // The shell side counts declarations AND numbered case headers on
        // both sides: 1 bats + 1 case header added, 2 fns + 2 case headers
        // removed. The prose line without a `# ` prefix never counts.
        assert_eq!(
            delta.shell,
            LanguageDelta {
                added: 2,
                removed: 4
            }
        );
        assert!(delta
            .markdown()
            .contains("| **Total** | **6** | **6** | **+0** |"));
        // The cap spans all three languages: the ported shell deletions pay
        // for the Rust additions, so a port nets to its true delta.
        assert_eq!(over_cap(&delta, 0), None);
        let growth_only_rust = TestDelta {
            rust: LanguageDelta {
                added: 2,
                removed: 0,
            },
            shell: LanguageDelta {
                added: 0,
                removed: 2,
            },
            ..Default::default()
        };
        assert_eq!(over_cap(&growth_only_rust, 0), None);
        let plain_growth = TestDelta {
            rust: LanguageDelta {
                added: 2,
                removed: 0,
            },
            ..Default::default()
        };
        assert_eq!(over_cap(&plain_growth, 1), Some(2));
    }
}
