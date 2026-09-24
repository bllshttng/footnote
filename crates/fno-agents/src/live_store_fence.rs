//! A build from a linked feature worktree refuses to open an operator store.
//!
//! Every writable SQLite open routes through [`refuse_worktree_build_on_operator_store`]
//! before `Connection::open`: a binary whose nearest `.git` ancestor is a FILE
//! (the linked-worktree marker) may not open a store under the passwd home's
//! `.fno`, because its own open-time schema and import steps would run on the
//! operator's store. The canonical checkout (`.git` a directory) and any
//! deployed binary (no `.git` ancestor) are exempt, as is every store outside
//! the home `.fno` or inside the worktree itself. Read-only opens stay
//! unfenced: they cannot migrate a store. There is no bypass flag; the remedy
//! is a different binary or a different store.

use std::path::{Path, PathBuf};

pub fn refuse_worktree_build_on_operator_store(store: &Path) -> Result<(), String> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok());
    let home = passwd_home();
    refusal(exe.as_deref(), home.as_deref(), store)
}

/// The pure rule. `Ok` whenever the fence cannot place the process: no exe,
/// no passwd home, no `.git` ancestor (deployed), a `.git` DIRECTORY
/// (canonical checkout), a store outside the home `.fno`, or a store under
/// the worktree itself (the checkout's own state, not the operator's).
fn refusal(exe: Option<&Path>, home: Option<&Path>, store: &Path) -> Result<(), String> {
    let Some(exe) = exe else {
        return Ok(());
    };
    let Some(home) = home else {
        return Ok(());
    };
    let Some((worktree, git_is_dir)) = nearest_git_ancestor(exe) else {
        return Ok(());
    };
    if git_is_dir {
        return Ok(());
    }
    let worktree = canonical_or_self(&worktree);
    let home_fno = canonical_or_self(&home.join(".fno"));
    let store = canonical_or_self(store);
    if !store.starts_with(&home_fno) || store.starts_with(&worktree) {
        return Ok(());
    }
    Err(format!(
        "refusing to open the live store {} from a feature-worktree build ({}, worktree {}): \
         this build's store code would run its open-time schema and import steps on the \
         operator's store. Run the deployed binary (fno doctor update), or point this checkout \
         at its own store.",
        store.display(),
        exe.display(),
        worktree.display()
    ))
}

/// The nearest ancestor of `exe` holding `.git`, with whether that `.git` is
/// a directory (canonical checkout) or a file (linked worktree).
fn nearest_git_ancestor(exe: &Path) -> Option<(PathBuf, bool)> {
    let mut candidate = exe.parent()?;
    loop {
        let git = candidate.join(".git");
        if git.exists() {
            return Some((candidate.to_path_buf(), git.is_dir()));
        }
        candidate = candidate.parent()?;
    }
}

/// Canonical form with a fallback for paths that do not fully exist yet:
/// canonicalize the deepest existing ancestor and re-append the missing tail,
/// so a store the guarded open is ABOUT to create still compares equal to its
/// canonical neighbors even when several leading directories are new.
fn canonical_or_self(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = path.to_path_buf();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&probe) {
            return missing
                .iter()
                .rev()
                .fold(canonical, |acc, part| acc.join(part));
        }
        match probe.parent() {
            Some(parent) if parent != probe => {
                missing.push(probe.file_name().unwrap_or_default().to_os_string());
                probe = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// The home directory of the real user behind this process, from the passwd
/// database. Never `$HOME`: a test that points `$HOME` at a tempdir must stay
/// outside the fence, while the store it guards does not.
#[cfg(unix)]
fn passwd_home() -> Option<PathBuf> {
    let mut buf = vec![0u8; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut pwd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() || pwd.pw_dir.is_null() {
        return None;
    }
    let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) }
        .to_string_lossy()
        .to_string();
    if dir.is_empty() {
        None
    } else {
        Some(PathBuf::from(dir))
    }
}

#[cfg(not(unix))]
fn passwd_home() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "live-store-fence-{}-{}",
                std::process::id(),
                name
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempRoot(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A worktree shape: `root/.git` is a FILE, and `exe` sits below `root`.
    fn worktree_exe(root: &Path) -> PathBuf {
        std::fs::write(root.join(".git"), "gitdir: elsewhere").unwrap();
        let dir = root.join("target").join("debug");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("worker");
        std::fs::write(&exe, b"").unwrap();
        exe
    }

    /// A canonical checkout shape: `root/.git` is a DIRECTORY.
    fn canonical_exe(root: &Path) -> PathBuf {
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let dir = root.join("target").join("debug");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("worker");
        std::fs::write(&exe, b"").unwrap();
        exe
    }

    #[test]
    fn worktree_exe_on_home_store_is_refused() {
        let root = TempRoot::new("worktree");
        let home = TempRoot::new("home");
        let exe = canonicalize_or_self(&worktree_exe(root.path()));
        let store = home.path().join(".fno").join("graph.db");
        std::fs::create_dir_all(home.path().join(".fno")).unwrap();
        let error = refusal(Some(&exe), Some(home.path()), &store).unwrap_err();
        assert!(error.contains("refusing to open the live store"));
        assert!(error.contains("graph.db"));
        assert!(error.contains(&root.path().to_string_lossy().as_ref()));
    }

    #[test]
    fn canonical_checkout_on_home_store_is_allowed() {
        let root = TempRoot::new("canonical");
        let home = TempRoot::new("home2");
        let exe = canonicalize_or_self(&canonical_exe(root.path()));
        let store = home.path().join(".fno").join("graph.db");
        std::fs::create_dir_all(home.path().join(".fno")).unwrap();
        assert!(refusal(Some(&exe), Some(home.path()), &store).is_ok());
    }

    #[test]
    fn store_outside_home_or_inside_worktree_is_allowed() {
        let root = TempRoot::new("worktree2");
        let home = TempRoot::new("home3");
        let exe = canonicalize_or_self(&worktree_exe(root.path()));
        std::fs::create_dir_all(home.path().join(".fno")).unwrap();
        let outside = TempRoot::new("outside").path().join("events.db");
        assert!(refusal(Some(&exe), Some(home.path()), &outside).is_ok());
        let inside = root.path().join(".fno").join("graph.db");
        std::fs::create_dir_all(root.path().join(".fno")).unwrap();
        assert!(refusal(Some(&exe), Some(home.path()), &inside).is_ok());
    }

    #[test]
    fn unplaceable_process_is_allowed() {
        let root = TempRoot::new("worktree3");
        let home = TempRoot::new("home4");
        let exe = canonicalize_or_self(&worktree_exe(root.path()));
        let store = home.path().join(".fno").join("graph.db");
        std::fs::create_dir_all(home.path().join(".fno")).unwrap();
        assert!(refusal(None, Some(home.path()), &store).is_ok());
        assert!(refusal(Some(&exe), None, &store).is_ok());
    }

    fn canonicalize_or_self(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }
}
