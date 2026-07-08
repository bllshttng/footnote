//! Shared version reporter for the fno-agents bins (client, daemon, worker).
//!
//! All three bins bake in the same crate-wide build.rs env vars
//! (`FNO_AGENTS_GIT_REV` / `FNO_AGENTS_CRATES_REV` / `FNO_AGENTS_GIT_DIRTY`) via
//! `env!`, so one reporter lets `fno update` interrogate each bin's `crates_rev`
//! and verify the whole triad is the SAME build, not merely present.

use serde_json::json;

/// The machine-readable version payload `fno doctor` / `fno update` read off a
/// resolved binary (`<bin> version --json`). `crates_rev` is the crates/ subtree
/// rev the rust-staleness verdict keys on (ab-716cd330) -- the same quantity
/// Python's `update._rust_subtree_rev` computes, so the comparison is
/// apples-to-apples. `git_rev` stays the full HEAD the binary was built from.
pub fn version_json() -> serde_json::Value {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    json!({
        "package": env!("CARGO_PKG_VERSION"),
        "git_rev": env!("FNO_AGENTS_GIT_REV"),       // full sha, or the literal "unknown"
        "crates_rev": env!("FNO_AGENTS_CRATES_REV"), // crates/ subtree rev, or "unknown"
        "dirty": env!("FNO_AGENTS_GIT_DIRTY") == "1",
        "profile": profile,
    })
}

/// Print the binary's embedded version. `--json` emits [`version_json`]; the
/// human form is a one-liner. Side-effect-free: never starts the daemon. The
/// human string is crate-generic ("fno-agents") since the lib cannot see
/// `CARGO_BIN_NAME`; `fno update` reads only the name-agnostic `--json` form.
pub fn print_version(json_out: bool) {
    let pkg = env!("CARGO_PKG_VERSION");
    let rev = env!("FNO_AGENTS_GIT_REV"); // full sha, or the literal "unknown"
    let dirty = env!("FNO_AGENTS_GIT_DIRTY") == "1";
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    if json_out {
        println!("{}", version_json());
    } else {
        let short = if rev.len() >= 12 { &rev[..12] } else { rev };
        let suffix = if dirty { "-dirty" } else { "" };
        println!("fno-agents {pkg} ({short}{suffix}, {profile})");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_json_carries_crates_rev_and_git_rev() {
        // build.rs always sets both env vars (falling back to "unknown"), so the
        // keys are present for `fno doctor` to read the rust-staleness signal
        // off the resolved binary without an external marker.
        let v = version_json();
        assert!(v.get("crates_rev").and_then(|x| x.as_str()).is_some());
        assert!(v.get("git_rev").and_then(|x| x.as_str()).is_some());
    }
}
