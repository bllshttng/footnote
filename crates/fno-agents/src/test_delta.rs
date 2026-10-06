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
            let delta = declaration_bucket(ext, code).map(|bucket| match bucket {
                Bucket::Python => &mut result.python,
                Bucket::Rust => &mut result.rust,
                Bucket::Shell => &mut result.shell,
            });
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
/// Which census language a declaration belongs to. The diff file's
/// extension scopes the matcher: a `# T1` comment in a Python file is
/// prose, a `test_`-prefixed function in Rust is not a shell case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bucket {
    Python,
    Rust,
    Shell,
}

fn declaration_bucket(ext: &str, code: &str) -> Option<Bucket> {
    if ext == "py" && (code.starts_with("def test_") || code.starts_with("async def test_")) {
        Some(Bucket::Python)
    } else if ext == "rs" && (code.trim() == "#[test]" || code.starts_with("#[tokio::test")) {
        Some(Bucket::Rust)
    } else if ext == "sh"
        && (code.starts_with("@test ")
            || code.starts_with("test_") && code.contains("() {")
            || is_shell_case_declaration(code))
    {
        Some(Bucket::Shell)
    } else {
        None
    }
}

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

pub fn diff_range(git_bin: &str, dir: &Path, range: &str) -> Result<String, String> {
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
    // The audited-owners census: which owner test files the branch audited
    // (a diff touch or a cut), else the obligation line the executor
    // completes after a real prune pass.
    let census = census(&diff);
    let py_tests = python_test_files(Path::new("."));
    let mut owners = owners_of(&census, &py_tests, Path::new("."));
    owners.sort();
    owners.dedup();
    if let Some(line) = owners_report(&owners, &census.test_files_touched, &census.cuts_by_file) {
        println!("{line}");
    }
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

/// The deduped owner test files over every changed production file.
pub fn owners_of(census: &DiffCensus, py_tests: &[String], root: &Path) -> Vec<String> {
    let mut owners: Vec<String> = Vec::new();
    for prod in census.changed.iter().filter(|p| !is_test_path(p)) {
        owners.extend(owner_test_files(prod, py_tests, root));
    }
    owners
}

/// Whether a path is a test file by convention: a `tests/` segment, a
/// `test_*.py` or `conftest.py` basename, or a `*_test.rs` name. Rust inline
/// tests live in production files, so a `.rs` file matching none of these is
/// production even when it carries `#[cfg(test)]`.
pub fn is_test_path(path: &str) -> bool {
    if path.split('/').any(|segment| segment == "tests") {
        return true;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    (name.starts_with("test_") && name.ends_with(".py"))
        || name == "conftest.py"
        || name.ends_with("_test.rs")
}

/// What the branch diff did around tests: every changed path, the test-file
/// paths it touched, and removed test declarations per file.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct DiffCensus {
    pub changed: Vec<String>,
    pub test_files_touched: Vec<String>,
    pub cuts_by_file: Vec<(String, u64)>,
}

impl DiffCensus {
    pub fn total_cuts(&self) -> u64 {
        self.cuts_by_file.iter().map(|(_, n)| *n).sum()
    }

    fn note_change(&mut self, path: &str) {
        if !self.changed.iter().any(|p| p == path) {
            self.changed.push(path.to_string());
        }
    }
}

/// Paths and per-file cuts from a unified=0 diff. A pure deletion names the
/// file only on the `--- a/` side; a pure addition only on `+++ b/`.
pub fn census(diff: &str) -> DiffCensus {
    let mut result = DiffCensus::default();
    let mut path = String::new();
    let mut cuts = 0u64;
    for line in diff.lines() {
        if let Some(p) = line.strip_prefix("--- a/") {
            if cuts > 0 {
                result.cuts_by_file.push((path.clone(), cuts));
            }
            cuts = 0;
            path = p.to_string();
            continue;
        }
        if let Some(p) = line.strip_prefix("+++ b/") {
            if p != "/dev/null" {
                path = p.to_string();
            }
            continue;
        }
        let (added, source) = match line.as_bytes().first() {
            Some(b'+') if !line.starts_with("+++") => (true, &line[1..]),
            Some(b'-') if !line.starts_with("---") => (false, &line[1..]),
            _ => continue,
        };
        if path.is_empty() {
            continue;
        }
        result.note_change(&path);
        if added && is_test_path(&path) && !result.test_files_touched.iter().any(|p| p == &path) {
            result.test_files_touched.push(path.clone());
        }
        let ext = path.rsplit('.').next().unwrap_or("");
        if !added && declaration_bucket(ext, source.trim_start()).is_some() {
            cuts += 1;
        }
    }
    if cuts > 0 {
        result.cuts_by_file.push((path, cuts));
    }
    result
}

/// Python test files git tracks, for the same-stem owner match. An empty
/// list is legal: a repo without Python tests maps no Python owners.
pub fn python_test_files(root: &Path) -> Vec<String> {
    let output = Command::new("git")
        .args(["ls-files", "*.py"])
        .current_dir(root)
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .filter(|p| is_test_path(p))
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// The test files that own one changed production file, by repo convention:
/// Python maps to the suite named for the module (`test_<stem>.py`, or
/// `test_<parent>_<stem>.py` for the package's cli-style entry points);
/// Rust maps to its own inline test module when the file carries
/// `#[cfg(test)]`, plus a same-stem integration file beside the crate when
/// one exists. No mapped owner means no audit obligation, not a pass on
/// coverage.
pub fn owner_test_files(path: &str, py_tests: &[String], root: &Path) -> Vec<String> {
    let mut owners: Vec<String> = Vec::new();
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = match name.rsplit_once('.') {
        Some((stem, _)) => stem,
        None => return owners,
    };
    if stem.is_empty() {
        return owners;
    }
    if path.ends_with(".py") {
        let mut wanted: Vec<String> = vec![format!("test_{stem}.py")];
        if let Some(parent) = path.rsplit('/').nth(1) {
            wanted.push(format!("test_{parent}_{stem}.py"));
        }
        for candidate in py_tests {
            let candidate_name = candidate.rsplit('/').next().unwrap_or(candidate);
            if wanted.iter().any(|w| candidate_name == w) {
                owners.push(candidate.clone());
            }
        }
    } else if path.ends_with(".rs") && path.contains("/src/") {
        if let Ok(content) = std::fs::read_to_string(root.join(path)) {
            if content.contains("#[cfg(test)]") {
                owners.push(path.to_string());
            }
        }
        if let Some((crate_dir, _)) = path.split_once("/src/") {
            let sibling = format!("{crate_dir}/tests/{stem}.rs");
            if root.join(&sibling).is_file() {
                owners.push(sibling);
            }
        }
    }
    owners.sort();
    owners.dedup();
    owners
}

/// The census line for the mapped owner set. The diff touching an owner
/// test file (or cutting from one) is evidence the audit ran, so the line
/// reads `Audited owners:`; otherwise the obligation is unmet and the line
/// names it. `None` when the branch maps no owners at all.
pub fn owners_report(
    owners: &[String],
    touched: &[String],
    cuts_by_file: &[(String, u64)],
) -> Option<String> {
    if owners.is_empty() {
        return None;
    }
    let audited = owners
        .iter()
        .any(|o| touched.iter().any(|t| t == o) || cuts_by_file.iter().any(|(f, _)| f == o));
    if !audited {
        return Some(format!("Owner tests unaudited: {}", owners.join(" ")));
    }
    let parts: Vec<String> = owners
        .iter()
        .map(|o| {
            let n: u64 = cuts_by_file
                .iter()
                .filter(|(f, _)| f == o)
                .map(|(_, n)| *n)
                .sum();
            if n > 0 {
                format!("{o} (cut {n})")
            } else {
                format!("{o} (no cut)")
            }
        })
        .collect();
    Some(format!("Audited owners: {}", parts.join(", ")))
}

/// The pr-create body gate: a code PR whose mapped owner test files went
/// unaudited is refused, so every session either prunes the tests that own
/// its changed files or records in the body why nothing was cut. A cut
/// anywhere in the branch, no mapped owners, or an `Audited owners:` line
/// in the body passes; `Err` names every owner file left unaudited.
pub fn owners_gate(body: &str, owners: &[String], cuts: u64) -> Result<(), String> {
    if cuts > 0 || owners.is_empty() {
        return Ok(());
    }
    let carried = body
        .lines()
        .any(|l| l.trim_start().starts_with("Audited owners:"));
    if carried {
        return Ok(());
    }
    Err(format!(
        "no test declaration was cut and the body carries no `Audited owners:` line; run /fno:test-audit prune on the owner test files of the changed production files, then land cuts or the line naming why nothing was cut. Owner test files: {}",
        owners.join(", ")
    ))
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
        // The census and the owner mapping guard the same branch diff, so
        // they run under this one declaration: the suite is shrink-only and
        // this branch nets to zero.
        checks_census_paths_touches_and_cuts();
        checks_owner_mapping_report_and_gate();
    }

    fn checks_census_paths_touches_and_cuts() {
        let diff = concat!(
            "diff --git a/crates/x/src/lib.rs b/crates/x/src/lib.rs\n",
            "--- a/crates/x/src/lib.rs\n",
            "+++ b/crates/x/src/lib.rs\n",
            "+pub fn fresh() {}\n",
            "-#[test]\n",
            "-fn old_case() {}\n",
            "diff --git a/cli/tests/unit/test_thing.py\n",
            "--- a/cli/tests/unit/test_thing.py\n",
            "+++ b/cli/tests/unit/test_thing.py\n",
            "+def test_new():\n",
            "-def test_gone():\n",
            "diff --git a/docs/note.md b/docs/note.md\n",
            "--- a/docs/note.md\n",
            "+++ b/docs/note.md\n",
            "+- prose, not code\n",
            "diff --git a/new.py b/new.py\n",
            "--- /dev/null\n",
            "+++ b/new.py\n",
            "+def test_brand_new():\n",
        );
        let census = census(diff);
        assert!(census.changed.contains(&"crates/x/src/lib.rs".to_string()));
        assert!(census.changed.contains(&"docs/note.md".to_string()));
        assert_eq!(
            census.test_files_touched,
            vec!["cli/tests/unit/test_thing.py".to_string()]
        );
        assert_eq!(
            census.cuts_by_file,
            vec![
                ("crates/x/src/lib.rs".to_string(), 1),
                ("cli/tests/unit/test_thing.py".to_string(), 1),
            ]
        );
        assert_eq!(census.total_cuts(), 2);
        // Path conventions: a tests segment, a test_*.py / conftest.py
        // basename, a *_test.rs name. Inline Rust tests stay production
        // paths; the owner rule reaches them instead.
        assert!(is_test_path("cli/tests/unit/test_thing.py"));
        assert!(is_test_path("tests/conftest.py"));
        assert!(is_test_path("crates/x/tests/journey.rs"));
        assert!(is_test_path("suites/launch_test.rs"));
        assert!(!is_test_path("cli/src/fno/pr/cli.py"));
        assert!(!is_test_path("crates/x/src/lib.rs"));
    }

    fn checks_owner_mapping_report_and_gate() {
        let dir = tempfile::tempdir().unwrap();
        let py_tests = vec![
            "cli/tests/unit/test_pr_cli.py".to_string(),
            "cli/tests/unit/test_cli.py".to_string(),
            // A sibling package's suite never owns this module: the parent
            // prefix is exact, not a suffix wildcard.
            "cli/tests/unit/test_agent_cli.py".to_string(),
            "test_other.py".to_string(),
        ];
        assert_eq!(
            owner_test_files("cli/src/fno/pr/cli.py", &py_tests, dir.path()),
            vec![
                "cli/tests/unit/test_cli.py".to_string(),
                "cli/tests/unit/test_pr_cli.py".to_string(),
            ]
        );
        assert!(owner_test_files("cli/src/fno/done/closer.py", &py_tests, dir.path()).is_empty());
        let src = dir.path().join("crates/x/src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("lib.rs"),
            "pub fn f() {}\n#[cfg(test)]\nmod tests {}\n",
        )
        .unwrap();
        assert_eq!(
            owner_test_files("crates/x/src/lib.rs", &[], dir.path()),
            vec!["crates/x/src/lib.rs".to_string()]
        );
        std::fs::write(src.join("plain.rs"), "pub fn g() {}\n").unwrap();
        assert!(owner_test_files("crates/x/src/plain.rs", &[], dir.path()).is_empty());
        let integration = dir.path().join("crates/x/tests");
        std::fs::create_dir_all(&integration).unwrap();
        std::fs::write(integration.join("plain.rs"), "").unwrap();
        assert_eq!(
            owner_test_files("crates/x/src/plain.rs", &[], dir.path()),
            vec!["crates/x/tests/plain.rs".to_string()]
        );
        // The report and the gate are one census contract: the diff touch
        // or cut is audit evidence, else the body line or nothing.
        let owners = vec!["t/a_test.py".to_string(), "t/b_test.py".to_string()];
        assert_eq!(
            owners_report(&owners, &[], &[("t/a_test.py".to_string(), 2)]),
            Some("Audited owners: t/a_test.py (cut 2), t/b_test.py (no cut)".to_string())
        );
        assert_eq!(
            owners_report(&owners, &["t/other.py".to_string()], &[]),
            Some("Owner tests unaudited: t/a_test.py t/b_test.py".to_string())
        );
        assert_eq!(owners_report(&[], &[], &[]), None);
        assert!(owners_gate("body", &owners, 1).is_ok());
        assert!(owners_gate("body", &[], 0).is_ok());
        assert!(owners_gate(
            "x\nAudited owners: t/a_test.py (no cut: kept, distinct contracts)\n",
            &owners,
            0
        )
        .is_ok());
        let err = owners_gate("no cuts, no line", &owners, 0).unwrap_err();
        assert!(err.contains("no test declaration was cut"), "{err}");
        assert!(err.contains("t/a_test.py"), "{err}");
    }
}
