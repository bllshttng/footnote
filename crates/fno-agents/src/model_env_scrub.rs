//! Mirror of the Python inherited-model-env scrub
//! (`cli/src/fno/agents/model_routing.py`). The Python seam covers the `fno`
//! front door; the compiled client is reachable without it (`fno-agents
//! spawn`, the loop runtime), and both Rust claude spawn arms would otherwise
//! hand the child the parent env verbatim. A Python test parses this file's
//! `MODEL_ENV_KEYS` mirror and pins it to the Python tuple, the same
//! anti-drift move the SessionStart hook's var list uses.
//!
//! A strip, never a refusal: a long-lived daemon stamps the env of its first
//! shell into every child it spawns, and a refusal at 3am kills a loop over a
//! condition the child cannot fix from inside itself. A real route is never
//! stripped: a foreign base URL serves those model ids, so the predicate
//! returns empty and the composed overlay reaches the child unchanged.

use std::path::PathBuf;

/// The five model vars, one list with the Python `MODEL_ENV_KEYS`.
pub const MODEL_ENV_KEYS: [&str; 5] = [
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
];

/// Bare tier aliases resolve to Anthropic models; ids start with `claude-`.
const TIER_ALIASES: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];

fn is_anthropic_model(model: &str) -> bool {
    let name = model.trim().to_ascii_lowercase();
    name.starts_with("claude-") || TIER_ALIASES.contains(&name.as_str())
}

fn env_truthy(value: Option<&str>) -> bool {
    match value.map(str::trim) {
        None => false,
        Some("") => false,
        Some(v) => !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
    }
}

/// Host of a base URL with scheme, path, and port stripped; empty when unset.
/// Splits on the FIRST "://" (mirrors Python's `split("://", 1)[-1]`), not
/// the last: a proxy URL that embeds a second URL in its path
/// (`https://gateway.corp/proxy/https://api.anthropic.com`) must resolve to
/// the proxy's own host, not the embedded one.
fn base_url_host(base: &str) -> String {
    let lower = base.trim().to_ascii_lowercase();
    let after_scheme = lower
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(lower.as_str());
    after_scheme
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_string()
}

/// True when the endpoint is Anthropic's: base URL unset, empty, or an
/// anthropic.com host (exact or subdomain - never a substring glob, so
/// notanthropic.com is a foreign endpoint).
pub fn base_url_is_anthropic(get: &dyn Fn(&str) -> Option<String>) -> bool {
    match get("ANTHROPIC_BASE_URL") {
        Some(raw) if !raw.trim().is_empty() => {
            let host = base_url_host(&raw);
            host == "anthropic.com" || host.ends_with(".anthropic.com")
        }
        _ => true,
    }
}

/// Every model var carrying a non-Anthropic id while the endpoint is
/// Anthropic's. Bedrock and Vertex return empty (their ids lack the
/// `claude-` prefix and leave the base URL unset, so the question does not
/// apply), and a foreign base URL returns empty (the endpoint serves those
/// ids, so a route is never stripped).
pub fn incoherent_model_env(get: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    if env_truthy(get("CLAUDE_CODE_USE_BEDROCK").as_deref())
        || env_truthy(get("CLAUDE_CODE_USE_VERTEX").as_deref())
    {
        return Vec::new();
    }
    if !base_url_is_anthropic(get) {
        return Vec::new();
    }
    MODEL_ENV_KEYS
        .iter()
        .filter_map(|key| {
            let value = get(key)?.trim().to_string();
            if value.is_empty() || is_anthropic_model(&value) {
                None
            } else {
                Some(((*key).to_string(), value))
            }
        })
        .collect()
}

/// Every `MODEL_ENV_KEYS` name carrying a value - the model claims the
/// environment is making, coherent or not - but only while the endpoint is
/// Anthropic's own. Mirror of Python's `unrouted_model_keys`: a child's model
/// claim may come from a composed route or an account overlay that pins one,
/// never from the launching shell. The endpoint guard matches
/// `incoherent_model_env` for the same reason: a foreign base serving foreign
/// ids is a hand-composed route the operator built in their shell, and
/// stripping it would break a working lane; Bedrock/Vertex pins are deliberate.
pub fn unrouted_model_keys(get: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    if env_truthy(get("CLAUDE_CODE_USE_BEDROCK").as_deref())
        || env_truthy(get("CLAUDE_CODE_USE_VERTEX").as_deref())
    {
        return Vec::new();
    }
    if !base_url_is_anthropic(get) {
        return Vec::new();
    }
    MODEL_ENV_KEYS
        .iter()
        .filter(|key| matches!(get(key).map(|v| v.trim().to_string()), Some(v) if !v.is_empty()))
        .map(|key| (*key).to_string())
        .collect()
}

/// The stderr line for the unrouted clear - its own sentence, because the
/// incoherence notice's cause is false for a coherent claim. Mirror of
/// Python's `unrouted_model_clear_notice`.
fn unrouted_model_clear_notice(cleared: &[String]) -> String {
    format!(
        "fno: cleared {} from this child's env: an unrouted child carries no \
         model claim, so it runs on its account's own default rather than a \
         model inherited from the launching shell. Select one with --model or \
         a config.agents.profiles entry to route it.",
        cleared.join(", ")
    )
}

/// Scrub the incoherent model vars off a child `Command` and emit the one
/// stderr line naming them. Called BEFORE any overlay env is set on the same
/// command, so a route or account that re-supplies a var still wins
/// (`Command::env` after `env_remove` is last-wins). `overlay` is that
/// about-to-be-applied env: an overlay re-supplying a model var changes the
/// notice's fallback sentence (Python's `routed=` kwarg), because "falls back
/// to its account's own default" is false for that child. When no overlay
/// re-supplies a model var, the same call clears every inherited model claim
/// (see `unrouted_model_keys`) and prints the unrouted-claim line - nothing
/// puts those vars back, so leaving them hands the child a model nobody
/// selected for it.
pub fn scrub_onto(cmd: &mut std::process::Command, overlay: &[(&str, &str)]) {
    let dropped = incoherent_model_env(&|k| std::env::var(k).ok());
    for (key, _) in &dropped {
        cmd.env_remove(key);
    }
    let routed = overlay.iter().any(|(k, _)| MODEL_ENV_KEYS.contains(k));
    if !routed {
        let claimed = unrouted_model_keys(&|k| std::env::var(k).ok());
        for key in &claimed {
            cmd.env_remove(key);
        }
        if !claimed.is_empty() {
            eprintln!("{}", unrouted_model_clear_notice(&claimed));
        }
    }
    if !dropped.is_empty() {
        let names = dropped
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let fallback = if routed {
            "The child receives that route's own model instead."
        } else {
            "The child falls back to its account's own default."
        };
        eprintln!(
            "fno: dropped {names} from this child's env: they name a non-Anthropic model \
             while ANTHROPIC_BASE_URL is unset or names an anthropic.com host, so the \
             child would ask Anthropic for a model it does not serve and every call on \
             that tier would error. {fallback} \
             This env was inherited, usually from a long-lived `claude` background daemon \
             started from a shell that held those exports; no config edit clears a \
             running daemon. Pin the tier defaults in ~/.claude/settings.json `env` \
             (that wins over an inherited value) or restart the daemon."
        );
    }
}

/// Write a `--settings` JSON flooring `dropped` model vars to "" under the
/// agents root's `route-settings/` dir and return its path. The Rust-side twin
/// of Python's `materialize_model_scrub_settings`: a `claude --bg` serving
/// session is forked by the claude daemon with the DAEMON's own env, so an env
/// scrub on the front-end command never reaches it - only a settings file
/// does. Content-addressed (blake3 of the payload) and mode 0600, matching the
/// Python writer's contract; a route overlay present in the env makes
/// `incoherent_model_env` empty, so this file can only be the unrouted floor.
pub fn write_scrub_settings(dropped: &[(String, String)]) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut env = serde_json::Map::new();
    for (key, _) in dropped {
        env.insert(key.clone(), serde_json::Value::String(String::new()));
    }
    let payload = serde_json::to_string(&serde_json::json!({ "env": env }))?;
    let digest = blake3::hash(payload.as_bytes()).to_hex();
    let dir = crate::paths::AgentsHome::from_env()
        .root()
        .join("route-settings");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("model-scrub-{}.json", &digest[..16]));
    let tmp = dir.join(format!(".{}.tmp", &digest[..16]));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(payload.as_bytes())?;
    f.sync_all().ok();
    drop(f);
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn poisoned_env_names_every_offender() {
        let get = env_of(&[
            ("ANTHROPIC_MODEL", "glm-5.2[1m]"),
            ("ANTHROPIC_DEFAULT_HAIKU_MODEL", "glm-4.5-air"),
        ]);
        let found = incoherent_model_env(&get);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, "ANTHROPIC_MODEL");
        assert_eq!(found[1].0, "ANTHROPIC_DEFAULT_HAIKU_MODEL");
    }

    #[test]
    fn scrub_settings_floors_exactly_the_dropped_vars() {
        // A bg serving session is forked with the DAEMON's env, so only this
        // file reaches it: it must floor each dropped var to "" (claude reads
        // an empty settings value as unset) under the agents tree, and the
        // write must be repeatable (content-addressed, same path twice).
        // The crate-wide env lock: FNO_AGENTS_HOME is process-global and the
        // unit tests share one process.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "fno-scrub-settings-{}-{}",
            std::process::id(),
            line!()
        ));
        std::env::set_var("FNO_AGENTS_HOME", dir.join("agents"));
        let dropped = vec![
            ("ANTHROPIC_MODEL".to_string(), "glm-5.2[1m]".to_string()),
            (
                "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
                "glm-4.5-air".to_string(),
            ),
        ];
        let path = write_scrub_settings(&dropped).expect("settings floor writes");
        let raw = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let env = v.get("env").unwrap().as_object().unwrap();
        assert_eq!(env.len(), 2);
        assert_eq!(env["ANTHROPIC_MODEL"], "");
        assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "");
        assert!(path.starts_with(dir.join("agents/route-settings")));
        assert_eq!(write_scrub_settings(&dropped).unwrap(), path);
        std::env::remove_var("FNO_AGENTS_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_real_route_is_never_stripped() {
        let get = env_of(&[
            ("ANTHROPIC_MODEL", "glm-5.2[1m]"),
            ("ANTHROPIC_BASE_URL", "https://api.z.ai/api/anthropic"),
        ]);
        assert!(incoherent_model_env(&get).is_empty());
        assert!(!base_url_is_anthropic(&get));
    }

    #[test]
    fn bedrock_and_vertex_lanes_are_coherent() {
        let get = env_of(&[
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            (
                "ANTHROPIC_MODEL",
                "us.anthropic.claude-sonnet-4-20250514-v1:0",
            ),
        ]);
        assert!(incoherent_model_env(&get).is_empty());
    }

    #[test]
    fn an_off_word_is_not_a_lane_opt_in() {
        let get = env_of(&[
            ("CLAUDE_CODE_USE_BEDROCK", "0"),
            ("ANTHROPIC_MODEL", "glm-5.2[1m]"),
        ]);
        assert_eq!(incoherent_model_env(&get).len(), 1);
    }

    #[test]
    fn a_lookalike_host_is_a_foreign_endpoint() {
        let get = env_of(&[
            ("ANTHROPIC_MODEL", "glm-5.2[1m]"),
            ("ANTHROPIC_BASE_URL", "https://notanthropic.com/api"),
        ]);
        assert!(incoherent_model_env(&get).is_empty());
    }

    #[test]
    fn unrouted_keys_name_every_claim_coherent_or_not() {
        let get = env_of(&[
            ("ANTHROPIC_MODEL", "claude-opus-4-8"),
            ("ANTHROPIC_DEFAULT_HAIKU_MODEL", "  "),
        ]);
        // The coherent claim and only it: whitespace-only is no claim.
        assert_eq!(
            unrouted_model_keys(&get),
            vec!["ANTHROPIC_MODEL".to_string()]
        );
    }

    #[test]
    fn scrub_onto_clears_a_coherent_claim_when_no_overlay_restores_it() {
        // The env lock because scrub_onto reads std::env directly. The base is
        // pinned EMPTY (Anthropic's endpoint): a routed test shell exports a
        // foreign base, which is the hand-composed-route case and stands the
        // clear down on purpose.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prior_base = std::env::var("ANTHROPIC_BASE_URL").ok();
        std::env::set_var("ANTHROPIC_BASE_URL", "");
        std::env::set_var("ANTHROPIC_MODEL", "claude-opus-4-8");
        let mut cmd = std::process::Command::new("claude");
        scrub_onto(&mut cmd, &[]);
        // get_envs reports env_remove as a (key, None) pair - assert the
        // POSITIVE marker: the key is present as explicitly removed.
        let removed: Vec<_> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().to_string())
            .collect();
        std::env::remove_var("ANTHROPIC_MODEL");
        match prior_base {
            Some(v) => std::env::set_var("ANTHROPIC_BASE_URL", v),
            None => std::env::remove_var("ANTHROPIC_BASE_URL"),
        }
        assert!(removed.contains(&"ANTHROPIC_MODEL".to_string()));
    }

    #[test]
    fn a_hand_composed_foreign_route_is_never_cleared() {
        // The inverse, positive: over a foreign base the ambient tier remap is
        // a working route the operator built in their shell (the headless
        // receipt resolves it as the spawn's real model), so the unrouted
        // clear stands down - no removal on the model key.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prior_base = std::env::var("ANTHROPIC_BASE_URL").ok();
        std::env::set_var("ANTHROPIC_BASE_URL", "https://api.z.ai/api/anthropic");
        std::env::set_var("ANTHROPIC_DEFAULT_OPUS_MODEL", "glm-5.2");
        let mut cmd = std::process::Command::new("claude");
        scrub_onto(&mut cmd, &[]);
        let removed: Vec<_> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().to_string())
            .collect();
        std::env::remove_var("ANTHROPIC_DEFAULT_OPUS_MODEL");
        match prior_base {
            Some(v) => std::env::set_var("ANTHROPIC_BASE_URL", v),
            None => std::env::remove_var("ANTHROPIC_BASE_URL"),
        }
        assert!(
            !removed.iter().any(|k| k.contains("MODEL")),
            "a hand-composed route must not be stripped, got removals {removed:?}"
        );
    }

    #[test]
    fn scrub_onto_keeps_a_routed_overlay_in_charge() {
        // With an overlay re-supplying the model var, the unrouted clear
        // stands down entirely - no env_remove may fire on the routed key.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        std::env::set_var("ANTHROPIC_MODEL", "glm-5.3[1m]");
        let mut cmd = std::process::Command::new("claude");
        scrub_onto(&mut cmd, &[("ANTHROPIC_MODEL", "glm-5.3[1m]")]);
        let removed: Vec<_> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().to_string())
            .collect();
        std::env::remove_var("ANTHROPIC_MODEL");
        assert!(
            !removed.contains(&"ANTHROPIC_MODEL".to_string()),
            "a routed overlay must own the key, got removals {removed:?}"
        );
    }

    #[test]
    fn an_anthropic_tier_pin_is_coherent() {
        let get = env_of(&[("ANTHROPIC_DEFAULT_OPUS_MODEL", "claude-opus-4-1")]);
        assert!(incoherent_model_env(&get).is_empty());
    }

    #[test]
    fn a_proxy_url_with_an_embedded_url_resolves_the_proxys_own_host() {
        // base_url_host must split on the FIRST "://", matching Python's
        // split("://", 1)[-1]: a proxy URL that carries a second URL in its
        // path (https://gateway.corp/proxy/https://api.anthropic.com) is a
        // foreign endpoint at gateway.corp, not api.anthropic.com. Splitting
        // on the LAST "://" (rsplit) resolved the embedded host instead and
        // wrongly stripped a working routed model var.
        let get = env_of(&[
            ("ANTHROPIC_MODEL", "glm-5.2[1m]"),
            (
                "ANTHROPIC_BASE_URL",
                "https://gateway.corp.com/proxy/https://api.anthropic.com",
            ),
        ]);
        assert!(!base_url_is_anthropic(&get));
        assert!(incoherent_model_env(&get).is_empty());
    }
}
