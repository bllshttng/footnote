//! The launch record: what fno must be able to replay when it brings a
//! stopped worker back. One block on the registry row, stamped at every
//! mint, holding only what no existing row field holds: the spawn argv
//! without the seed, the allowlisted non-secret env, the resolved store
//! root and the current cwd. Model, effort, bypass or permission mode and
//! add-dir grants ride INSIDE `argv` as harness-native tokens, the way
//! Claude's roster keeps `dispatch.respawnFlags` - there is no second
//! spelling of a launch axis.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The launch record stamped on a row at birth (schema v33). `source` names
/// the mint that wrote it (`pane`, `keeper`, `bg`, `adopt`, `serve`,
/// `codex-thread`), so a reader can tell a wrapper-built argv from a
/// harness-reported one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LaunchRecord {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_current: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// The env keys a launch record may carry. Everything else is DROPPED at the
/// stamp: credentials, tokens and per-session state never enter the row.
/// These are the identity/store-redirect keys the relaunch must reproduce -
/// an alternate config dir or a pinned pi/cursor model IS the worker's
/// identity; `ANTHROPIC_AUTH_TOKEN` is not.
pub const LAUNCH_ENV_ALLOWLIST: [&str; 8] = [
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_BG_ISOLATION",
    "CODEX_HOME",
    "PI_CODING_AGENT_DIR",
    "PI_CODING_AGENT_SESSION_DIR",
    "FNO_PI_PROVIDER",
    "FNO_PI_MODEL",
    "FNO_CURSOR_AGENT_MODEL",
];

/// Filter a caller env down to the allowlist. Deterministic (BTreeMap), so
/// the same inputs stamp the same record.
pub fn allowlisted_env(env: impl Iterator<Item = (String, String)>) -> BTreeMap<String, String> {
    env.filter(|(k, _)| LAUNCH_ENV_ALLOWLIST.contains(&k.as_str()))
        .collect()
}

/// The argv with the seed tokens removed: the prompt travels on the row's
/// node/ledger side, never in the replayed launch (a resume supplies its own
/// wake message, not the birth seed). Both spellings drop - the bare tail
/// token and the fused `--prompt=<seed>` form.
pub fn argv_without_seed(argv: &[String], seed: &str) -> Vec<String> {
    let fused = format!("--prompt={seed}");
    argv.iter()
        .filter(|t| t.as_str() != seed && t.as_str() != fused)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC4-ERR: credentials drop, the store redirect survives.
    #[test]
    fn the_env_allowlist_keeps_identity_and_drops_secrets() {
        let env = [
            ("ANTHROPIC_AUTH_TOKEN".to_string(), "sk-ant-1".to_string()),
            ("OPENAI_API_KEY".to_string(), "sk-oai-1".to_string()),
            ("CLAUDE_CONFIG_DIR".to_string(), "/tmp/cc".to_string()),
            ("FNO_AGENT_SELF".to_string(), "king".to_string()),
        ];
        let kept = allowlisted_env(env.into_iter());
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(
            kept.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/tmp/cc")
        );
    }

    /// The seed drops in both spellings; every other token rides.
    #[test]
    fn the_seed_leaves_the_argv_in_both_spellings() {
        let argv: Vec<String> = ["claude", "--model", "opus", "--prompt=do the thing"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let kept = argv_without_seed(&argv, "do the thing");
        assert_eq!(
            kept,
            vec![
                "claude".to_string(),
                "--model".to_string(),
                "opus".to_string()
            ]
        );

        let bare: Vec<String> = vec!["codex".to_string(), "do the thing".to_string()];
        let kept = argv_without_seed(&bare, "do the thing");
        assert_eq!(kept, vec!["codex".to_string()]);
    }
}
