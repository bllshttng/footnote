//! The supervisor half of the footnote endpoint: resolve the borrowed model
//! endpoint from the route env and `config.model_routing`, then hand it to the
//! `footnote` binary inside the launch spec.

use crate::footnote_transcript::EndpointSpec;
use std::path::Path;

/// The host part of a base URL, for the refusals below.
fn host(base_url: &str) -> &str {
    let rest = base_url.split("://").nth(1).unwrap_or(base_url);
    rest.split('/').next().unwrap_or("")
}

/// Resolve the endpoint: the spawn front's route env first, then the
/// `config.model_routing.providers.<FNO_ROUTE_PROVIDER>` record. Never a
/// Claude.ai login: an Anthropic host with no API key refuses.
pub fn resolve_endpoint(
    cwd: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<EndpointSpec, String> {
    let get = |k: &str| env(k).filter(|v| !v.is_empty());
    let route_provider = get(crate::codex_route::ROUTE_PROVIDER_ENV);
    let ep = if let Some(base_url) = get("ANTHROPIC_BASE_URL") {
        let (key, bearer) = match (get("ANTHROPIC_AUTH_TOKEN"), get("ANTHROPIC_API_KEY")) {
            (Some(t), _) => (t, true),
            (None, Some(k)) => (k, false),
            (None, None) => (String::new(), false),
        };
        EndpointSpec {
            base_url,
            key,
            bearer,
            wire: "anthropic".into(),
            provider_id: route_provider,
            route: "env".into(),
        }
    } else if let Some(provider) = route_provider {
        let table = crate::agents_config::config_table_merged(
            cwd,
            &["model_routing", "providers", &provider],
        );
        // The zero-config zai lane: the same built-in record Python's
        // model_routing `_DEFAULT_PROVIDERS` carries; a config field wins.
        let builtin = |n: &str| match (provider.as_str(), n) {
            ("zai", "base_url") => Some("https://api.z.ai/api/anthropic"),
            ("zai", "api_key_env") => Some("ZAI_API_KEY"),
            ("zai", "api_key_file") => Some("~/.fno/.env"),
            _ => None,
        };
        if table.is_none() && builtin("base_url").is_none() {
            return Err(format!(
                "provider {provider:?} is not configured: add config.model_routing.providers.{provider}"
            ));
        }
        let field = |n: &str| {
            table
                .as_ref()
                .and_then(|t| t.get(n))
                .and_then(toml::Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
                .or_else(|| builtin(n).map(str::to_string))
        };
        let openai = field("protocol")
            .unwrap_or_else(|| "anthropic".into())
            .eq_ignore_ascii_case("openai");
        let base_url = field("base_url")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("provider {provider:?} has no base_url"))?;
        let key_env = field("api_key_env").unwrap_or_else(|| {
            if openai {
                "OPENAI_API_KEY"
            } else {
                "ANTHROPIC_API_KEY"
            }
            .into()
        });
        let key = crate::provider_key::resolve_key(&key_env, field("api_key_file").as_deref())
            .ok_or_else(|| format!("provider {provider:?} has no API key under {key_env:?}"))?;
        EndpointSpec {
            base_url,
            key,
            bearer: key_env != "ANTHROPIC_API_KEY",
            wire: if openai { "openai" } else { "anthropic" }.into(),
            provider_id: Some(provider),
            route: "config".into(),
        }
    } else {
        return Err(
            "no model endpoint: spawn with -P <provider> or set ANTHROPIC_BASE_URL and a key"
                .into(),
        );
    };
    if host(&ep.base_url).ends_with("api.anthropic.com") && (ep.bearer || ep.key.is_empty()) {
        return Err("api.anthropic.com needs an API key (ANTHROPIC_API_KEY); a Claude.ai login runs only in the Claude Code lane (-H claude)".into());
    }
    if ep.key.is_empty() {
        return Err(format!("no API key for {}", host(&ep.base_url)));
    }
    Ok(ep)
}
