//! Build script: embed the source git revision into the `fno` mux binary so a
//! built front door can self-report which commit it came from, the same way
//! crates/fno-agents does for the triad.
//!
//! `FNO_MUX_CRATES_REV` is the crates/ subtree rev `fno update` compares against
//! the source's crates/ subtree rev; it surfaces via `fno version --json`, so
//! `fno update` can detect a present-but-STALE front door (the mux install is
//! best-effort, so a prior failed build can leave an old `fno` beside a fresh
//! triad) and reinstall it, not just heal an absent one.
//!
//! Always emits all three env vars (falling back to "unknown"/"0") so `env!`
//! in the crate compiles even when git is unavailable -- e.g. a crates.io
//! tarball build, where there is no `.git` (the crate is `publish = true`).

use std::process::Command;

fn main() {
    let rev = git_rev().unwrap_or_else(|| "unknown".to_string());
    let dirty = git_dirty();
    // The crates/ subtree rev (last commit touching crates/) is the SAME
    // quantity Python's update._rust_subtree_rev computes and the one
    // crates/fno-agents bakes, so the mux's self-reported rev and the source
    // rev compare apples-to-apples (both subtree revs, not HEAD).
    let crates_rev = git_crates_rev().unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=FNO_MUX_GIT_REV={rev}");
    println!("cargo:rustc-env=FNO_MUX_GIT_DIRTY={}", u8::from(dirty));
    println!("cargo:rustc-env=FNO_MUX_CRATES_REV={crates_rev}");

    // Rebuild when HEAD moves so an incremental dev build does not bake a stale
    // rev. Best-effort: a missing ref path just re-runs this script, never errors.
    println!("cargo:rerun-if-changed=build.rs");
    if let Some(gitdir) = run("git", &["rev-parse", "--absolute-git-dir"]) {
        let gitdir = gitdir.trim();
        println!("cargo:rerun-if-changed={gitdir}/HEAD");
        if let Ok(head) = std::fs::read_to_string(format!("{gitdir}/HEAD")) {
            if let Some(reference) = head.strip_prefix("ref: ") {
                println!("cargo:rerun-if-changed={gitdir}/{}", reference.trim());
            }
        }
    }
}

/// Full HEAD SHA, or `None` when git is unavailable / this is not a checkout.
fn git_rev() -> Option<String> {
    let out = run("git", &["rev-parse", "HEAD"])?;
    let rev = out.trim().to_string();
    if rev.is_empty() {
        None
    } else {
        Some(rev)
    }
}

/// True when `crates/` has uncommitted changes. Scoped to the same pathspec as
/// [`git_crates_rev`]: consumers pair `dirty` with `crates_rev` to decide whether
/// a binary matches its source, and a dirty file elsewhere in the repo says
/// nothing about that. Conservative: any git failure reports `false` (a
/// published/CI build is treated as clean).
fn git_dirty() -> bool {
    let Some(top) = run("git", &["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    match run(
        "git",
        &["-C", top.trim(), "status", "--porcelain", "--", "crates/"],
    ) {
        Some(s) => !s.trim().is_empty(),
        None => false,
    }
}

/// Last commit SHA that touched `crates/`, or `None` when git is unavailable.
/// Mirrors Python `update._rust_subtree_rev` and crates/fno-agents exactly:
/// `git -C <repo-root> log -1 --format=%H -- crates/`.
fn git_crates_rev() -> Option<String> {
    let top = run("git", &["rev-parse", "--show-toplevel"])?;
    let top = top.trim();
    let out = run(
        "git",
        &["-C", top, "log", "-1", "--format=%H", "--", "crates/"],
    )?;
    let rev = out.trim().to_string();
    if rev.is_empty() {
        None
    } else {
        Some(rev)
    }
}

/// Run a command, returning trimmed stdout on a zero exit, else `None`.
fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}
