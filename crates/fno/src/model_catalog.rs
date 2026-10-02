//! The models.dev catalog: the whole-model universe behind the composer's
//! `more` rows. The cache is the fetched api.json bytes under
//! `<state>/cache/models-dev.json`. A parse into a typed subset (provider
//! id, name, env, api, npm; model id + name) gives the reach rows their
//! model names. The fetch never blocks the list: the composer spawns
//! `refresh` in the background whenever the cache is stale and reads
//! whatever exists now.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const FETCH_TIMEOUT_SECS: &str = "20";

/// The parsed catalog: every models.dev provider keyed by its catalog id.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub providers: HashMap<String, CatalogProvider>,
}

impl Catalog {
    pub fn get(&self, id: &str) -> Option<&CatalogProvider> {
        self.providers.get(id)
    }
}

/// The typed subset of one models.dev provider. Unknown fields are ignored.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct CatalogProvider {
    #[serde(default)]
    pub name: String,
    /// Key env var names the catalog states; `env[0]` is the reach rule's
    /// default key env.
    #[serde(default)]
    pub env: Vec<String>,
    /// The provider's API base URL, when the catalog states one.
    #[serde(default)]
    pub api: Option<String>,
    /// The SDK package whose protocol the provider speaks
    /// (`@ai-sdk/anthropic`, `@ai-sdk/openai-compatible`, `@ai-sdk/openai`).
    #[serde(default)]
    pub npm: Option<String>,
    #[serde(default)]
    pub models: HashMap<String, CatalogModel>,
}

impl CatalogProvider {
    /// The provider's model ids: the catalog key, or the model's own `id`
    /// field when it disagrees with the key.
    pub fn model_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .models
            .iter()
            .map(|(key, model)| match &model.id {
                Some(id) if !id.is_empty() => id.clone(),
                _ => key.clone(),
            })
            .collect();
        ids.sort();
        ids
    }
}

/// The typed subset of one models.dev model: its id and display name.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct CatalogModel {
    #[serde(default)]
    pub id: Option<String>,

    #[serde(default)]
    pub name: Option<String>,
}

/// One model row's launchability: the user-confirmed 2026-09-26 row states.
/// `Ready` fills the main list; `NoKey` names the missing key (hollow dot)
/// and Enter shows the connect steps; `Unreachable` names the protocol gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelState {
    Ready,
    NoKey { key_env: String, steps: Vec<String> },
    Unreachable { reason: String },
}

/// One configured model choice: `name` is what the model chip shows, `model`
/// is the launch id, and `route`/`provider` preserve its configured route.
/// `verdict: String` became `state: ModelState`; the key fields feed the
/// composer's launch check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelChoice {
    pub name: String,
    pub model: String,
    pub route: String,
    /// Derived from the configured account route; no provider list is baked in.
    pub provider: Option<String>,
    pub state: ModelState,
    pub key_env: Option<String>,
    pub key_file: Option<String>,
}

pub(crate) fn provider_from_route(route: &str) -> Option<String> {
    route
        .split_once('/')
        .map(|(provider, _)| provider.trim())
        .filter(|provider| !provider.is_empty())
        .map(str::to_string)
}

pub(crate) fn parse_configured_account_models(
    stdout: &str,
) -> Result<std::collections::HashMap<String, Vec<ModelChoice>>, String> {
    let value: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|_| "account records response was unreadable".to_string())?;
    let records_value = value
        .get("value")
        .ok_or_else(|| "account records response had no value list".to_string())?;
    if records_value.is_null() {
        return Ok(std::collections::HashMap::new());
    }
    let records = records_value
        .as_array()
        .ok_or_else(|| "account records response had no value list".to_string())?;
    let mut by_harness: std::collections::HashMap<String, Vec<ModelChoice>> =
        std::collections::HashMap::new();
    for record in records {
        let Some(harness) = record.get("harness").and_then(|v| v.as_str()) else {
            continue;
        };
        let route = record
            .get("route")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim();
        let declared_provider = record
            .get("route_provider_id")
            .or_else(|| record.get("provider"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|provider| !provider.is_empty());
        let declared_model = record
            .get("model_name")
            .or_else(|| record.get("model"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|model| !model.is_empty());
        let route_provider = provider_from_route(route);
        let route_model = route.split_once('/').map(|(_, model)| model.trim());
        let provider = declared_provider
            .map(str::to_string)
            .or(route_provider)
            .or_else(|| declared_model.and_then(provider_from_route));
        let model_source = declared_model.or(route_model);
        let Some(model_source) = model_source.filter(|model| !model.is_empty()) else {
            continue;
        };
        let model_id = provider
            .as_ref()
            .and_then(|provider| {
                model_source
                    .strip_prefix(provider)
                    .and_then(|rest| rest.strip_prefix('/'))
            })
            .unwrap_or(model_source)
            .to_string();
        let route = if route.is_empty() {
            provider
                .as_ref()
                .map(|provider| format!("{provider}/{model_id}"))
                .unwrap_or_default()
        } else {
            route.to_string()
        };
        let name = if model_id.is_empty() {
            continue;
        } else {
            model_id.clone()
        };
        let choices = by_harness.entry(harness.to_string()).or_default();
        if choices
            .iter()
            .any(|choice| choice.model == model_id && choice.provider == provider)
        {
            continue;
        }
        choices.push(ModelChoice {
            name,
            model: model_id,
            route,
            provider,
            state: ModelState::Ready,
            key_env: None,
            key_file: None,
        });
    }
    Ok(by_harness)
}

pub(crate) fn parse_opencode_models(stdout: &str) -> Vec<ModelChoice> {
    let mut models = Vec::new();
    for id in stdout.lines().map(str::trim).filter(|id| !id.is_empty()) {
        let Some(provider) = provider_from_route(id) else {
            continue;
        };
        if models.iter().any(|model: &ModelChoice| model.name == id) {
            continue;
        }
        models.push(ModelChoice {
            name: id.to_string(),
            model: id.to_string(),
            route: String::new(),
            provider: Some(provider),
            state: ModelState::Ready,
            key_env: None,
            key_file: None,
        });
    }
    models
}

/// codex's own model catalog cache, maintained by codex under CODEX_HOME
/// (default ~/.codex).
pub(crate) fn codex_models_cache_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".codex"))
        })?;
    Some(home.join("models_cache.json"))
}

/// Parse codex's models_cache.json into (visible, hidden): one ModelChoice
/// per models[].slug whose visibility is not "hide", plus the hidden slugs
/// so the live cache can retire floor entries. A missing or unreadable
/// cache parses to two empty lists: the capability-table floor stands,
/// never an error row.
pub(crate) fn parse_codex_models(text: &str) -> (Vec<ModelChoice>, Vec<String>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return (Vec::new(), Vec::new());
    };
    let Some(models) = value.get("models").and_then(|v| v.as_array()) else {
        return (Vec::new(), Vec::new());
    };
    let mut models_out = Vec::new();
    let mut hidden = Vec::new();
    for model in models {
        let Some(slug) = model.get("slug").and_then(|v| v.as_str()) else {
            continue;
        };
        if model.get("visibility").and_then(|v| v.as_str()) == Some("hide") {
            if !hidden.iter().any(|known: &String| known == slug) {
                hidden.push(slug.to_string());
            }
            continue;
        }
        if models_out
            .iter()
            .any(|choice: &ModelChoice| choice.model == slug)
        {
            continue;
        }
        models_out.push(ModelChoice {
            name: slug.to_string(),
            model: slug.to_string(),
            route: String::new(),
            provider: None,
            state: ModelState::Ready,
            key_env: None,
            key_file: None,
        });
    }
    (models_out, hidden)
}

/// Append `extra` choices whose (model id, provider) pair is new, so the
/// floor list stays first and the live sources (codex's cache, configured
/// account records) fill in without duplicates.
pub(crate) fn merge_model_choices(base: &mut Vec<ModelChoice>, extra: &[ModelChoice]) {
    for choice in extra {
        if base
            .iter()
            .any(|m| m.model == choice.model && m.provider == choice.provider)
        {
            continue;
        }
        base.push(choice.clone());
    }
}

#[cfg(test)]
pub(crate) fn state_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The state root: `FNO_STATE_DIR` when set, else `$HOME/.fno`. The same
/// resolution Python's `fno.paths` applies.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FNO_STATE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".fno"))
        .unwrap_or_else(|| PathBuf::from(".fno"))
}

/// The cache file: the raw api.json bytes.
pub fn cache_path(state: &Path) -> PathBuf {
    state.join("cache").join("models-dev.json")
}

/// True when the cache is missing or older than 24 hours.
pub fn needs_refresh(mtime: Option<SystemTime>, now: SystemTime) -> bool {
    match mtime {
        None => true,
        Some(mtime) => match now.duration_since(mtime) {
            Ok(age) => age > MAX_AGE,
            // A cache stamped in the future is fresh by definition.
            Err(_) => false,
        },
    }
}

/// The one stale-check-and-spawn block the composer ran inline, moved here
/// so the mux server can run the same hour-24 fetch: pricing never depends
/// on a user who never opens the composer. Fire-and-forget: the fetch never
/// blocks the caller, a failed fetch keeps the old cache, and the next
/// hour's stat is the retry.
pub fn refresh_if_stale(state: &Path) {
    let mtime = std::fs::metadata(cache_path(state))
        .and_then(|m| m.modified())
        .ok();
    if !needs_refresh(mtime, std::time::SystemTime::now()) {
        return;
    }
    let spawn_state = state.to_path_buf();
    tokio::spawn(async move {
        let _ = refresh(&spawn_state).await;
    });
}

/// One refresh: fetch to `<dir>/models-dev.json.tmp.<pid>`, parse the tmp
/// file, and rename it over the cache only when the parse succeeds. A failed
/// fetch or parse deletes the tmp file and keeps the old cache. The composer
/// spawns this in the background; it never blocks the picker.
pub async fn refresh(state: &Path) -> Result<(), String> {
    let dir = state.join("cache");
    // A fresh install has no cache/ dir; curl would fail on the tmp path on
    // every open and the catalog would never populate.
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(format!("cache dir unwritable: {e}"));
    }
    let tmp = dir.join(format!("models-dev.json.tmp.{}", std::process::id()));
    let fetched = fetch_to(&tmp).await;
    let committed = fetched.and_then(|()| install_from(&tmp, &cache_path(state)));
    let _ = std::fs::remove_file(&tmp);
    committed
}

/// The fetch leg: curl to `tmp`. Factored for tests, which install fixtures
/// through [`install_from`] instead of the network.
async fn fetch_to(tmp: &Path) -> Result<(), String> {
    let status = tokio::process::Command::new("curl")
        .args([
            "-fsS",
            "--max-time",
            FETCH_TIMEOUT_SECS,
            "-o",
            tmp.to_str().unwrap_or_default(),
        ])
        .arg(MODELS_DEV_URL)
        .output()
        .await
        .map_err(|e| format!("curl failed: {e}"))?;
    if !status.status.success() {
        return Err(format!(
            "curl exited {:?} fetching the model catalog",
            status.status.code()
        ));
    }
    Ok(())
}

/// The commit leg: parse the fetched bytes and rename over the cache only on
/// a good parse; anything else keeps the old cache, deletes the tmp file,
/// and returns the reason.
fn install_from(tmp: &Path, cache: &Path) -> Result<(), String> {
    let outcome = (|| {
        let text =
            std::fs::read_to_string(tmp).map_err(|e| format!("catalog fetch unreadable: {e}"))?;
        parse(&text)?;
        std::fs::rename(tmp, cache).map_err(|e| format!("cache write failed: {e}"))
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    outcome
}

/// Parse the raw api.json into the typed subset. Unknown fields are ignored,
/// so a catalog shape drift never blocks the picker.
pub fn parse(text: &str) -> Result<Catalog, String> {
    let providers: HashMap<String, CatalogProvider> =
        serde_json::from_str(text).map_err(|e| format!("catalog unparseable: {e}"))?;
    Ok(Catalog { providers })
}

/// Read the cache now: the parsed catalog, or the one-line reason. The
/// reason names the cache path, so an offline first run names the one thing
/// that would fix it.
pub fn load(state: &Path) -> Result<Catalog, String> {
    let path = cache_path(state);
    let text = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "model catalog not fetched yet; reopen to retry ({})",
            path.display()
        )
    })?;
    parse(&text).map_err(|e| format!("{e} ({})", path.display()))
}

// -- reach --------------------------------------------------------------------

/// One provider's launchability facts after the precedence: config record >
/// map vendor > catalog npm rule.
struct ProviderFacts {
    protocol: Option<String>,
    base_url: Option<String>,
    key_env: Option<String>,
    key_file: Option<String>,
    has_record: bool,
    builtin: bool,
}

/// The URL's host part, lowercase, for the catalog host match.
fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let auth = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    auth.to_lowercase()
}

/// The compiled-in reach map.
pub(crate) const REACH_TOML: &str = include_str!("model_reach.toml");

/// The parsed reach map: per-harness launchability facts and the vendor
/// presets no catalog states.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModelReach {
    pub harness: HashMap<String, HarnessReach>,
    pub vendor: HashMap<String, VendorReach>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HarnessReach {
    /// The routed protocols this harness's spawn door can carry.
    pub routes: Vec<String>,
    /// The catalog id whose rows are this harness's own models.
    pub native_catalog: Option<String>,
    /// True when the harness owns its model list outright (opencode).
    pub own_list: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VendorReach {
    pub catalog: Option<String>,
    pub builtin: bool,
    /// The anthropic-protocol endpoint facts (base_url, key_env, key_file).
    pub anthropic: Option<VendorEndpoint>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VendorEndpoint {
    pub base_url: String,
    pub key_env: String,
    pub key_file: Option<String>,
}

/// Parse the reach map. An unparseable map is a compile-time bug surfaced at
/// startup, not a runtime path: parse() panics.
pub(crate) fn parse_reach(toml_text: &str) -> ModelReach {
    let parsed: toml::Value =
        toml::from_str(toml_text).expect("model_reach.toml must parse as TOML");
    let harness = parsed
        .get("harness")
        .and_then(|h| h.as_table())
        .map(|t| {
            t.iter()
                .map(|(name, caps)| {
                    (
                        name.to_string(),
                        HarnessReach {
                            routes: caps
                                .get("routes")
                                .and_then(|v| v.as_array())
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str().map(str::to_string))
                                        .collect()
                                })
                                .unwrap_or_default(),
                            native_catalog: caps
                                .get("native_catalog")
                                .and_then(|v| v.as_str())
                                .map(str::to_string),
                            own_list: caps
                                .get("own_list")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let vendor = parsed
        .get("vendor")
        .and_then(|h| h.as_table())
        .map(|t| {
            t.iter()
                .map(|(name, caps)| {
                    (
                        name.to_string(),
                        VendorReach {
                            catalog: caps
                                .get("catalog")
                                .and_then(|v| v.as_str())
                                .map(str::to_string),
                            builtin: caps
                                .get("builtin")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false),
                            anthropic: caps.get("anthropic").and_then(|v| v.as_table()).map(|ep| {
                                VendorEndpoint {
                                    base_url: ep
                                        .get("base_url")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                    key_env: ep
                                        .get("key_env")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                    key_file: ep
                                        .get("key_file")
                                        .and_then(|v| v.as_str())
                                        .map(str::to_string),
                                }
                            }),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    ModelReach { harness, vendor }
}

/// Facts for `provider`: a config record wins over a map vendor, which wins
/// over the catalog npm rule. Returns the facts and the linked catalog
/// provider (the model-name source).
fn facts_for<'a>(
    provider: &str,
    reach: &'a ModelReach,
    cfg: &'a serde_json::Value,
    catalog: Option<&'a Catalog>,
) -> Option<(ProviderFacts, Option<&'a CatalogProvider>)> {
    let record = cfg
        .get("providers")
        .and_then(|v| v.get(provider))
        .filter(|v| v.is_object());
    let vendor = reach.vendor.get(provider);
    let mut linked: Option<&CatalogProvider> = None;
    if let Some(v) = vendor {
        if let Some(c) = &v.catalog {
            linked = catalog.and_then(|c0| c0.get(c));
        }
        if linked.is_none() {
            linked = catalog.and_then(|c0| c0.get(provider));
        }
    }
    if linked.is_none() {
        if let Some(record) = record {
            let base = record
                .get("base_url")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if !base.is_empty() {
                let host = host_of(base);
                if let Some(cat) = catalog {
                    // Sorted ids: the link is the first matching catalog id,
                    // never whichever row a HashMap iteration hits first.
                    let mut ids: Vec<&String> = cat.providers.keys().collect();
                    ids.sort();
                    for id in ids {
                        let cp = &cat.providers[id];
                        if cp.api.as_deref().map(host_of).as_deref() == Some(host.as_str()) {
                            linked = Some(cp);
                            break;
                        }
                    }
                }
            }
        }
    }
    let npm_protocol = |cp: &CatalogProvider| -> Option<String> {
        match cp.npm.as_deref() {
            Some("@ai-sdk/anthropic") => Some("anthropic".to_string()),
            Some("@ai-sdk/openai-compatible") | Some("@ai-sdk/openai") => {
                Some("openai".to_string())
            }
            _ => None,
        }
    };
    let rec = |k: &str| {
        record
            .and_then(|r| r.get(k))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let ven = |k: &str| {
        vendor
            .and_then(|v| v.anthropic.as_ref())
            .and_then(|ep| match k {
                "base_url" => Some(ep.base_url.clone()),
                "key_env" => Some(ep.key_env.clone()),
                "key_file" => ep.key_file.clone(),
                _ => None,
            })
    };
    let protocol = rec("protocol")
        .or_else(|| ven("base_url").map(|_| "anthropic".to_string()))
        .or_else(|| linked.and_then(npm_protocol));
    let base_url = rec("base_url").or_else(|| ven("base_url")).or_else(|| {
        linked
            .and_then(|cp| cp.api.clone())
            .map(|api| api.strip_suffix("/v1").unwrap_or(api.as_str()).to_string())
    });
    let key_env = rec("api_key_env")
        .or_else(|| ven("key_env"))
        .or_else(|| linked.and_then(|cp| cp.env.first().cloned()));
    let key_file = rec("api_key_file").or_else(|| ven("key_file"));
    if protocol.is_none() {
        return None;
    }
    Some((
        ProviderFacts {
            protocol,
            base_url,
            key_env,
            key_file,
            has_record: record.is_some(),
            builtin: vendor.map(|v| v.builtin).unwrap_or(false),
        },
        linked,
    ))
}

/// The `provider -> model ids` the config names: each provider record's
/// tier_models and haiku_model (both live under model_routing.providers.<id>),
/// plus the account records' routes for this harness.
fn config_named_models(
    cfg: &serde_json::Value,
    records: &HashMap<String, Vec<ModelChoice>>,
    harness: &str,
) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let mut push = |provider: &str, model: &str| {
        if provider.is_empty() || model.is_empty() {
            return;
        }
        let list = out.entry(provider.to_string()).or_default();
        if !list.iter().any(|m| m == model) {
            list.push(model.to_string());
        }
    };
    if let Some(providers) = cfg.get("providers").and_then(|v| v.as_object()) {
        for (id, record) in providers {
            if let Some(tm) = record.get("tier_models").and_then(|v| v.as_object()) {
                for (_, val) in tm {
                    let model = val.as_str().unwrap_or_default();
                    // A bare id names the record's own provider; a
                    // provider/model value attributes itself.
                    match model.split_once('/') {
                        Some((p, m)) => push(p, m),
                        None => push(id, model),
                    }
                }
            }
            if let Some(hm) = record.get("haiku_model").and_then(|v| v.as_str()) {
                match hm.split_once('/') {
                    Some((p, m)) => push(p, m),
                    None => push(id, hm),
                }
            }
        }
    }
    if let Some(choices) = records.get(harness) {
        for c in choices {
            if let Some(p) = &c.provider {
                push(p, &c.model);
            }
        }
    }
    out
}

/// Classify one provider's rows for one harness and render its ModelChoice
/// rows. `key_present` is a parameter so tests stub it; production passes
/// crate::provider_key::key_present.
fn rows_for_provider(
    harness: &str,
    provider: &str,
    reach: &ModelReach,
    cfg: &serde_json::Value,
    named: &HashMap<String, Vec<String>>,
    catalog: Option<&Catalog>,
    key_present: &dyn Fn(&str, Option<&str>) -> bool,
) -> Option<(Vec<ModelChoice>, Vec<ModelChoice>)> {
    let (facts, linked) = facts_for(provider, reach, cfg, catalog)?;
    let routes = reach.harness.get(harness)?;
    let native = routes.native_catalog.as_deref() == Some(provider);
    let protocol = facts.protocol.clone().unwrap_or_default();
    let key_env_s = facts.key_env.clone().unwrap_or_default();
    let key_file = facts.key_file.clone();
    let key_ok = native || key_present(&key_env_s, key_file.as_deref());
    let has_access = facts.has_record || facts.builtin || native;
    let unreachable = !routes.routes.iter().any(|r| r == &protocol);

    let mut model_ids: Vec<String> = linked.map(|cp| cp.model_ids()).unwrap_or_default();
    if let Some(ms) = named.get(provider) {
        for m in ms {
            if !model_ids.iter().any(|k| k == m) {
                model_ids.push(m.clone());
            }
        }
    }
    if model_ids.is_empty() {
        return None;
    }
    let state = if unreachable {
        let reason = if routes.routes.is_empty() {
            format!("fno cannot launch a routed codex model yet ({provider} serves {protocol})")
        } else {
            format!(
                "{} speaks {}; {provider} serves {protocol}",
                harness,
                routes.routes.join("/")
            )
        };
        ModelState::Unreachable { reason }
    } else if !has_access {
        let base = facts.base_url.clone().unwrap_or_default();
        let cmd = format!(
            "fno config set model_routing.providers.{provider}.protocol={protocol} model_routing.providers.{provider}.base_url={base} model_routing.providers.{provider}.api_key_env={key_env_s}"
        );
        let mut steps = vec![cmd];
        if !key_env_s.is_empty() && !key_present(&key_env_s, key_file.as_deref()) {
            steps.push(format!("export {key_env_s}=<your key>"));
        }
        ModelState::NoKey {
            key_env: key_env_s,
            steps,
        }
    } else if !key_ok {
        let mut steps = Vec::new();
        if key_env_s.is_empty() {
            // No env var name anywhere: the record itself is missing the
            // api_key_env field, so name that instead of a bare export.
            steps.push(format!(
                "fno config set model_routing.providers.{provider}.api_key_env=<ENV VAR>"
            ));
        } else {
            steps.push(format!("export {key_env_s}=<your key>"));
        }
        if let Some(kf) = &key_file {
            steps.push(format!("or put the key in the file {kf}"));
        }
        ModelState::NoKey {
            key_env: key_env_s,
            steps,
        }
    } else {
        ModelState::Ready
    };

    let rows: Vec<ModelChoice> = model_ids
        .into_iter()
        .map(|model| ModelChoice {
            name: model.clone(),
            model,
            route: String::new(),
            provider: Some(provider.to_string()),
            state: state.clone(),
            key_env: facts.key_env.clone().filter(|k| !k.is_empty()),
            key_file: facts.key_file.clone(),
        })
        .collect();
    let ready = rows
        .iter()
        .filter(|r| matches!(r.state, ModelState::Ready))
        .cloned()
        .collect();
    let more = rows
        .iter()
        .filter(|r| !matches!(r.state, ModelState::Ready))
        .cloned()
        .collect();
    Some((ready, more))
}

/// The reach walk: (ready, more) rows for one harness. `model_routing` is
/// the parsed `fno config get model_routing -J` value, `records` the parsed
/// account records by harness. An own_list harness (opencode) owns its ready
/// list outright; catalog rows missing from it are NoKey naming the
/// catalog's first key env.
pub(crate) fn reach_rows(
    harness: &str,
    reach: &ModelReach,
    model_routing: &serde_json::Value,
    records: &HashMap<String, Vec<ModelChoice>>,
    catalog: Option<&Catalog>,
    key_present: &dyn Fn(&str, Option<&str>) -> bool,
) -> (Vec<ModelChoice>, Vec<ModelChoice>) {
    let cfg = model_routing
        .get("value")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let Some(routes) = reach.harness.get(harness) else {
        return (Vec::new(), Vec::new());
    };
    let mut ready: Vec<ModelChoice> = Vec::new();
    let mut more: Vec<ModelChoice> = Vec::new();
    if routes.own_list {
        let own: Vec<String> = records
            .get(harness)
            .map(|cs| cs.iter().map(|c| c.model.clone()).collect())
            .unwrap_or_default();
        if let Some(cat) = catalog {
            let mut ids: Vec<&String> = cat.providers.keys().collect();
            ids.sort();
            for id in ids {
                let cp = cat.get(id).expect("key from iter");
                let (Some(_api), Some(_npm)) = (&cp.api, &cp.npm) else {
                    continue;
                };
                for model in cp.model_ids() {
                    let composite = format!("{id}/{model}");
                    if own.iter().any(|o| *o == model || *o == composite) {
                        continue;
                    }
                    more.push(ModelChoice {
                        name: composite,
                        model,
                        route: String::new(),
                        provider: Some(id.clone()),
                        state: ModelState::NoKey {
                            key_env: cp.env.first().cloned().unwrap_or_default(),
                            steps: vec![],
                        },
                        key_env: cp.env.first().cloned(),
                        key_file: None,
                    });
                }
            }
        }
        return (ready, more);
    }

    let named = config_named_models(&cfg, records, harness);
    let mut universe: Vec<String> = Vec::new();
    {
        let mut add = |p: String| {
            if !p.is_empty() && !universe.iter().any(|k| k == &p) {
                universe.push(p);
            }
        };
        if let Some(providers) = cfg.get("providers").and_then(|v| v.as_object()) {
            for k in providers.keys() {
                add(k.clone());
            }
        }
        for k in reach.vendor.keys() {
            add(k.clone());
        }
        for k in named.keys() {
            add(k.clone());
        }
        if let Some(roles) = cfg.get("roles").and_then(|v| v.as_object()) {
            for (_, v) in roles {
                if let Some(p) = v.as_str() {
                    add(p.to_string());
                }
            }
        }
        if let Some(cat) = catalog {
            for (id, cp) in &cat.providers {
                if cp.api.is_some() && cp.npm.is_some() {
                    add(id.clone());
                }
            }
        }
    }
    universe.sort();
    for p in &universe {
        if let Some((r, m)) =
            rows_for_provider(harness, p, reach, &cfg, &named, catalog, key_present)
        {
            ready.extend(r);
            more.extend(m);
        }
    }
    (ready, more)
}

/// The vendor endpoint's base_url for one protocol, read from the
/// compiled-in map.
pub(crate) fn vendor_base_url(vendor: &str, protocol: &str) -> Option<String> {
    if protocol != "anthropic" {
        return None;
    }
    let reach = parse_reach(REACH_TOML);
    reach
        .vendor
        .get(vendor)?
        .anthropic
        .as_ref()
        .map(|ep| ep.base_url.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hermetic state root: FNO_STATE_DIR is one of the vars the cache
    /// reads, so tests pin it and hold an env lock.
    fn state_fixture() -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = state_env_lock();
        let dir = std::env::temp_dir().join(format!(
            "fno-model-catalog-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("cache")).unwrap();
        std::env::set_var("FNO_STATE_DIR", &dir);
        (dir, guard)
    }

    const FIXTURE: &str = r#"{
        "deepseek": {
            "name": "DeepSeek",
            "env": ["DEEPSEEK_API_KEY"],
            "api": "https://api.deepseek.com",
            "npm": "@ai-sdk/anthropic",
            "models": {
                "deepseek-chat": {"id": "deepseek-chat", "name": "DeepSeek V3"},
                "deepseek-reasoner": {"id": "deepseek-reasoner", "name": "DeepSeek R1"}
            },
            "unknown_future_field": true
        },
        "zai": {
            "name": "Z.ai",
            "env": ["ZHIPU_API_KEY"],
            "api": "https://api.z.ai",
            "npm": "@ai-sdk/anthropic",
            "models": {
                "glm-5.3-flash": {"id": "glm-5.3-flash", "name": "GLM Flash"}
            }
        }
    }"#;

    #[test]
    fn parse_reads_the_typed_subset_and_ignores_unknown_fields() {
        let catalog = parse(FIXTURE).unwrap();
        let deepseek = catalog.get("deepseek").unwrap();
        assert_eq!(deepseek.name, "DeepSeek");
        assert_eq!(deepseek.env, vec!["DEEPSEEK_API_KEY"]);
        assert_eq!(deepseek.api.as_deref(), Some("https://api.deepseek.com"));
        assert_eq!(deepseek.npm.as_deref(), Some("@ai-sdk/anthropic"));
        assert_eq!(
            deepseek.model_ids(),
            vec!["deepseek-chat".to_string(), "deepseek-reasoner".to_string()]
        );
        // The model's own id field wins when it disagrees with the key.
        let zai = catalog.get("zai").unwrap();
        assert_eq!(zai.model_ids(), vec!["glm-5.3-flash".to_string()]);
        assert_eq!(zai.name, "Z.ai");
    }

    #[test]
    fn needs_refresh_true_when_missing_or_older_than_24h() {
        let now = SystemTime::now();
        assert!(needs_refresh(None, now));
        let old = now - Duration::from_secs(25 * 60 * 60);
        assert!(needs_refresh(Some(old), now));
        let fresh = now - Duration::from_secs(3600);
        assert!(!needs_refresh(Some(fresh), now));
    }

    #[test]
    fn refresh_commits_a_good_fetch_and_leaves_no_tmp() {
        let (dir, _guard) = state_fixture();
        let tmp = dir.join("cache").join("models-dev.json.tmp.999");
        std::fs::write(&tmp, FIXTURE).unwrap();
        let committed = install_from(&tmp, &cache_path(&dir));
        assert!(committed.is_ok(), "{committed:?}");
        assert!(cache_path(&dir).is_file(), "cache committed");
        assert!(!tmp.exists(), "tmp renamed away");
        let catalog = load(&dir).unwrap();
        assert!(catalog.get("deepseek").is_some());
    }

    #[test]
    fn failed_parse_keeps_old_cache_and_deletes_tmp() {
        let (dir, _guard) = state_fixture();
        let cache = cache_path(&dir);
        std::fs::write(&cache, FIXTURE).unwrap();
        let tmp = dir.join("cache").join("models-dev.json.tmp.999");
        std::fs::write(&tmp, "not json at all {").unwrap();
        let committed = install_from(&tmp, &cache);
        assert!(committed.is_err());
        assert_eq!(
            std::fs::read_to_string(&cache).unwrap(),
            FIXTURE,
            "old cache kept"
        );
        assert!(!tmp.exists(), "tmp deleted");
    }

    #[test]
    fn load_reason_names_the_cache_path_when_missing_or_unparseable() {
        let (dir, _guard) = state_fixture();
        let reason = load(&dir).unwrap_err();
        assert!(reason.contains("models-dev.json"), "{reason}");
        std::fs::write(cache_path(&dir), "{not json").unwrap();
        let reason = load(&dir).unwrap_err();
        assert!(reason.contains("models-dev.json"), "{reason}");
        let _ = std::fs::remove_dir_all(&dir);
    }
    fn reach_fixture() -> (
        ModelReach,
        serde_json::Value,
        std::collections::HashMap<String, Vec<ModelChoice>>,
    ) {
        let reach = parse_reach(REACH_TOML);
        // The real `fno config get model_routing -J` wraps the table in "value".
        let cfg: serde_json::Value = serde_json::json!({
            "value": {
                "providers": {
                "deepseek": {
                    "protocol": "anthropic",
                    "base_url": "https://api.deepseek.com/anthropic",
                    "api_key_env": "FNO_TEST_DS_KEY"
                },
                    "zai-openai": {
                        "protocol": "openai",
                        "base_url": "https://api.z.ai/api/coding/paas/v4",
                        "api_key_env": "FNO_TEST_ZAI_OPENAI_KEY"
                    }
                }
            }
        });
        (reach, cfg, std::collections::HashMap::new())
    }

    #[test]
    fn parse_reach_reads_the_compiled_map() {
        let reach = parse_reach(REACH_TOML);
        let claude = reach.harness.get("claude").unwrap();
        assert_eq!(claude.routes, vec!["anthropic".to_string()]);
        assert_eq!(claude.native_catalog.as_deref(), Some("anthropic"));
        assert!(!claude.own_list);
        assert!(reach.harness.get("codex").unwrap().routes.is_empty());
        assert!(reach.harness.get("opencode").unwrap().own_list);
        let zai = reach.vendor.get("zai").unwrap();
        assert!(zai.builtin);
        assert_eq!(
            zai.anthropic.as_ref().unwrap().base_url,
            "https://api.z.ai/api/anthropic"
        );
        assert_eq!(zai.anthropic.as_ref().unwrap().key_env, "ZAI_API_KEY");
    }

    #[test]
    fn keyed_anthropic_record_rows_are_ready() {
        let (reach, cfg, records) = reach_fixture();
        let catalog = parse(FIXTURE).unwrap();
        let key_present = |env: &str, _file: Option<&str>| env == "FNO_TEST_DS_KEY";
        let (ready, more) = reach_rows(
            "claude",
            &reach,
            &cfg,
            &records,
            Some(&catalog),
            &key_present,
        );
        assert!(
            more.iter()
                .all(|r| r.provider.as_deref() != Some("deepseek")),
            "keyed deepseek rows belong in ready, not more: {more:?}"
        );
        let ds: Vec<&ModelChoice> = ready
            .iter()
            .filter(|r| r.provider.as_deref() == Some("deepseek"))
            .collect();
        assert!(!ds.is_empty());
        for row in ds {
            assert!(matches!(row.state, ModelState::Ready), "{row:?}");
        }
    }

    #[test]
    fn unrecorded_catalog_provider_is_nokey_with_config_set_step() {
        let (reach, mut cfg, records) = reach_fixture();
        // Drop the deepseek record: the catalog npm rule alone must place it.
        cfg["value"]["providers"]
            .as_object_mut()
            .unwrap()
            .remove("deepseek");
        let catalog = parse(FIXTURE).unwrap();
        // DEEPSEEK_API_KEY is set but no record exists: the door refuses an
        // unknown provider, so the row stays NoKey with the one config step.
        let key_present = |env: &str, _file: Option<&str>| env == "DEEPSEEK_API_KEY";
        let (ready, more) = reach_rows(
            "claude",
            &reach,
            &cfg,
            &records,
            Some(&catalog),
            &key_present,
        );
        assert!(ready
            .iter()
            .all(|r| r.provider.as_deref() != Some("deepseek")));
        let ds: Vec<&ModelChoice> = more
            .iter()
            .filter(|r| r.provider.as_deref() == Some("deepseek"))
            .collect();
        assert!(!ds.is_empty());
        for row in ds {
            let ModelState::NoKey { key_env, steps } = &row.state else {
                panic!("expected NoKey, got {:?}", row.state);
            };
            assert_eq!(key_env, "DEEPSEEK_API_KEY");
            assert!(
                steps[0].starts_with("fno config set model_routing.providers.deepseek"),
                "{steps:?}"
            );
            assert!(steps[0].contains("DEEPSEEK_API_KEY"), "{steps:?}");
            assert_eq!(steps.len(), 1, "key is set; only the config step shows");
        }
    }

    #[test]
    fn protocol_gaps_are_unreachable_with_the_reasons() {
        let (reach, cfg, records) = reach_fixture();
        let catalog = parse(FIXTURE).unwrap();
        let key_present = |env: &str, _file: Option<&str>| {
            env == "FNO_TEST_DS_KEY" || env == "FNO_TEST_ZAI_OPENAI_KEY"
        };
        // claude + an openai-protocol record.
        let (ready, more) = reach_rows(
            "claude",
            &reach,
            &cfg,
            &records,
            Some(&catalog),
            &key_present,
        );
        assert!(ready
            .iter()
            .all(|r| r.provider.as_deref() != Some("zai-openai")));
        let row = more
            .iter()
            .find(|r| r.provider.as_deref() == Some("zai-openai"))
            .expect("zai-openai rows land under more");
        let ModelState::Unreachable { reason } = &row.state else {
            panic!("expected Unreachable, got {:?}", row.state);
        };
        assert_eq!(reason, "claude speaks anthropic; zai-openai serves openai");
        // codex + a routed provider: the door refuses a routed codex pick.
        let (ready, more) = reach_rows(
            "codex",
            &reach,
            &cfg,
            &records,
            Some(&catalog),
            &key_present,
        );
        assert!(ready.is_empty(), "codex launches nothing routed: {ready:?}");
        let row = more
            .iter()
            .find(|r| r.provider.as_deref() == Some("deepseek"))
            .expect("deepseek rows land under more for codex");
        let ModelState::Unreachable { reason } = &row.state else {
            panic!("expected Unreachable, got {:?}", row.state);
        };
        assert!(
            reason.starts_with("fno cannot launch a routed codex model yet"),
            "{reason}"
        );
    }

    #[test]
    fn builtin_vendor_key_from_env_file_is_ready_on_claude() {
        let (reach, mut cfg, records) = reach_fixture();
        // Remove the test deepseek record; zai is builtin via the map.
        cfg["value"]["providers"]
            .as_object_mut()
            .unwrap()
            .remove("deepseek");
        cfg["value"]["providers"]
            .as_object_mut()
            .unwrap()
            .remove("zai-openai");
        let catalog = parse(FIXTURE).unwrap();
        let key_present = |env: &str, file: Option<&str>| {
            // The user's shape: ZAI_API_KEY only in the dotenv file.
            env == "ZAI_API_KEY" && file == Some("~/.fno/.env")
        };
        let (ready, more) = reach_rows(
            "claude",
            &reach,
            &cfg,
            &records,
            Some(&catalog),
            &key_present,
        );
        let zai: Vec<&ModelChoice> = ready
            .iter()
            .filter(|r| r.provider.as_deref() == Some("zai"))
            .collect();
        assert!(
            !zai.is_empty(),
            "builtin zai with a file key is Ready; more was {more:?}"
        );
        for row in zai {
            assert!(matches!(row.state, ModelState::Ready), "{row:?}");
        }
    }
    #[test]
    fn provider_declared_tier_and_haiku_models_become_rows() {
        // tier_models and haiku_model live under model_routing.providers.<id>,
        // not at the root: a custom provider with no catalog link and no
        // account record still lists the models it declares.
        let (reach, _cfg, _records) = reach_fixture();
        let cfg: serde_json::Value = serde_json::json!({
            "value": {
                "providers": {
                    "myproxy": {
                        "protocol": "anthropic",
                        "base_url": "https://myproxy.local/anthropic",
                        "api_key_env": "FNO_TEST_PROXY_KEY",
                        "tier_models": {"opus": "proxy-big", "sonnet": "proxy-mid"},
                        "haiku_model": "proxy-small"
                    }
                }
            }
        });
        let key_present = |env: &str, _file: Option<&str>| env == "FNO_TEST_PROXY_KEY";
        let (ready, more) = reach_rows(
            "claude",
            &reach,
            &cfg,
            &std::collections::HashMap::new(),
            None,
            &key_present,
        );
        let mut names: Vec<&str> = ready
            .iter()
            .filter(|r| r.provider.as_deref() == Some("myproxy"))
            .map(|r| r.model.as_str())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["proxy-big", "proxy-mid", "proxy-small"],
            "declared models listed; more was {more:?}"
        );
    }
}
