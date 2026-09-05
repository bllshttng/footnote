//! Test-only git helpers. Two tests in this crate flip the process PATH
//! (client_verbs, provider) and the board tests flip HOME under their own
//! lock, so any test spawning `git` by bare name races a mid-flip env and
//! loses at random (NotFound, or a temp HOME's git config). Everything here
//! snapshots what it needs and pins what git reads.

/// git resolved to an absolute path ONCE: the snapshot is immune to PATH
/// flips that happen after it.
pub(crate) fn git_bin() -> std::path::PathBuf {
    std::env::var("PATH")
        .ok()
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join("git"))
                .find(|p| p.is_file())
        })
        .unwrap_or_else(|| std::path::PathBuf::from("git"))
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
