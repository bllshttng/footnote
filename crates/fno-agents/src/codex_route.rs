//! The one codex route owner: identity predicate, config read, key, tokens,
//! splice (x-3954).
//!
//! A routed codex spawn selects its endpoint through three inline `-c` config
//! tokens (`model_providers.<p>={...}`, `model_provider=<p>`, `model=<m>`);
//! the env carries only the key. The spawn stamps the route's IDENTITY on the
//! registry row (`route_provider_id`, `model_name` - identifiers only, never
//! an endpoint or a token), and every relaunch door re-resolves the route from
//! TODAY's config through this module, so the two moments cannot disagree and
//! no secret is ever at rest.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::agents_config::config_table_merged;

/// The env stamp a routed lane writes so a worker can name its own model
/// provider. Twin of Python's `ROUTE_PROVIDER_ENV` (`model_routing.py`).
pub const ROUTE_PROVIDER_ENV: &str = "FNO_ROUTE_PROVIDER";

const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";
const DEFAULT_WIRE_API: &str = "chat";

/// A resolved codex route: the argv tokens select the endpoint, the env pairs
/// carry the key under the name the provider declares plus the provider stamp.
pub struct CodexRoute {
    pub provider: String,
    pub model: String,
    /// The env var the provider declares for its key; the one pair in `env`
    /// whose value is a secret.
    pub key_env: String,
    pub config_args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl CodexRoute {
    /// The env pairs with the key masked - the `--print-command` shape.
    pub fn env_masked(&self) -> Vec<(String, String)> {
        self.env
            .iter()
            .map(|(k, v)| {
                let v = if k == &self.key_env {
                    "<from config>".to_string()
                } else {
                    v.clone()
                };
                (k.clone(), v)
            })
            .collect()
    }
}

/// Why a route did not resolve. `Unrouted` is the deliberate no-op (the
/// provider belongs to the claude lane); `Refused` names a misconfiguration
/// the operator should see. Both carry a key-free reason.
pub enum CodexRouteError {
    Unrouted(String),
    Refused(String),
}

impl CodexRouteError {
    pub fn message(&self) -> &str {
        match self {
            CodexRouteError::Unrouted(m) | CodexRouteError::Refused(m) => m,
        }
    }
}

impl std::fmt::Display for CodexRouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// Some((provider, model)) for a routed codex row; None for an unrouted one;
/// Err when the row names a route but no model.
///
/// A routed codex row is one whose `harness` is `codex` and whose
/// `route_provider_id` is set and is neither empty nor `openai`. `openai` is
/// the id every unrouted codex lane already stamps (`codex_thread_entry.rs`,
/// `codex_ask.rs`), so it reads as unrouted here - and `resolve_codex_route`
/// refuses a routing provider named `openai`, because a relaunch could not
/// tell it from an unrouted row.
pub fn row_route_identity(
    harness: Option<&str>,
    route_provider_id: Option<&str>,
    model_name: Option<&str>,
) -> Result<Option<(String, String)>, String> {
    if harness != Some("codex") {
        return Ok(None);
    }
    let provider = route_provider_id
        .map(str::trim)
        .filter(|p| !p.is_empty() && *p != "openai");
    let Some(provider) = provider else {
        return Ok(None);
    };
    match model_name.map(str::trim).filter(|m| !m.is_empty()) {
        Some(model) => Ok(Some((provider.to_string(), model.to_string()))),
        None => Err(format!(
            "the row names codex route {provider:?} but records no model, so the route cannot be rebuilt"
        )),
    }
}

/// `row_route_identity` then `resolve_codex_route`; `Ok(None)` for an
/// unrouted row.
pub fn resolve_row_route(entry: &Value, cwd: &Path) -> Result<Option<CodexRoute>, String> {
    let harness = entry.get("harness").and_then(Value::as_str);
    let provider = entry.get("route_provider_id").and_then(Value::as_str);
    let model = entry.get("model_name").and_then(Value::as_str);
    match row_route_identity(harness, provider, model) {
        Err(missing_model) => Err(missing_model),
        Ok(None) => Ok(None),
        Ok(Some((p, m))) => resolve_codex_route(cwd, &p, &m)
            .map(Some)
            .map_err(|e| e.message().to_string()),
    }
}

/// Resolve the codex route for a (provider, model) identity from today's
/// config: read the provider record, resolve the key, build the three `-c`
/// tokens. The tokens must equal Python's recorded launch tokens byte for
/// byte - the spawn side and every relaunch side render through here.
pub fn resolve_codex_route(
    cwd: &Path,
    provider: &str,
    model: &str,
) -> Result<CodexRoute, CodexRouteError> {
    // The unrouted sentinel can never be a route: a relaunch could not tell
    // the restored row from a default-endpoint one, which reads as success
    // while billing the wrong account.
    if provider == "openai" {
        return Err(CodexRouteError::Refused(
            "provider name \"openai\" is the unrouted sentinel and cannot name a route".to_string(),
        ));
    }
    if !provider
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(CodexRouteError::Refused(format!(
            "provider name {provider:?} is not a safe codex provider id"
        )));
    }
    let table =
        config_table_merged(cwd, &["model_routing", "providers", provider]).ok_or_else(|| {
            CodexRouteError::Refused(format!("provider {provider:?} is not configured"))
        })?;
    let field = |name: &str| {
        table
            .get(name)
            .and_then(toml::Value::as_str)
            .map(str::to_string)
    };
    // A provider without a protocol belongs to the claude lane (the claude
    // default), never the codex one - the same default Python applies.
    let protocol = field("protocol")
        .unwrap_or_else(|| "anthropic".to_string())
        .to_lowercase();
    if protocol != "openai" {
        return Err(CodexRouteError::Unrouted(format!(
            "provider {provider:?} declares protocol {protocol:?}, which belongs to the claude lane"
        )));
    }
    let base_url = field("base_url").unwrap_or_default();
    if base_url.is_empty() {
        return Err(CodexRouteError::Refused(format!(
            "provider {provider:?} has no base_url"
        )));
    }
    let key_env = field("api_key_env").unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_string());
    let key = resolve_key(&key_env, field("api_key_file").as_deref()).ok_or_else(|| {
        CodexRouteError::Refused(format!(
            "provider {provider:?} has no API key under {key_env:?} (env or api_key_file)"
        ))
    })?;
    let wire_api = field("wire_api").unwrap_or_else(|| DEFAULT_WIRE_API.to_string());
    // Every embedded value becomes a TOML literal string (single-quoted, no
    // escapes); a single quote or a control char would break the literal or
    // survive into the argv. Controlled config values we won't try to escape.
    for value in [
        ("base_url", base_url.as_str()),
        ("env_key", key_env.as_str()),
        ("wire_api", wire_api.as_str()),
        ("provider", provider),
        ("model", model),
    ] {
        if value.1.contains('\'') || value.1.chars().any(|c| c.is_control()) {
            return Err(CodexRouteError::Refused(format!(
                "provider {provider:?} has a {:?} value that cannot be embedded in TOML",
                value.0
            )));
        }
    }
    let config_args = vec![
        "-c".to_string(),
        format!(
            "model_providers.{provider}={{ base_url = '{base_url}', env_key = '{key_env}', wire_api = '{wire_api}' }}"
        ),
        "-c".to_string(),
        format!("model_provider='{provider}'"),
        "-c".to_string(),
        format!("model='{model}'"),
    ];
    let env = vec![
        (key_env.clone(), key),
        (ROUTE_PROVIDER_ENV.to_string(), provider.to_string()),
    ];
    Ok(CodexRoute {
        provider: provider.to_string(),
        model: model.to_string(),
        key_env,
        config_args,
        env,
    })
}

/// Insert the route tokens right after `argv[0]` (the codex binary), so the
/// order is `codex`, route tokens, grant, `--cd`, `resume`, id.
pub fn splice_route(argv: &mut Vec<String>, route: &CodexRoute) {
    argv.splice(1..1, route.config_args.clone());
}

/// Key precedence (twin of `_resolve_key`, `model_routing.py`): the env var
/// named by `api_key_env` wins over the same name read from the
/// `api_key_file` dotenv file. Never returns the empty string.
fn resolve_key(key_env: &str, api_key_file: Option<&str>) -> Option<String> {
    if key_env.is_empty() {
        return None;
    }
    if let Ok(from_env) = std::env::var(key_env) {
        if !from_env.is_empty() {
            return Some(from_env);
        }
    }
    api_key_file.and_then(|file| read_var_from_env_file(file, key_env))
}

/// Port of Python's `read_var_from_env_file` (`cli/src/fno/env_file.py`):
/// skip `#` lines, allow an `export ` prefix, trim, strip the quotes Python's
/// `str.strip` would strip. A missing file or key is None, never fatal.
fn read_var_from_env_file(path_str: &str, key_name: &str) -> Option<String> {
    let path = expand_tilde(path_str)?;
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() == key_name {
            let v = py_strip(value.trim(), '"');
            let v = py_strip(v, '\'');
            return if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
        }
    }
    None
}

/// `str.strip(char)`: repeatedly drop the quote from both ends.
fn py_strip(s: &str, quote: char) -> &str {
    let mut s = s;
    while let Some(rest) = s.strip_prefix(quote) {
        s = rest;
    }
    while let Some(rest) = s.strip_suffix(quote) {
        s = rest;
    }
    s
}

fn expand_tilde(path_str: &str) -> Option<PathBuf> {
    if path_str == "~" {
        return std::env::var_os("HOME").map(PathBuf::from);
    }
    match path_str.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|home| Path::new(&home).join(rest)),
        None => Some(PathBuf::from(path_str)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hermetic config anchor: `<dir>/.fno/config.toml` is the
    /// highest-priority candidate, so a fixture there outranks the operator's
    /// global config for every field it defines.
    struct ConfigFixture {
        dir: PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl ConfigFixture {
        fn write(&self, toml_body: &str) {
            let f = self.dir.join(".fno/config.toml");
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, toml_body).unwrap();
        }

        fn env_file(&self, body: &str) -> PathBuf {
            let p = self.dir.join("keys.env");
            std::fs::write(&p, body).unwrap();
            p
        }

        fn cwd(&self) -> &Path {
            &self.dir
        }
    }

    /// Serializes tests that touch the process env (key precedence reads the
    /// real env vars) and hands each test a fresh temp dir.
    fn fixture() -> ConfigFixture {
        // A global mutex keeps the env mutations below race-free; tests in
        // this module hold it for their whole body.
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "fno-codex-route-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(".fno")).unwrap();
        ConfigFixture { dir, _guard: guard }
    }

    const PROVIDER: &str = r#"
[model_routing.providers.zai-openai]
protocol = "openai"
base_url = "https://api.z.ai/api/coding/paas/v4"
api_key_env = "FNO_TEST_ZAI_KEY"
"#;

    #[test]
    fn tokens_equal_pythons_recorded_launch_tokens() {
        let fx = fixture();
        fx.write(PROVIDER);
        // SAFETY: single-threaded within the fixture mutex.
        std::env::remove_var("FNO_TEST_ZAI_KEY");
        let route = resolve_codex_route(fx.cwd(), "zai-openai", "glm-5.3-flash[1m]").unwrap();
        assert_eq!(
            route.config_args,
            vec![
                "-c",
                "model_providers.zai-openai={ base_url = 'https://api.z.ai/api/coding/paas/v4', env_key = 'FNO_TEST_ZAI_KEY', wire_api = 'chat' }",
                "-c",
                "model_provider='zai-openai'",
                "-c",
                "model='glm-5.3-flash[1m]'",
            ]
        );
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[test]
    fn key_precedence_env_beats_file() {
        let fx = fixture();
        fx.write(PROVIDER);
        let file = fx.env_file("FNO_TEST_ZAI_KEY=from-file\n");
        let body = PROVIDER.replace(
            "api_key_env = \"FNO_TEST_ZAI_KEY\"",
            &format!(
                "api_key_env = \"FNO_TEST_ZAI_KEY\"\napi_key_file = \"{}\"",
                file.display()
            ),
        );
        fx.write(&body);
        std::env::remove_var("FNO_TEST_ZAI_KEY");
        let from_file = resolve_codex_route(fx.cwd(), "zai-openai", "m")
            .unwrap()
            .env[0]
            .1
            .clone();
        assert_eq!(from_file, "from-file");
        std::env::set_var("FNO_TEST_ZAI_KEY", "from-env");
        let from_env = resolve_codex_route(fx.cwd(), "zai-openai", "m")
            .unwrap()
            .env[0]
            .1
            .clone();
        assert_eq!(from_env, "from-env");
        std::env::remove_var("FNO_TEST_ZAI_KEY");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[test]
    fn env_file_reader_matches_python() {
        let fx = fixture();
        let f = fx.env_file("# comment\nexport K1 = 'v one'\nK2=\"v2\"\nK3=\nK1=overwritten-later-line-wins-no-first-wins\n");
        // First match wins, like Python's line loop.
        assert_eq!(
            read_var_from_env_file(f.to_str().unwrap(), "K1"),
            Some("v one".to_string())
        );
        assert_eq!(
            read_var_from_env_file(f.to_str().unwrap(), "K2"),
            Some("v2".to_string())
        );
        assert_eq!(read_var_from_env_file(f.to_str().unwrap(), "K3"), None);
        assert_eq!(read_var_from_env_file(f.to_str().unwrap(), "K4"), None);
        assert_eq!(read_var_from_env_file("/nonexistent/env", "K1"), None);
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[test]
    fn refusals_name_provider_and_missing_piece_and_never_the_key() {
        let fx = fixture();
        fx.write(PROVIDER);
        std::env::remove_var("FNO_TEST_ZAI_KEY");
        let secret = "sk-super-secret-value";
        std::env::set_var("FNO_TEST_ZAI_KEY", secret);
        // Not configured.
        let err = resolve_codex_route(fx.cwd(), "no-such-provider", "m").unwrap_err();
        assert!(err.message().contains("no-such-provider"));
        // Unrouted (claude lane).
        fx.write(&format!(
            "{PROVIDER}\n[model_routing.providers.claude-y]\nprotocol = \"anthropic\"\n"
        ));
        let err = resolve_codex_route(fx.cwd(), "claude-y", "m").unwrap_err();
        assert!(matches!(err, CodexRouteError::Unrouted(_)), "{err}");
        // No base_url.
        fx.write("[model_routing.providers.no-url]\nprotocol = \"openai\"\napi_key_env = \"K\"\n");
        assert!(resolve_codex_route(fx.cwd(), "no-url", "m").is_err());
        // Unsafe provider name + the openai sentinel.
        assert!(resolve_codex_route(fx.cwd(), "openai", "m").is_err());
        assert!(resolve_codex_route(fx.cwd(), "bad name", "m").is_err());
        for err in [
            resolve_codex_route(fx.cwd(), "no-such-provider", "m")
                .unwrap_err()
                .message()
                .to_string(),
            resolve_codex_route(fx.cwd(), "no-url", "m")
                .unwrap_err()
                .message()
                .to_string(),
        ] {
            assert!(!err.contains(secret), "refusal leaked the key: {err}");
        }
        std::env::remove_var("FNO_TEST_ZAI_KEY");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[test]
    fn row_route_identity_predicate() {
        // Unrouted shapes.
        assert_eq!(
            row_route_identity(Some("codex"), Some("openai"), Some("m")).unwrap(),
            None
        );
        assert_eq!(row_route_identity(Some("codex"), None, None).unwrap(), None);
        assert_eq!(
            row_route_identity(Some("claude"), Some("zai"), Some("m")).unwrap(),
            None
        );
        // Routed.
        assert_eq!(
            row_route_identity(Some("codex"), Some("zai-openai"), Some("glm"))
                .unwrap()
                .unwrap(),
            ("zai-openai".to_string(), "glm".to_string())
        );
        // Route named but no model: Err.
        assert!(row_route_identity(Some("codex"), Some("zai-openai"), None).is_err());
        assert!(row_route_identity(Some("codex"), Some("zai-openai"), Some(" ")).is_err());
    }

    #[test]
    fn resolve_row_route_none_for_unrouted_value_entry() {
        let fx = fixture();
        fx.write(PROVIDER);
        let unrouted = serde_json::json!({
            "harness": "codex",
            "route_provider_id": "openai",
            "model_name": "glm",
        });
        assert_eq!(resolve_row_route(&unrouted, fx.cwd()).unwrap(), None);
        let routed = serde_json::json!({
            "harness": "codex",
            "route_provider_id": "zai-openai",
            "model_name": "glm-5.3-flash[1m]",
        });
        let route = resolve_row_route(&routed, fx.cwd()).unwrap().unwrap();
        assert_eq!(route.provider, "zai-openai");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[test]
    fn splice_inserts_right_after_the_binary() {
        let fx = fixture();
        fx.write(PROVIDER);
        std::env::remove_var("FNO_TEST_ZAI_KEY");
        let route = resolve_codex_route(fx.cwd(), "zai-openai", "glm").unwrap();
        let mut argv = vec![
            "codex".to_string(),
            "-c".to_string(),
            "sandbox_workspace_write.writable_roots=['/w']".to_string(),
            "resume".to_string(),
            "sid".to_string(),
        ];
        splice_route(&mut argv, &route);
        assert_eq!(argv[0], "codex");
        assert!(argv[1].starts_with("-c"));
        assert!(argv[2].starts_with("model_providers."));
        // The grant token follows the route tokens.
        assert!(argv[route.config_args.len() + 1].starts_with("sandbox_workspace_write"));
        assert_eq!(argv.last().unwrap(), "sid");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[test]
    fn env_masked_hides_the_key_value_only() {
        let fx = fixture();
        fx.write(PROVIDER);
        std::env::set_var("FNO_TEST_ZAI_KEY", "sk-live-secret");
        let route = resolve_codex_route(fx.cwd(), "zai-openai", "glm").unwrap();
        let masked = route.env_masked();
        assert_eq!(
            masked[0],
            ("FNO_TEST_ZAI_KEY".to_string(), "<from config>".to_string())
        );
        assert_eq!(
            masked[1],
            ("FNO_ROUTE_PROVIDER".to_string(), "zai-openai".to_string())
        );
        let printed = format!("{masked:?}");
        assert!(!printed.contains("sk-live-secret"));
        std::env::remove_var("FNO_TEST_ZAI_KEY");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }
}
