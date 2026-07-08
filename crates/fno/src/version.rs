//! Version reporter for the `fno` mux front-door binary.
//!
//! Mirrors the crates/fno-agents reporter: build.rs bakes `FNO_MUX_GIT_REV` /
//! `FNO_MUX_CRATES_REV` / `FNO_MUX_GIT_DIRTY` crate-wide, so `fno version --json`
//! lets `fno update` interrogate the installed mux's crates_rev and detect a
//! present-but-stale front door (the mux analog of the triad same-build check).
//! Env vars are per-crate, so this cannot share fno-agents' module.

use serde_json::json;

/// The machine-readable version payload `fno update` reads off the installed
/// mux (`fno version --json`). `crates_rev` is the crates/ subtree rev - the
/// same quantity Python's `update._rust_subtree_rev` computes - so the gate
/// compares apples-to-apples with the source.
pub fn version_json() -> serde_json::Value {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    json!({
        "package": env!("CARGO_PKG_VERSION"),
        "git_rev": env!("FNO_MUX_GIT_REV"),       // full sha, or the literal "unknown"
        "crates_rev": env!("FNO_MUX_CRATES_REV"), // crates/ subtree rev, or "unknown"
        "dirty": env!("FNO_MUX_GIT_DIRTY") == "1",
        "profile": profile,
    })
}

/// Print the mux's embedded version. `--json` emits [`version_json`]; the human
/// form is a one-liner. Side-effect-free: never spawns a server or attaches.
pub fn print_version(json_out: bool) {
    let pkg = env!("CARGO_PKG_VERSION");
    let rev = env!("FNO_MUX_GIT_REV"); // full sha, or the literal "unknown"
    let dirty = env!("FNO_MUX_GIT_DIRTY") == "1";
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
        println!("fno {pkg} ({short}{suffix}, {profile})");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_json_carries_crates_rev_and_git_rev() {
        // build.rs always sets both env vars (falling back to "unknown"), so the
        // keys are present for `fno update` to read the mux's rev.
        let v = version_json();
        assert!(v.get("crates_rev").and_then(|x| x.as_str()).is_some());
        assert!(v.get("git_rev").and_then(|x| x.as_str()).is_some());
    }
}
