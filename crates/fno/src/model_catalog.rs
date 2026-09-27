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

/// One refresh: fetch to `<dir>/models-dev.json.tmp.<pid>`, parse the tmp
/// file, and rename it over the cache only when the parse succeeds. A failed
/// fetch or parse deletes the tmp file and keeps the old cache. The composer
/// spawns this in the background; it never blocks the picker.
pub async fn refresh(state: &Path) -> Result<(), String> {
    let dir = state.join("cache");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A hermetic state root: FNO_STATE_DIR is one of the vars the cache
    /// reads, so tests pin it and hold an env lock.
    fn state_fixture() -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
}
