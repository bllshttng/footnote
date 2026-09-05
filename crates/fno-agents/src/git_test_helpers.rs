//! Test-only git helpers. Two tests in this crate flip the process PATH
//! (client_verbs, provider) and the board tests flip HOME under their own
//! lock, so any test spawning `git` by bare name races a mid-flip env and
//! loses at random (NotFound, or a temp HOME's git config). Everything here
//! snapshots what it needs and pins what git reads.

/// git resolved to an absolute path ONCE: the snapshot is immune to PATH
/// flips that happen after it.
///
/// The ONCE is the whole point and it was missing: this re-read PATH on every
/// call, so it was a fresh lookup rather than a snapshot and every caller
/// still raced the flips the module header describes. Only a real hit is
/// cached, so a call that lands inside a stub window returns the bare name for
/// itself without poisoning the snapshot for everyone after it.
pub(crate) fn git_bin() -> std::path::PathBuf {
    static RESOLVED: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    if let Some(found) = RESOLVED.get() {
        return found.clone();
    }
    let found = std::env::var("PATH").ok().and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("git"))
            .find(|p| p.is_file())
    });
    match found {
        Some(path) => RESOLVED.get_or_init(|| path).clone(),
        None => std::path::PathBuf::from("git"),
    }
}

/// `git init -q` with config sources pinned to /dev/null.
pub(crate) fn git_init(repo: &std::path::Path) -> bool {
    std::process::Command::new(git_bin())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["init", "-q"])
        .current_dir(repo)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run git with the snapshot binary and pinned config sources.
pub(crate) fn git_run(
    args: &[&str],
    cwd: &std::path::Path,
) -> std::io::Result<std::process::Output> {
    std::process::Command::new(git_bin())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(args)
        .current_dir(cwd)
        .output()
}
