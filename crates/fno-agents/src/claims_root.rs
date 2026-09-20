//! Which claims root does one claim key live under: the global root for a
//! global-id prefix, else the repo's space (the ported half of Python
//! `fno.claims.io.claims_dir`). Split out of `claims.rs`: that file is over
//! the 5,000-line budget and shrink-only, and the root-resolution question
//! outgrew living beside the acquire/reap machinery. `claims.rs` re-exports
//! the public names, so callers keep their paths.

use std::path::{Path, PathBuf};

/// The claims directory spelled under a root or the global root. The space
/// branch is the ONE exception: it joins `claims` directly.
pub(crate) const CLAIMS_DIRNAME: &str = ".fno/claims";

/// Claim prefixes whose identifier is globally unique (mirrors
/// `io._GLOBAL_ID_PREFIXES`): these coordinate across worktrees/repos via the
/// global root, never a cwd-local dir.
///
/// `flight:` is here because the fan-out it latches is machine-wide: the seven
/// concurrent `agents truth` children that motivated it came from five parents
/// in different worktrees. A cwd-local root would give each of them its own
/// lock and dedupe nothing.
const GLOBAL_ID_PREFIXES: &[&str] = &[
    "node",
    "dispatch",
    "reconcile",
    "session",
    "groom",
    "update",
    "config-optout",
    "flight",
    "gate",
    // `worker:<name>`, the spawn gate's provider-lane reservation: the gate
    // mints it under global_claims_root() (gate_claims_root), so a root-less
    // reader resolves the same file the gate wrote.
    "worker",
    // `test:suite` (test_run.rs): a caller with no explicit `--claims-root`
    // and no FNO_CLAIMS_ROOT/HOME in its environment must not hard-fail the
    // claim lookup - it degrades to the machine-wide root like every other
    // global key, never to a refusal that no root can be found.
    "test",
    // `build:cargo` (test_run.rs build-admit): one cargo build per machine.
    "build",
];

/// The global claims ROOT: `$FNO_CLAIMS_ROOT`, else `$HOME`. A set-but-EMPTY
/// env value is UNSET (falls to `$HOME`) — Python's `os.environ.get` returns
/// the empty string, which is falsy there; resolving it here as a real path
/// would silently fork the claims dir (the drive.rs empty-is-unset lesson).
pub fn global_claims_root() -> Option<PathBuf> {
    let claims_root = std::env::var_os("FNO_CLAIMS_ROOT").filter(|v| !v.is_empty());
    crate::paths::refuse_undeclared_home_fallback(
        claims_root.is_some() || crate::paths::test_root_declared(),
        "FNO_CLAIMS_ROOT",
    );
    global_claims_root_from(claims_root, std::env::var_os("HOME"))
}

/// Testable core of [`global_claims_root`]: env values are explicit so the
/// empty-is-unset contract is exercised without mutating process-global env.
pub fn global_claims_root_from(
    claims_root: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let non_empty = |v: std::ffi::OsString| (!v.is_empty()).then_some(v);
    claims_root
        .and_then(non_empty)
        .or_else(|| home.and_then(non_empty))
        .map(PathBuf::from)
}

/// The global claims DIRECTORY (the resolver callers should hold, not a
/// hand-built `<root>/.fno/claims`).
pub fn global_claims_dir() -> Option<PathBuf> {
    global_claims_root().map(|root| root.join(CLAIMS_DIRNAME))
}

/// Resolve the claims ROOT for `key` by prefix (mirrors `io.claims_root_for`):
/// `<prefix>:<id>` with a global-id prefix routes to the global root; a
/// colon-less key or unrecognized prefix returns `None` and `claims_dir`
/// falls back to the repo's space (the ported half of Python's
/// `claims_dir(None)` default).
pub fn claims_root_for(key: &str) -> Option<PathBuf> {
    match key.split_once(':') {
        Some((prefix, _)) if GLOBAL_ID_PREFIXES.contains(&prefix) => global_claims_root(),
        _ => None,
    }
}

pub(crate) fn claims_dir(key: &str, root: Option<&Path>) -> Result<PathBuf, String> {
    let claims_root_env = std::env::var_os("FNO_CLAIMS_ROOT");
    claims_dir_in(
        key,
        root,
        || {
            std::env::current_dir()
                .map_err(|e| format!("no claims root for key {key:?}: cwd unreadable: {e}"))
        },
        claims_root_env,
        None,
    )
}

/// Testable core of [`claims_dir`]: cwd, the override env, and the spaces
/// root are explicit. A repo-local key (no global-id prefix, no explicit
/// root) resolves like Python `fno.claims.io.claims_dir(None)`: the
/// `$FNO_CLAIMS_ROOT` override first (set-and-nonempty only; the
/// empty-is-unset rule), else the repo's space. The resume-attach
/// single-writer lock rides this: the Python wake and the Rust resume route
/// must mint the SAME lockfile or the guard guards nothing.
///
/// `cwd` is a PROVIDER, not a path, and only the space branch calls it. An
/// explicit root and a machine-wide key such as `session:<uuid>` resolved
/// from the root or `$HOME` alone before this fallback existed, and must
/// keep doing so: a process sitting in a deleted directory would otherwise
/// lose every global claim to an unreadable cwd it never needed.
pub(crate) fn claims_dir_in(
    key: &str,
    root: Option<&Path>,
    cwd: impl FnOnce() -> Result<PathBuf, String>,
    claims_root_env: Option<std::ffi::OsString>,
    spaces_root: Option<&Path>,
) -> Result<PathBuf, String> {
    if let Some(r) = root {
        return Ok(r.join(CLAIMS_DIRNAME));
    }
    if let Some(global) = claims_root_for(key) {
        return Ok(global.join(CLAIMS_DIRNAME));
    }
    let non_empty = |v: std::ffi::OsString| (!v.is_empty()).then_some(v);
    if let Some(override_root) = claims_root_env.and_then(non_empty) {
        return Ok(PathBuf::from(override_root).join(CLAIMS_DIRNAME));
    }
    let cwd = &cwd()?;
    // The space branch does NOT use CLAIMS_DIRNAME: Python lands repo-space
    // claims at `<space>/claims` directly, no nested .fno segment
    // (`fno.claims.io.claims_dir`'s space return).
    let base = match spaces_root {
        Some(spaces) => spaces.join(crate::paths::space_slug(
            &crate::paths::canonical_repo_root(cwd)
                .unwrap_or_else(|| crate::paths::worktree_repo_root(cwd)),
        )),
        None => crate::paths::space_dir(cwd),
    };
    Ok(base.join("claims"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_but_empty_claims_root_is_unset() {
        let root = global_claims_root_from(Some(std::ffi::OsString::new()), Some("/home/x".into()));
        assert_eq!(root, Some(PathBuf::from("/home/x")));
        let root = global_claims_root_from(Some("/custom".into()), Some("/home/x".into()));
        assert_eq!(root, Some(PathBuf::from("/custom")));
        assert_eq!(global_claims_root_from(None, None), None);
    }

    #[test]
    fn root_routing_requires_colon_and_known_prefix() {
        // A bare token equal to a prefix must NOT route globally (partition
        // semantics: a global-id key is always "<prefix>:<id>"): it falls to
        // the repo-space fallback, Python `claims_dir(None)`'s answer, never
        // an error. The spaces root is injected, so no process env is read
        // (the binary's parallel tests race FNO_SPACES_DIR pins).
        let spaces =
            std::env::temp_dir().join(format!("fno-routing-spaces-{}", std::process::id()));
        let repo = Path::new("/repo");
        let expected = spaces
            .join(crate::paths::space_slug(&crate::paths::worktree_repo_root(
                repo,
            )))
            .join("claims");
        assert_eq!(
            claims_dir_in("node", None, || Ok(repo.to_path_buf()), None, Some(&spaces)).unwrap(),
            expected
        );
        assert_eq!(
            claims_dir_in(
                "walker:/repo/root",
                None,
                || Ok(repo.to_path_buf()),
                None,
                Some(&spaces)
            )
            .unwrap(),
            expected
        );
        // Explicit root always wins.
        let dir = claims_dir("walker:/repo/root", Some(Path::new("/tmp/x"))).unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/x/.fno/claims"));
    }

    #[test]
    fn a_root_or_global_key_never_reads_the_cwd() {
        // A process whose working directory was deleted still holds every
        // machine-wide claim: those resolve from the explicit root or from
        // $FNO_CLAIMS_ROOT/$HOME and never needed a cwd. The provider here
        // fails on call, so reaching it at all is the failure.
        let boom = || Err("the cwd provider must not run".to_string());
        assert_eq!(
            claims_dir_in(
                "resume-attach:abcd1234",
                Some(Path::new("/tmp/x")),
                boom,
                None,
                None
            )
            .unwrap(),
            PathBuf::from("/tmp/x").join(CLAIMS_DIRNAME)
        );
        let boom = || Err("the cwd provider must not run".to_string());
        let global = global_claims_root().expect("a global root resolves under test");
        assert_eq!(
            claims_dir_in("session:abcd1234", None, boom, None, None).unwrap(),
            global.join(CLAIMS_DIRNAME)
        );
        // The override branch is also cwd-free.
        let boom = || Err("the cwd provider must not run".to_string());
        assert_eq!(
            claims_dir_in(
                "resume-attach:abcd1234",
                None,
                boom,
                Some(std::ffi::OsString::from("/tmp/override")),
                None
            )
            .unwrap(),
            PathBuf::from("/tmp/override").join(CLAIMS_DIRNAME)
        );
    }

    #[test]
    fn the_resume_attach_fallback_lands_where_python_lands() {
        // The resume-attach single-writer key is repo-local: the Python wake
        // holds it through `fno.claims.io.claims_dir(None)`, so the Rust
        // fallback must resolve the SAME file for one session or the guard
        // guards nothing. The expectation is built from Python's rule (the
        // set-and-nonempty `$FNO_CLAIMS_ROOT` override, else the repo's
        // space keyed on the canonical root's slug), never by calling the
        // resolver under test on the global default. The spaces root is
        // injected: process env races other tests in this binary.
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(init.success(), "git init failed");
        let override_root = temp.path().join("override");
        let dir = claims_dir_in(
            "resume-attach:abcd1234",
            None,
            || Ok(repo.clone()),
            Some(override_root.clone().into_os_string()),
            None,
        )
        .unwrap();
        assert_eq!(dir, override_root.join(CLAIMS_DIRNAME));
        // An empty override is UNSET (Python's falsy ""), so the space wins.
        let spaces = temp.path().join("spaces");
        let dir = claims_dir_in(
            "resume-attach:abcd1234",
            None,
            || Ok(repo.clone()),
            Some(std::ffi::OsString::from("")),
            Some(&spaces),
        )
        .unwrap();
        let canonical = crate::paths::canonical_repo_root(&repo).unwrap();
        let expected = spaces
            .join(crate::paths::space_slug(&canonical))
            .join("claims");
        assert_eq!(dir, expected);
    }
}
