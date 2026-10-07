//! parity-stage: characterization
//! parity-oracle: fno.style
//!
//! Characterization for the `fno doctor lint style` port: the Rust leg is the
//! hidden `style-check` verb on the sibling fno-agents binary; the Python leg
//! was `cli/src/fno/style.py` plus the `style` action in `cli/src/fno/lint_cli.py`
//! (deleted in the same change that flipped this file). Each case runs the
//! Rust verb as a subprocess and asserts `(exit, stdout, stderr)` against the
//! frozen goldens under `tests/golden/style/`, keyed by a slug of the case
//! label. To regenerate them (only meaningful while the Python leg still
//! lives), run with `FNO_CAPTURE_GOLDEN=1`: the helper then runs the Python
//! CLI (`cli/.venv/bin/fno-py doctor lint style ...`, uv fallback) on the same
//! fixture, writes the goldens, and asserts Rust==Python before freezing.
//!
//! Coverage: one breach per rule 1 to 8 (mail), the 80-word cap on mail and
//! encounter, a style-exception line, pr-body/markdown/comment surfaces on
//! stdin, clean and empty stdin, `--files` clean/exception, `--diff-base`
//! added-lines in a temp git repo (a fence opened on an untouched line),
//! `--fix` residue and clean, and the usage refusals.

use common::{assert_golden as assert_golden_common, capture_mode, Golden};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod common;

/// The repo the worktree test binary lives in.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// The Python CLI: prefer the synced venv script, fall back to `uv run`
/// (which syncs the venv on first use).
fn python_cli(repo: &Path) -> (PathBuf, Vec<String>) {
    let venv = repo.join("cli/.venv/bin/fno-py");
    if venv.is_file() {
        return (venv, vec![]);
    }
    let uv = which_uv();
    assert!(
        !uv.is_empty(),
        "no cli/.venv and no uv on PATH: run `cd cli && uv sync` to provide the Python oracle"
    );
    (
        PathBuf::from(uv),
        vec![
            "run".into(),
            "--project".into(),
            repo.join("cli").display().to_string(),
            "fno-py".into(),
        ],
    )
}

fn which_uv() -> String {
    std::env::var_os("PATH")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
        .split(':')
        .map(PathBuf::from)
        .find(|dir| dir.join("uv").is_file())
        .map(|dir| dir.join("uv").display().to_string())
        .unwrap_or_default()
}

/// Run one leg on a fixture. `cwd` anchors git and relative paths; `stdin`
/// feeds the body for `--stdin` runs.
fn run_leg(
    leg: Leg,
    cwd: Option<&Path>,
    args: &[&str],
    stdin_text: Option<&str>,
) -> (i32, String, String) {
    let mut argv: Vec<String> = match leg {
        Leg::Rust => vec!["style-check".into()],
        Leg::Python => vec!["doctor".into(), "lint".into(), "style".into()],
    };
    argv.extend(args.iter().map(|s| s.to_string()));
    let mut cmd = match leg {
        Leg::Rust => {
            let mut c = Command::new(env!("CARGO_BIN_EXE_fno-agents"));
            c.args(&argv).envs(fno_agents::test_run::self_owner_env());
            c
        }
        Leg::Python => {
            let (bin, prefix) = python_cli(&repo_root());
            let mut full = prefix;
            full.extend(argv);
            let mut c = Command::new(bin);
            c.args(&full);
            c
        }
    };
    cmd.current_dir(cwd.unwrap_or(&repo_root()));
    // The pin is the test hook both legs would otherwise inherit from the
    // ambient shell; a test run must resolve the repo the way the caller's
    // cwd says.
    cmd.env_remove("FNO_REPO_ROOT");
    cmd.stdin(match stdin_text {
        Some(_) => Stdio::piped(),
        None => Stdio::null(),
    });
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("style leg spawns");
    if let Some(text) = stdin_text {
        use std::io::Write as _;
        child
            .stdin
            .take()
            .expect("stdin piped")
            .write_all(text.as_bytes())
            .expect("stdin written");
    }
    let out = child.wait_with_output().expect("style leg runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

enum Leg {
    Rust,
    Python,
}

/// Tempdir paths must not freeze into goldens: both the captured golden and
/// the live Rust output get each known volatile prefix replaced before
/// write/compare. Longest first so a shorter prefix cannot partial-match.
fn normalize(s: &str, volatile: &[String]) -> String {
    let mut out = s.to_string();
    for p in volatile.iter().rev() {
        out = out.replace(p.as_str(), "<FIXTURE>");
    }
    out
}

/// Core golden assertion over one `(cwd, args, stdin)` fixture.
fn assert_case(cwd: Option<&Path>, args: &[&str], stdin: Option<&str>, label: &str) {
    assert_case_volatile(cwd, args, stdin, &[], label);
}

/// Same, with volatile path prefixes normalized on both sides.
fn assert_case_volatile(
    cwd: Option<&Path>,
    args: &[&str],
    stdin: Option<&str>,
    volatile: &[String],
    label: &str,
) {
    let (rc, ro, re) = run_leg(Leg::Rust, cwd, args, stdin);
    let rust = Golden {
        exit: Some(rc),
        streams: vec![normalize(&ro, volatile), normalize(&re, volatile)],
    };
    let oracle = capture_mode().then(|| {
        let (pc, po, pe) = run_leg(Leg::Python, cwd, args, stdin);
        Golden {
            exit: Some(pc),
            streams: vec![normalize(&po, volatile), normalize(&pe, volatile)],
        }
    });
    assert_golden_common("style", label, &rust, oracle);
}

/// Build the temp git repo the --diff-base cases run against: a base commit,
/// then a HEAD commit adding a fenced-block line plus a prose violation.
fn init_repo(base_md: &str, head_md: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::TempDir::new().unwrap();
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-q"]);
    run(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "--allow-empty",
        "-q",
        "-m",
        "base",
    ]);
    let base = String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    std::fs::write(dir.path().join("notes.md"), base_md).unwrap();
    run(&["add", "notes.md"]);
    run(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "-q",
        "-m",
        "head",
    ]);
    std::fs::write(dir.path().join("notes.md"), head_md).unwrap();
    run(&["add", "notes.md"]);
    run(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "-q",
        "-m",
        "edit",
    ]);
    (dir, base)
}

// --- stdin: one breach per rule 1 to 8 (surface mail) ---

#[test]
fn rule1_length_over_25_words() {
    let body = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty twentyone twentytwo twentythree twentyfour twentyfive twentysix.\n";
    assert_case(
        None,
        &["--stdin", "--surface", "mail"],
        Some(body),
        "rule 1 length breach on mail",
    );
}

#[test]
fn rule2_semicolon() {
    assert_case(
        None,
        &["--stdin", "--surface", "mail"],
        Some("Do this; do that.\n"),
        "rule 2 semicolon on mail",
    );
}

#[test]
fn rule3_modal() {
    assert_case(
        None,
        &["--stdin", "--surface", "mail"],
        Some("You should do this.\n"),
        "rule 3 modal on mail",
    );
}

#[test]
fn rule4_contraction() {
    assert_case(
        None,
        &["--stdin", "--surface", "mail"],
        Some("It can't work.\n"),
        "rule 4 contraction on mail",
    );
}

#[test]
fn rule5_condition_after_command() {
    assert_case(
        None,
        &["--stdin", "--surface", "mail"],
        Some("Run it when it fails.\n"),
        "rule 5 condition on mail",
    );
}

#[test]
fn rule6_wrapped_paragraph() {
    assert_case(
        None,
        &["--stdin", "--surface", "pr-body"],
        Some("First line of the paragraph\nsecond line continues it.\n"),
        "rule 6 wrap on pr-body",
    );
}

#[test]
fn rule7_word_cap_mail() {
    let body = format!("{}\n", "word ".repeat(81).trim_end());
    assert_case(
        None,
        &["--stdin", "--surface", "mail"],
        Some(&body),
        "rule 7 cap on mail",
    );
}

#[test]
fn rule8_filler() {
    assert_case(
        None,
        &["--stdin", "--surface", "mail"],
        Some("Please do this.\n"),
        "rule 8 filler on mail",
    );
}

// --- stdin: surfaces, exceptions, clean and empty bodies ---

#[test]
fn style_exception_line_skips_stdin() {
    assert_case(
        None,
        &["--stdin", "--surface", "mail"],
        Some("style-exception: legacy prose\nYou should do this.\n"),
        "style-exception line on stdin",
    );
}

#[test]
fn comment_surface_footer() {
    assert_case(
        None,
        &["--stdin", "--surface", "comment"],
        Some("You should do this.\n"),
        "comment surface refusal",
    );
}

#[test]
fn clean_stdin_exits_zero() {
    assert_case(
        None,
        &["--stdin", "--surface", "mail"],
        Some("Do this now.\n"),
        "clean stdin exits 0",
    );
}

// --- --files ---

/// A temp markdown file the --files cases check whole.
fn write_case_file(dir: &tempfile::TempDir, name: &str, body: &str) -> String {
    let path = dir.path().join(name);
    std::fs::write(&path, body).unwrap();
    path.display().to_string()
}

#[test]
fn files_with_violation() {
    let dir = tempfile::TempDir::new().unwrap();
    let p = write_case_file(&dir, "note.md", "You should do this.\n");
    assert_case_volatile(
        None,
        &["--files", &p],
        None,
        &[dir.path().display().to_string()],
        "files on a violating markdown file",
    );
}

#[test]
fn files_style_exception_zero_read() {
    let dir = tempfile::TempDir::new().unwrap();
    let p = write_case_file(
        &dir,
        "note.md",
        "style-exception: legacy\n\nYou should do this.\n",
    );
    assert_case_volatile(
        None,
        &["--files", &p],
        None,
        &[dir.path().display().to_string()],
        "files skipped by style-exception exits 2",
    );
}

// --- --diff-base (temp git repo) ---

#[test]
fn diff_base_added_prose_violation() {
    let base_md = "Intro line.\n\nOutro line.\n";
    let head_md = "Intro line.\n\nYou should do this; it is basically fine.\n\nOutro line.\n";
    let (dir, base) = init_repo(base_md, head_md);
    assert_case_volatile(
        Some(dir.path()),
        &["--surface", "markdown", "--diff-base", &base],
        None,
        &[dir.path().display().to_string()],
        "diff-base added prose violation",
    );
}

#[test]
fn diff_base_fence_on_untouched_line() {
    // The fence opens on a line the commit did not touch, so the added line
    // inside is masked as code and skipped, not read as prose.
    let base_md = "```text\nold code\n```\n";
    let head_md = "```text\nold code\nadded; semicolon line\n```\n";
    let (dir, base) = init_repo(base_md, head_md);
    assert_case_volatile(
        Some(dir.path()),
        &["--surface", "markdown", "--diff-base", &base],
        None,
        &[dir.path().display().to_string()],
        "diff-base added line inside existing fence",
    );
}

#[test]
fn diff_base_requires_markdown_surface() {
    let dir = tempfile::TempDir::new().unwrap();
    assert_case_volatile(
        Some(dir.path()),
        &["--stdin", "--surface", "mail", "--diff-base", "HEAD"],
        None,
        &[dir.path().display().to_string()],
        "diff-base on mail refuses",
    );
}

#[test]
fn diff_base_fix_combination_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    assert_case_volatile(
        Some(dir.path()),
        &["--surface", "markdown", "--diff-base", "HEAD", "--fix"],
        None,
        &[dir.path().display().to_string()],
        "fix with diff-base refuses",
    );
}

#[test]
fn diff_base_bad_ref_refuses() {
    let dir = tempfile::TempDir::new().unwrap();
    assert_case_volatile(
        Some(dir.path()),
        &["--surface", "markdown", "--diff-base", "no-such-ref"],
        None,
        &[dir.path().display().to_string()],
        "bad diff-base refuses",
    );
}

// --- usage refusals ---

#[test]
fn unknown_surface_refuses() {
    assert_case(
        None,
        &["--stdin", "--surface", "wat"],
        Some("Do this.\n"),
        "unknown surface refuses",
    );
}

#[test]
fn no_input_mode_refuses() {
    assert_case(None, &["--surface", "mail"], None, "no input mode refuses");
}

// --- --fix ---

#[test]
fn fix_clean_on_stdin() {
    assert_case(
        None,
        &["--stdin", "--surface", "mail", "--fix"],
        Some("Do this; do that.\n"),
        "fix rewrites semicolon clean",
    );
}

#[test]
fn fix_residue_exits_1() {
    assert_case(
        None,
        &["--stdin", "--surface", "mail", "--fix"],
        Some("You should do this; do that.\n"),
        "fix leaves residue exit 1",
    );
}
