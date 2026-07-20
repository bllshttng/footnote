//! Build script: embed the source git revision into the fno-agents bins so a
//! built binary can self-report which commit it came from (ab-24a59d50).
//!
//! Rust-side `fno doctor` staleness now keys on a rev baked INTO the binary
//! instead of the external `~/.fno/installed-rust-rev` marker (which was
//! written only by `fno update`, so a bare `cargo install` or dirty dev build
//! was misjudged). `FNO_AGENTS_CRATES_REV` is the crates/ subtree rev the
//! verdict compares against the source's crates/ subtree rev (ab-716cd330);
//! `FNO_AGENTS_GIT_REV` is the full HEAD identity (ab-24a59d50). Both surface
//! via `fno-agents version --json`, so the verdict needs no marker.
//!
//! Always emits all three env vars (falling back to "unknown"/"0") so `env!`
//! in the crate compiles even when git is unavailable -- e.g. a crates.io
//! tarball build, where there is no `.git` (the crate is `publish = true`).

use std::process::Command;

fn main() {
    let rev = git_rev().unwrap_or_else(|| "unknown".to_string());
    let dirty = git_dirty();
    // The crates/ subtree rev (last commit touching crates/) is the rev `fno
    // doctor` keys its rust-staleness verdict on (ab-716cd330). It must be the
    // SAME quantity Python's update._rust_subtree_rev computes -- the last
    // commit touching crates/ -- so the binary's self-reported rev and the
    // source rev compare apples-to-apples (both subtree revs, not HEAD).
    let crates_rev = git_crates_rev().unwrap_or_else(|| "unknown".to_string());

    // Both vars are ALWAYS set so `env!("FNO_AGENTS_GIT_REV")` never fails to
    // compile, regardless of whether git was reachable at build time.
    println!("cargo:rustc-env=FNO_AGENTS_GIT_REV={rev}");
    println!("cargo:rustc-env=FNO_AGENTS_GIT_DIRTY={}", u8::from(dirty));
    println!("cargo:rustc-env=FNO_AGENTS_CRATES_REV={crates_rev}");

    // Rebuild when HEAD moves so an incremental dev build does not bake a stale
    // rev. (The install path -- `cargo install` -- always does a clean build, so
    // it is correct regardless; this is dev-iteration hygiene.) Best-effort:
    // a missing ref path just makes cargo re-run this script, never an error.
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
/// published/CI build is treated as clean rather than spuriously flagged dirty).
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
///
/// Mirrors Python `update._rust_subtree_rev` exactly: `git -C <repo-root> log -1
/// --format=%H -- crates/`. Resolving the repo root via `--show-toplevel` keeps
/// the pathspec correct regardless of build.rs's cwd (the crate dir).
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
