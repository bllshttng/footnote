//! Characterization tests for the `kill-check` Rust port, frozen against the
//! bash oracle `scripts/lib/kill-criteria.sh` (packaging EPIC ab-8bdb4642).
//!
//! These were originally DIFFERENTIAL parity tests that ran BOTH the bash
//! oracle and the Rust verb on identical fixtures and asserted byte-equality.
//! The bash oracle has since been deleted (the Rust port is the sole
//! implementation), so each case now asserts the Rust output against a GOLDEN
//! `(exit, stdout)` captured from the proven-correct bash BEFORE deletion.
//!
//! Goldens live under `tests/golden/kill_criteria/<case>.{exit,out}`, keyed by a
//! slug of each case's label. To regenerate them (only meaningful while the bash
//! oracle still exists), run with `FNO_CAPTURE_GOLDEN=1`: the helper then runs
//! bash, writes the golden files, AND asserts Rust==bash before freezing.
//!
//! Each test builds a fixture (temp git repo + plan + `.fno/target-state.md`)
//! and runs `kill_criteria::run_kill_check_capture(["kill-check", <plan>])`.
//! Only `(exit, stdout)` is asserted; the bash WARN-to-stderr surface was never
//! part of the parity contract here.
//!
//! coverage (AC1-EDGE / AC1-ERR fixtures):
//!   - iteration ceiling hit / not-hit (`>` and `>=`)
//!   - same_test_failing_for hit / not-hit
//!   - files_outside over / under
//!   - any_test_file_deleted (test deletion present / absent)
//!   - a malformed predicate -> WARN + exit 0 (no fire)
//!   - no kill_criteria block -> exit 0
//!   - quick-mode fenced `## Kill Criteria`

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Slug a case label into a filename-safe golden key (lowercase, every run of
/// non-alphanumeric chars collapses to a single `_`, trimmed).
fn slug(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut prev_us = false;
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    out.trim_matches('_').to_string()
}

/// Directory holding the frozen kill_criteria goldens.
fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/kill_criteria")
}

/// Whether to (re)capture goldens from the live bash oracle this run.
fn capture_mode() -> bool {
    std::env::var("FNO_CAPTURE_GOLDEN").is_ok()
}

/// Absolute path to the bash oracle (only used in capture mode).
fn bash_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts/lib/kill-criteria.sh")
}

/// Init a fresh git repo at `dir` so git-root resolution is hermetic.
fn init_repo(dir: &Path) {
    run_git(dir, &["init", "-q"]);
    run_git(dir, &["config", "user.email", "t@t.t"]);
    run_git(dir, &["config", "user.name", "t"]);
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed in {dir:?}");
}

/// Run the bash oracle with cwd=`dir`. Returns (exit_code, stdout).
/// Only invoked in capture mode.
fn run_bash(dir: &Path, plan_path: &str) -> (i32, String) {
    let script = bash_script();
    let cmd = format!(
        "source '{}'; check_kill_criteria '{}'",
        script.display(),
        plan_path
    );
    let out = Command::new("bash")
        .current_dir(dir)
        .args(["-c", &cmd])
        .output()
        .expect("run bash oracle");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Run the Rust port in-process with cwd=`dir` (so git-root matches the bash
/// that produced the golden). Returns (exit_code, stdout).
fn run_rust(dir: &Path, plan_path: &str) -> (i32, String) {
    // The Rust resolves git-root via `git rev-parse --show-toplevel` from the
    // process cwd, so we chdir for the call. The chdir is process-global, so a
    // single static Mutex serializes the window across parallel test threads.
    let _guard = cwd_lock().lock().unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();
    let (code, stdout, _stderr) = fno_agents::kill_criteria::run_kill_check_capture(&[
        "kill-check".to_string(),
        plan_path.to_string(),
    ]);
    std::env::set_current_dir(prev).unwrap();
    (code, stdout)
}

/// Process-global cwd lock: `set_current_dir` mutates shared state, so the Rust
/// chdir window must not interleave across parallel test threads.
fn cwd_lock() -> &'static std::sync::Mutex<()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Build a full-mode plan folder with a `00-INDEX.md` carrying the given
/// kill_criteria YAML list body (already indented list items).
fn write_full_plan(repo: &Path, criteria_body: &str) -> String {
    let plan = repo.join("plan");
    fs::create_dir_all(&plan).unwrap();
    let content =
        format!("---\ntitle: test\nkill_criteria:\n{criteria_body}status: ready\n---\n# body\n");
    fs::write(plan.join("00-INDEX.md"), content).unwrap();
    plan.to_string_lossy().into_owned()
}

/// Write `.fno/target-state.md` with the given frontmatter body.
fn write_state(repo: &Path, body: &str) {
    let fno = repo.join(".fno");
    fs::create_dir_all(&fno).unwrap();
    fs::write(fno.join("target-state.md"), format!("---\n{body}---\n")).unwrap();
}

/// Assert the Rust `(exit, stdout)` equals the frozen golden for this case.
///
/// In capture mode (`FNO_CAPTURE_GOLDEN=1`), runs bash on the SAME fixture,
/// writes the golden files, and additionally asserts Rust==bash so a broken
/// capture is caught at freeze time. In normal mode (the deleted-bash world),
/// reads the golden and asserts Rust matches it — bash is never run.
fn assert_golden(repo: &Path, plan_path: &str, label: &str) {
    let key = slug(label);
    let dir = golden_dir();
    let exit_path = dir.join(format!("{key}.exit"));
    let out_path = dir.join(format!("{key}.out"));

    let (rust_code, rust_out) = run_rust(repo, plan_path);

    if capture_mode() {
        let (bash_code, bash_out) = run_bash(repo, plan_path);
        assert_eq!(
            bash_code, rust_code,
            "[{label}] capture: exit differs bash={bash_code} rust={rust_code}\nbash_out={bash_out:?}\nrust_out={rust_out:?}"
        );
        assert_eq!(
            bash_out, rust_out,
            "[{label}] capture: stdout differs\nbash={bash_out:?}\nrust={rust_out:?}"
        );
        fs::create_dir_all(&dir).unwrap();
        fs::write(&exit_path, format!("{bash_code}\n")).unwrap();
        fs::write(&out_path, &bash_out).unwrap();
        return;
    }

    let golden_exit: i32 = fs::read_to_string(&exit_path)
        .unwrap_or_else(|e| panic!("[{label}] missing golden {exit_path:?}: {e}"))
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("[{label}] bad golden exit in {exit_path:?}: {e}"));
    let golden_out = fs::read_to_string(&out_path)
        .unwrap_or_else(|e| panic!("[{label}] missing golden {out_path:?}: {e}"));

    assert_eq!(
        golden_exit, rust_code,
        "[{label}] exit differs from golden: golden={golden_exit} rust={rust_code}\nrust_out={rust_out:?}"
    );
    assert_eq!(
        golden_out, rust_out,
        "[{label}] stdout differs from golden:\ngolden={golden_out:?}\nrust={rust_out:?}"
    );
}

// ── iteration predicate ───────────────────────────────────────────────────────

#[test]
fn iteration_ceiling_hit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    let plan = write_full_plan(
        repo,
        "  - name: iter_ceiling\n    predicate: iteration > 3\n    reason: too many\n",
    );
    write_state(repo, "session_id: s\niteration: 5\n");
    assert_golden(repo, &plan, "iteration > 3 (cur=5 -> FIRE)");
}

#[test]
fn iteration_ceiling_not_hit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    let plan = write_full_plan(
        repo,
        "  - name: iter_ceiling\n    predicate: iteration > 10\n    reason: too many\n",
    );
    write_state(repo, "session_id: s\niteration: 2\n");
    assert_golden(repo, &plan, "iteration > 10 (cur=2 -> no fire)");
}

#[test]
fn iteration_gte_boundary() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    let plan = write_full_plan(
        repo,
        "  - name: iter_ge\n    predicate: iteration >= 4\n    reason: at ceiling\n",
    );
    // cur == rhs: `>=` fires, `>` would not.
    write_state(repo, "session_id: s\niteration: 4\n");
    assert_golden(repo, &plan, "iteration >= 4 (cur=4 -> FIRE)");
}

#[test]
fn iteration_missing_state_defaults_to_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    let plan = write_full_plan(
        repo,
        "  - name: iter\n    predicate: iteration > 0\n    reason: any\n",
    );
    // No .fno/target-state.md at all -> bash defaults cur=1; 1 > 0 -> FIRE.
    assert_golden(repo, &plan, "iteration > 0, no state (cur defaults 1)");
}

// ── same_test_failing_for predicate ───────────────────────────────────────────

#[test]
fn stuck_test_hit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    let plan = write_full_plan(
        repo,
        "  - name: stuck\n    predicate: same_test_failing_for >= 3\n    reason: stuck test\n",
    );
    write_state(
        repo,
        "session_id: s\niteration: 1\nverification:\n  consecutive_failures: 3\n",
    );
    assert_golden(repo, &plan, "same_test_failing_for >= 3 (cf=3 -> FIRE)");
}

#[test]
fn stuck_test_not_hit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    let plan = write_full_plan(
        repo,
        "  - name: stuck\n    predicate: same_test_failing_for > 5\n    reason: stuck test\n",
    );
    write_state(
        repo,
        "session_id: s\niteration: 1\nverification:\n  consecutive_failures: 2\n",
    );
    assert_golden(repo, &plan, "same_test_failing_for > 5 (cf=2 -> no fire)");
}

#[test]
fn stuck_test_no_verification_block_defaults_zero() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    let plan = write_full_plan(
        repo,
        "  - name: stuck\n    predicate: same_test_failing_for >= 1\n    reason: stuck\n",
    );
    // State exists but no verification block -> cf defaults 0; 0 >= 1 -> no fire.
    write_state(repo, "session_id: s\niteration: 1\n");
    assert_golden(repo, &plan, "same_test_failing_for >= 1 (cf defaults 0)");
}

// ── files_outside predicate ───────────────────────────────────────────────────

#[test]
fn files_outside_over() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    // Plan folder inside the repo; create staged files OUTSIDE it.
    let plan = write_full_plan(
        repo,
        "  - name: scope\n    predicate: files_outside(plan_path) > 1\n    reason: scope creep\n",
    );
    // Stage three files outside the plan folder.
    for f in ["a.rs", "b.rs", "c.rs"] {
        fs::write(repo.join(f), "x").unwrap();
    }
    run_git(repo, &["add", "a.rs", "b.rs", "c.rs"]);
    assert_golden(repo, &plan, "files_outside > 1 (3 outside -> FIRE)");
}

#[test]
fn files_outside_under() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    let plan = write_full_plan(
        repo,
        "  - name: scope\n    predicate: files_outside(plan_path) > 5\n    reason: scope creep\n",
    );
    // Only one file outside -> 1 is not > 5.
    fs::write(repo.join("only.rs"), "x").unwrap();
    run_git(repo, &["add", "only.rs"]);
    assert_golden(repo, &plan, "files_outside > 5 (1 outside -> no fire)");
}

#[test]
fn files_outside_excludes_plan_folder_contents() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    let plan = write_full_plan(
        repo,
        "  - name: scope\n    predicate: files_outside(plan_path) > 0\n    reason: scope creep\n",
    );
    // A file INSIDE the plan folder should be excluded; nothing outside ->
    // count 0, not > 0. (Stage the plan's own index + an inside file.)
    fs::write(Path::new(&plan).join("extra.md"), "x").unwrap();
    run_git(repo, &["add", "plan"]);
    assert_golden(
        repo,
        &plan,
        "files_outside > 0 (only inside-plan -> no fire)",
    );
}

// ── any_test_file_deleted predicate ───────────────────────────────────────────

#[test]
fn test_file_deleted_present() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    // Commit a test file, then delete it (unstaged deletion shows in porcelain).
    fs::create_dir_all(repo.join("tests")).unwrap();
    fs::write(repo.join("tests/test_thing.py"), "def test(): pass\n").unwrap();
    run_git(repo, &["add", "."]);
    run_git(repo, &["commit", "-q", "-m", "init"]);
    fs::remove_file(repo.join("tests/test_thing.py")).unwrap();

    let plan = write_full_plan(
        repo,
        "  - name: testdel\n    predicate: any_test_file_deleted\n    reason: removed a test\n",
    );
    assert_golden(repo, &plan, "any_test_file_deleted (deleted test -> FIRE)");
}

#[test]
fn test_file_deleted_absent_nontest_deletion() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    // Delete a NON-test file -> predicate does not fire.
    fs::write(repo.join("main.rs"), "fn main(){}\n").unwrap();
    run_git(repo, &["add", "."]);
    run_git(repo, &["commit", "-q", "-m", "init"]);
    fs::remove_file(repo.join("main.rs")).unwrap();

    let plan = write_full_plan(
        repo,
        "  - name: testdel\n    predicate: any_test_file_deleted\n    reason: removed a test\n",
    );
    assert_golden(
        repo,
        &plan,
        "any_test_file_deleted (non-test del -> no fire)",
    );
}

// ── malformed / edge predicates ───────────────────────────────────────────────

#[test]
fn malformed_predicate_warns_and_exits_zero() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    // Unknown predicate keyword -> dispatch returns malformed -> WARN + skip,
    // exit 0, empty stdout. (Golden is on stdout + exit; stderr WARN is not
    // part of the frozen surface but both produce it.)
    let plan = write_full_plan(
        repo,
        "  - name: bogus\n    predicate: definitely_not_a_predicate > 9\n    reason: nope\n",
    );
    write_state(repo, "session_id: s\niteration: 1\n");
    assert_golden(repo, &plan, "malformed predicate -> WARN + exit 0");
}

#[test]
fn malformed_iteration_operator() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    // `iteration < 3` routes to the iteration evaluator (prefix match) but the
    // regex rejects `<` -> rc=2 malformed -> WARN + skip.
    let plan = write_full_plan(
        repo,
        "  - name: badop\n    predicate: iteration < 3\n    reason: bad operator\n",
    );
    write_state(repo, "session_id: s\niteration: 5\n");
    assert_golden(repo, &plan, "iteration < 3 (bad op) -> WARN + exit 0");
}

#[test]
fn no_kill_criteria_block_exits_zero() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    // Plan with NO kill_criteria block -> exit 0 immediately, empty stdout.
    let plan = repo.join("plan");
    fs::create_dir_all(&plan).unwrap();
    fs::write(
        plan.join("00-INDEX.md"),
        "---\ntitle: test\nstatus: ready\n---\n# body\n",
    )
    .unwrap();
    assert_golden(
        repo,
        &plan.to_string_lossy(),
        "no kill_criteria block -> exit 0",
    );
}

#[test]
fn first_firing_predicate_wins() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    // Two firing predicates; the FIRST in list order must be the one emitted.
    let plan = write_full_plan(
        repo,
        "  - name: first\n    predicate: iteration > 0\n    reason: first reason\n  - name: second\n    predicate: same_test_failing_for >= 0\n    reason: second reason\n",
    );
    write_state(
        repo,
        "session_id: s\niteration: 5\nverification:\n  consecutive_failures: 9\n",
    );
    assert_golden(repo, &plan, "two firing -> first wins");
}

#[test]
fn reason_defaults_to_name_when_absent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    // No reason field -> bash emits `<name>|<name>`.
    let plan = write_full_plan(repo, "  - name: noreason\n    predicate: iteration > 0\n");
    write_state(repo, "session_id: s\niteration: 5\n");
    assert_golden(repo, &plan, "fired entry with no reason -> name|name");
}

// ── quick-mode fenced section ─────────────────────────────────────────────────

#[test]
fn quick_mode_fenced_kill_criteria() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    // A quick-plan *.md file (NOT a folder) with a fenced YAML block under
    // `## Kill Criteria`.
    let plan_file = repo.join("quick-plan.md");
    let content = "# Quick plan\n\n## Kill Criteria\n\n```yaml\nkill_criteria:\n  - name: iter\n    predicate: iteration > 2\n    reason: quick mode reason\n```\n\n## Other\n";
    fs::write(&plan_file, content).unwrap();
    write_state(repo, "session_id: s\niteration: 9\n");
    assert_golden(
        repo,
        &plan_file.to_string_lossy(),
        "quick-mode fenced kill_criteria -> FIRE",
    );
}

#[test]
fn empty_plan_path_warns_and_exits_zero() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    // Empty plan_path: the bash `check_kill_criteria ""` warns and returns 0.
    // Both sides emit a WARN to stderr (not frozen) and exit 0 / empty stdout.
    assert_golden(repo, "", "empty plan_path -> WARN + exit 0");
}
