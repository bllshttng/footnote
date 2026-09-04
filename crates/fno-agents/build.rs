//! Build script: embed the source git revision into the fno-agents bins so a
//! built binary can self-report which commit it came from (ab-24a59d50).
//!
//! Rust-side `fno doctor` staleness now keys on a rev baked INTO the binary
//! instead of the external `~/.fno/installed-rust-rev` marker (which was
//! written only by `fno doctor update`, so a bare `cargo install` or dirty dev build
//! was misjudged). `FNO_AGENTS_CRATES_REV` is the crates/ subtree rev the
//! verdict compares against the source's crates/ subtree rev (ab-716cd330);
//! `FNO_AGENTS_GIT_REV` is the full HEAD identity (ab-24a59d50). Both surface
//! via `fno-agents version --json`, so the verdict needs no marker.
//!
//! Always emits all three env vars (falling back to "unknown"/"0") so `env!`
//! in the crate compiles even when git is unavailable -- e.g. a crates.io
//! tarball build, where there is no `.git` (the crate is `publish = true`).

use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "codegen/check_supersession.rs"]
mod check_supersession_codegen;

fn main() {
    // Produce the cross-tree copies instead of checking them. Both
    // run before the env-var work so a build that later fails still leaves the
    // copies fresh.
    sync_harness_capabilities();
    sync_merge_posture();
    sync_events_limits();
    sync_check_supersession();

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

/// Repo root as a path, or `None` when git is unavailable (crates.io tarball).
fn repo_root() -> Option<PathBuf> {
    let top = run("git", &["rev-parse", "--show-toplevel"])?;
    let top = top.trim();
    if top.is_empty() {
        None
    } else {
        Some(PathBuf::from(top))
    }
}

/// Write `bytes` to `path` only when they differ from what is already there.
///
/// An unconditional write restamps the mtime on every build, which makes cargo
/// re-run downstream work forever. Write-on-difference converges.
fn write_if_different(path: &Path, bytes: &[u8]) {
    if let Ok(existing) = std::fs::read(path) {
        if existing == bytes {
            return;
        }
    }
    if let Err(err) = std::fs::write(path, bytes) {
        println!(
            "cargo:warning=fno-agents build: could not write {}: {err}",
            path.display()
        );
    }
}

/// PRODUCE the downstream copies of the capability table instead of checking
/// them.
///
/// `harness_capabilities.rs` `include_str!`s the canonical TOML, so the Rust
/// tree owns it. Two byte copies hang off it, both generated per build:
/// `cli/src/fno/agents/harness_capabilities.toml` (loaded as Python package
/// data) and `crates/fno/src/harness_capabilities.toml` (`include_str!`ed by
/// the mux's registry reader). The fno crate stays copy-fed rather than
/// dep-fed on purpose: the two crates publish to crates.io independently
/// (the crates-publish workflow states the order is irrelevant because fno
/// does not depend on fno-agents as a cargo dep), so a dep would tie fno's
/// publishability to a registry state that lags this repo. The developer who
/// edits the canonical is the developer who builds this crate, so the sync
/// happens where the edit happens and silent drift is impossible.
/// `scripts/ci/check-harness-capabilities-fresh.sh` stays only as a tripwire
/// for a copy edited by hand.
///
/// No-op when `cli/` or the sibling crate is absent: that is the `cargo
/// package` / crates.io tarball case, where the crate must still build
/// (`publish = true`).
fn sync_harness_capabilities() {
    println!("cargo:rerun-if-changed=src/harness_capabilities.toml");
    let canonical = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/harness_capabilities.toml");
    let Ok(bytes) = std::fs::read(&canonical) else {
        return;
    };
    let Some(root) = repo_root() else { return };
    let cli_copy = root.join("cli/src/fno/agents/harness_capabilities.toml");
    if !cli_copy.is_file() {
        return;
    }
    write_if_different(&cli_copy, &bytes);
    let mux_copy = root.join("crates/fno/src/harness_capabilities.toml");
    if !mux_copy.is_file() {
        return;
    }
    write_if_different(&mux_copy, &bytes);
}

/// PRODUCE the downstream copy of the merge-posture carrier table (x-8151).
///
/// Same shape as [`sync_harness_capabilities`]: the canonical TOML lives in
/// this crate (`merge_posture.rs` `include_str!`s it), and the Python package
/// reads a byte copy as package data so a binary-less interpreter still
/// resolves the carrier vocabulary. The only copy is the Python tree's, so
/// this is one write, not two.
fn sync_merge_posture() {
    println!("cargo:rerun-if-changed=src/merge_posture.toml");
    let canonical = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/merge_posture.toml");
    let Ok(bytes) = std::fs::read(&canonical) else {
        return;
    };
    let Some(root) = repo_root() else { return };
    let cli_copy = root.join("cli/src/fno/agents/merge_posture.toml");
    if !cli_copy.is_file() {
        return;
    }
    write_if_different(&cli_copy, &bytes);
}

/// PRODUCE `src/events_limits.toml` from the Python-owned event schema.
///
/// `cli/src/fno/events/schema.yaml` is canonical and Python reads its `limits`
/// block at runtime. Rust used to MIRROR the two scalars as literals in
/// `verify_evidence.rs`, linked only by a comment; you cannot generate from a
/// comment. Now the build renders the block into a tracked TOML sibling that
/// `events_limits.rs` `include_str!`s, so the committed file is what a
/// crates.io build compiles against and the link is a real dependency edge.
///
/// No-op when the schema is absent (tarball case) and on any parse failure: the
/// committed file is then the value, and `scripts/ci/check-events-limits-fresh.sh`
/// is the tripwire against a hand edit.
fn sync_events_limits() {
    let generated = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/events_limits.toml");
    let Some(root) = repo_root() else { return };
    let schema = root.join("cli/src/fno/events/schema.yaml");
    if !schema.is_file() {
        return;
    }
    println!("cargo:rerun-if-changed={}", schema.display());
    let Ok(text) = std::fs::read_to_string(&schema) else {
        return;
    };
    let parsed: serde_yaml_ng::Value = match serde_yaml_ng::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            println!("cargo:warning=fno-agents build: schema.yaml did not parse: {err}");
            return;
        }
    };
    let limits = &parsed["limits"];
    let (Some(max_data_bytes), Some(encoding)) = (
        limits["max_data_bytes"].as_u64(),
        limits["data_size_encoding"].as_str(),
    ) else {
        println!("cargo:warning=fno-agents build: schema.yaml limits block incomplete");
        return;
    };
    write_if_different(
        &generated,
        render_events_limits(max_data_bytes, encoding).as_bytes(),
    );
}

/// Generate the shared latest-attempt selector for both runtime languages.
fn sync_check_supersession() {
    let contract_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("codegen/check_supersession.toml");
    println!("cargo:rerun-if-changed={}", contract_path.display());
    println!("cargo:rerun-if-changed=codegen/check_supersession.rs");
    let contract = check_supersession_codegen::load_contract(&contract_path)
        .expect("check_supersession.toml must parse");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR must be set"));
    std::fs::write(
        out_dir.join("check_supersession.rs"),
        check_supersession_codegen::render_rust(&contract),
    )
    .expect("generated Rust supersession source must be writable");

    let Some(root) = repo_root() else { return };
    let cli = root.join("cli");
    if cli.is_dir() {
        write_if_different(
            &cli.join("src/fno/pr/_check_supersession_generated.py"),
            check_supersession_codegen::render_python(&contract).as_bytes(),
        );
    }
}

/// Render the generated `events_limits.toml` body. The CI tripwire renders the
/// same shape from the same source, so the two can be read against each other.
fn render_events_limits(max_data_bytes: u64, encoding: &str) -> String {
    format!(
        "# GENERATED by crates/fno-agents/build.rs from cli/src/fno/events/schema.yaml.\n\
         # Do not edit. Change the limits block in schema.yaml and rebuild.\n\
         max_data_bytes = {max_data_bytes}\n\
         data_size_encoding = \"{encoding}\"\n"
    )
}

/// Run a command, returning trimmed stdout on a zero exit, else `None`.
fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}
