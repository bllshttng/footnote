//! Branch-local test-count changes for the PR description.

use std::path::Path;
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
        for line in diff.lines() {
            let (added, source) = match line.as_bytes().first() {
                Some(b'+') if !line.starts_with("+++") => (true, &line[1..]),
                Some(b'-') if !line.starts_with("---") => (false, &line[1..]),
                _ => continue,
            };
            let code = source.trim_start();
            let delta = if code.starts_with("def test_") || code.starts_with("async def test_") {
                Some(&mut result.python)
            } else if code.trim() == "#[test]" || code.starts_with("#[tokio::test") {
                Some(&mut result.rust)
            } else if code.starts_with("@test ")
                || code.starts_with("test_") && code.contains("() {")
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

/// Net test declarations over the two census languages (Python and Rust).
/// Shell stays outside the cap: a shell deletion never pays for a Python or
/// Rust test.
fn over_cap(delta: &TestDelta, cap: i64) -> Option<i64> {
    let net = net(&delta.python) + net(&delta.rust);
    (net > cap).then_some(net)
}

fn diff_range(dir: &Path, range: &str) -> Result<String, String> {
    let output = Command::new("git")
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

/// The shrink-only gate the push path shares with CI (`--max-net 0`). The
/// OverCap message carries the table so a worker fixes the delta locally
/// instead of learning the cap from a red main one full round later.
pub fn shrink_only_gate(dir: &Path, base: &str) -> Result<(), ShrinkGate> {
    let range = format!("{base}...HEAD");
    let diff = diff_range(dir, &range).map_err(ShrinkGate::Diff)?;
    let delta = TestDelta::from_diff(&diff);
    if let Some(net) = over_cap(&delta, 0) {
        return Err(ShrinkGate::OverCap(format!(
            "the suite is shrink-only: net {net:+} test declarations against {base} (cap 0). \
             Delete a test that guards no contract of its own (docs/test-audit/README.md, Keep rule):\n{}",
            delta.markdown()
        )));
    }
    Ok(())
}

/// `fno-agents test-delta --base <ref>` prints test declaration changes on
/// the current branch. Python test functions, Rust test attributes, and shell
/// test declarations are counted from the committed merge-base diff.
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
        None => None,
    };
    let range = format!("{base}...HEAD");
    let diff = match diff_range(Path::new("."), &range) {
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
                "test-delta: net {net:+} test declarations against {base} (cap {cap}). The suite is shrink-only: delete a test that guards no contract of its own (docs/test-audit/README.md, Keep rule)."
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
            "+def test_new_case():\n",
            "+#[test]\n",
            "+#[tokio::test]\n",
            "+#[tokio::test(flavor = \"multi_thread\")]\n",
            "+@test \"shell case\" {\n",
            "-def test_old_case():\n",
            "-#[test]\n",
            "-test_old_shell() {\n",
            "-test_old_shell_two() {\n",
            "+not_a_test() {\n",
            "+++ b/tests/test_added.py\n",
            "--- a/tests/test_removed.py\n",
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
        assert_eq!(
            delta.shell,
            LanguageDelta {
                added: 1,
                removed: 2
            }
        );
        assert!(delta
            .markdown()
            .contains("| **Total** | **5** | **4** | **+1** |"));
        // The cap covers the census languages only: shell's -1 never offsets
        // Python or Rust growth.
        assert_eq!(over_cap(&delta, 0), Some(2));
        assert_eq!(over_cap(&delta, 1), Some(2));
        assert_eq!(over_cap(&delta, 2), None);
    }
}
