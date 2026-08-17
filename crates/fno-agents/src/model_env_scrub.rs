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
fn base_url_host(base: &str) -> String {
    base.trim()
        .to_ascii_lowercase()
        .rsplit("://")
        .next()
        .unwrap_or("")
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

/// Scrub the incoherent model vars off a child `Command` and emit the one
/// stderr line naming them. Called BEFORE any overlay env is set on the same
/// command, so a route or account that re-supplies a var still wins
/// (`Command::env` after `env_remove` is last-wins).
pub fn scrub_onto(cmd: &mut std::process::Command) -> Vec<String> {
    let dropped = incoherent_model_env(&|k| std::env::var(k).ok());
    for (key, _) in &dropped {
        cmd.env_remove(key);
    }
    if !dropped.is_empty() {
        let names = dropped
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "fno: dropped {names} from this child's env: they name a non-Anthropic model \
             while ANTHROPIC_BASE_URL is unset or names an anthropic.com host, so the \
             child would ask Anthropic for a model it does not serve and every call on \
             that tier would error. The child falls back to its account's own default. \
             This env was inherited, usually from a long-lived `claude` background daemon \
             started from a shell that held those exports; no config edit clears a \
             running daemon. Pin the tier defaults in ~/.claude/settings.json `env` \
             (that wins over an inherited value) or restart the daemon."
        );
    }
    dropped.into_iter().map(|(k, _)| k).collect()
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
            ("ANTHROPIC_MODEL", "us.anthropic.claude-sonnet-4-20250514-v1:0"),
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
    fn an_anthropic_tier_pin_is_coherent() {
        let get = env_of(&[("ANTHROPIC_DEFAULT_OPUS_MODEL", "claude-opus-4-1")]);
        assert!(incoherent_model_env(&get).is_empty());
    }
}
