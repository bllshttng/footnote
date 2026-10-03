//! The one price leg: models.dev rates, the token-to-dollar math, and the
//! running session-cost fold the daemon serves. `model_catalog.rs` (crate
//! `fno`) fetches and caches `https://models.dev/api.json`; this module only
//! reads that cache file, so fno-agents never fetches and the card and the
//! ledger price through the same table. It replaces the hand-kept Python
//! `pricing.yaml`, whose stale-table incidents (two 3x cost inflations when
//! a new Opus shipped before the hand table was updated) are recorded in
//! `cost_tracker.py`'s header.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use serde::Deserialize;

use crate::session_activity::Tokens;
use crate::state::RegistryEntry;

/// Dollars per million tokens, per kind. A missing rate is `None`: pricing a
/// nonzero count against it yields no cost, never a guess.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rates {
    pub(crate) input: Option<f64>,
    pub(crate) output: Option<f64>,
    pub(crate) cache_read: Option<f64>,
    pub(crate) cache_write: Option<f64>,
}

/// The parsed `cost` subset of one models.dev model row.
#[derive(Debug, Default, Clone, Deserialize)]
struct RawCost {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct CatalogModelCosts {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    cost: Option<RawCost>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct CatalogProviderCosts {
    #[serde(default)]
    models: HashMap<String, CatalogModelCosts>,
}

/// The price book: every models.dev provider's models' costs.
#[derive(Debug, Default, Clone)]
pub(crate) struct PriceBook {
    providers: HashMap<String, CatalogProviderCosts>,
}

/// Provider ids probed in order when no route provider matched: the
/// first-party set whose ids the fno fleet actually spawns under.
const FIRST_PARTY: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "zai",
    "deepseek",
    "moonshotai",
    "xai",
    "mistral",
];

/// The catalog id a spawn model string keys on: cut the `[1m]`-style
/// context suffix and any route prefix (`zai/glm-5.3-flash` keys the model
/// after the slash), trim, lowercase. Both sides of every match normalize
/// through this, so the comparison stays exact: no fuzzy match, no family
/// fallback.
fn normalize(model: &str) -> String {
    let bare = model.split('[').next().unwrap_or(model).trim();
    let bare = bare.rsplit('/').next().unwrap_or(bare);
    bare.to_ascii_lowercase()
}

impl PriceBook {
    /// The rates for `model`: the route provider's catalog first, then the
    /// first-party order. Match the model key or its `id` field. A miss is
    /// `None`; so is a matched model with no `cost` object.
    pub(crate) fn rates(&self, model: &str, route_provider: Option<&str>) -> Option<Rates> {
        let key = normalize(model);
        let mut order: Vec<&str> = route_provider.into_iter().collect();
        order.extend(FIRST_PARTY.iter().copied());
        for provider in order {
            let Some(catalog) = self.providers.get(provider) else {
                continue;
            };
            for (name, model_costs) in &catalog.models {
                let id_matches = model_costs
                    .id
                    .as_deref()
                    .is_some_and(|id| normalize(id) == key);
                if normalize(name) == key || id_matches {
                    return model_costs.cost.as_ref().map(|c| Rates {
                        input: c.input,
                        output: c.output,
                        cache_read: c.cache_read,
                        cache_write: c.cache_write,
                    });
                }
            }
        }
        None
    }
}

/// The dollar cost of one model's token usage at one rate row. A kind with a
/// nonzero count and no rate is `None`: the session is unpriced, never
/// guessed at a default tier.
pub(crate) fn cost(t: Tokens, r: &Rates) -> Option<f64> {
    let part = |count: u64, rate: Option<f64>| -> Option<f64> {
        if count == 0 {
            Some(0.0)
        } else {
            rate.map(|rate| count as f64 * rate)
        }
    };
    let dollars = part(t.input, r.input)?
        + part(t.output, r.output)?
        + part(t.cache_read, r.cache_read)?
        + part(t.cache_write, r.cache_write)?;
    Some(dollars / 1_000_000.0)
}

/// The state root: `FNO_STATE_DIR` when set, else `$HOME/.fno` -- the same
/// resolution `model_catalog::state_dir` (crate `fno`) applies.
pub(crate) fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FNO_STATE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".fno"))
        .unwrap_or_else(|| PathBuf::from(".fno"))
}

fn cache_path(state: &Path) -> PathBuf {
    state.join("cache").join("models-dev.json")
}

/// The parsed book for `state`, re-read only when the cache file moves.
/// ponytail: process-global keyed by (path, mtime); a daemon restart re-reads
/// the catalog once.
pub(crate) fn price_book(state: &Path) -> Option<Arc<PriceBook>> {
    static BOOK: OnceLock<Mutex<Option<(PathBuf, SystemTime, Arc<PriceBook>)>>> = OnceLock::new();
    let path = cache_path(state);
    let mtime = std::fs::metadata(&path).ok()?.modified().ok()?;
    let cell = BOOK.get_or_init(|| Mutex::new(None));
    let mut seen = cell.lock().ok()?;
    if let Some((seen_path, seen_at, book)) = seen.as_ref() {
        if *seen_path == path && *seen_at == mtime {
            return Some(book.clone());
        }
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let providers: HashMap<String, CatalogProviderCosts> = serde_json::from_str(&text).ok()?;
    let book = Arc::new(PriceBook { providers });
    *seen = Some((path, mtime, book.clone()));
    Some(book)
}

/// The running cost reading the daemon serves: cents when every token kind
/// found a rate, plus the raw token count so an unpriced model still shows
/// its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionCost {
    pub(crate) cents: Option<u64>,
    pub(crate) tokens: u64,
    pub(crate) measured_at: String,
}

/// The incremental fold for one transcript: a byte offset, compaction
/// boundaries, seen Claude message ids, per-model token sums, and the codex
/// cumulative total. The served count is absent until a read succeeds.
#[derive(Default)]
pub(crate) struct RunningCost {
    offset: u64,
    seen_ids: HashSet<String>,
    per_model: HashMap<String, Tokens>,
    codex_total: Option<Tokens>,
    compaction_count: u64,
    compaction_readable: bool,
    compaction_parse_error: bool,
}

impl RunningCost {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Absorb the bytes appended to `path` since the last call, one line at
    /// a time, so memory stays bounded by the longest line rather than the
    /// appended region (a daemon restart re-reads the whole transcript). A
    /// file shorter than the remembered offset (rotated, replaced) resets
    /// the fold. Only whole newline-terminated lines are consumed; a partial
    /// tail stays for the next call.
    pub(crate) fn absorb(&mut self, path: &Path) {
        let Ok(len) = std::fs::metadata(path).map(|m| m.len()) else {
            self.compaction_readable = false;
            return;
        };
        if len < self.offset {
            *self = Self::new();
        }
        let Ok(file) = std::fs::File::open(path) else {
            self.compaction_readable = false;
            return;
        };
        let mut reader = std::io::BufReader::new(file);
        if reader.seek(SeekFrom::Start(self.offset)).is_err() {
            self.compaction_readable = false;
            return;
        }
        let mut line = Vec::new();
        let mut consumed = 0u64;
        let mut read_ok = true;
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(n) => {
                    if line.last() != Some(&b'\n') {
                        break; // partial tail stays for the next sweep
                    }
                    consumed += n as u64;
                    if let Ok(text) = std::str::from_utf8(&line) {
                        self.row_line(text.trim_end_matches('\n'));
                    } else {
                        self.compaction_parse_error = true;
                    }
                }
                Err(_) => {
                    read_ok = false;
                    break;
                }
            }
        }
        self.offset += consumed;
        self.compaction_readable = read_ok && !self.compaction_parse_error;
    }

    fn row_line(&mut self, line: &str) {
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        if crate::compaction::boundary_ts(&row).is_some() {
            self.compaction_count += 1;
        }
        if let Some(total) = crate::session_activity::codex_token_total(&row) {
            self.codex_total = Some(total);
            return;
        }
        let is_assistant = row.get("type").and_then(|v| v.as_str()) == Some("assistant")
            || row
                .get("message")
                .and_then(|m| m.get("role"))
                .and_then(|v| v.as_str())
                == Some("assistant");
        if !is_assistant {
            return;
        }
        let message = row.get("message");
        // The dedup is `ActivityFold::row`'s own rule (session_activity.rs):
        // a row with no id counts once by itself.
        let id = message.and_then(|m| m.get("id")).and_then(|v| v.as_str());
        let fresh = match id {
            Some(id) => self.seen_ids.insert(id.to_string()),
            None => true,
        };
        if !fresh {
            return;
        }
        let Some(usage) = message
            .and_then(|m| m.get("usage"))
            .filter(|u| u.is_object())
        else {
            return;
        };
        let f = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        let model = message
            .and_then(|m| m.get("model"))
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let tokens = self.per_model.entry(model).or_default();
        tokens.input += f("input_tokens");
        tokens.output += f("output_tokens");
        tokens.cache_read += f("cache_read_input_tokens");
        tokens.cache_write += f("cache_creation_input_tokens");
    }

    /// Price everything seen so far. Codex totals count cached tokens inside
    /// `input_tokens`, so the codex path prices `input - cache_read` at the
    /// input rate. Cents are `None` when any model holding tokens is
    /// unpriced; `tokens` is always the raw sum.
    pub(crate) fn session(
        &self,
        book: &PriceBook,
        codex_model: Option<&str>,
        route: Option<&str>,
    ) -> SessionCost {
        let mut dollars = 0.0;
        let mut priced = true;
        let mut tokens = 0u64;
        for (model, t) in &self.per_model {
            tokens += t.input + t.output + t.cache_read + t.cache_write;
            match book.rates(model, route).and_then(|r| cost(*t, &r)) {
                Some(c) => dollars += c,
                None => priced = false,
            }
        }
        if let Some(total) = self.codex_total {
            tokens += total.input + total.output + total.cache_read + total.cache_write;
            let metered = Tokens {
                input: total.input.saturating_sub(total.cache_read),
                ..total
            };
            match codex_model
                .and_then(|m| book.rates(m, route))
                .and_then(|r| cost(metered, &r))
            {
                Some(c) => dollars += c,
                None => priced = false,
            }
        }
        let cents = if priced {
            Some((dollars * 100.0).round() as u64)
        } else {
            None
        };
        SessionCost {
            cents,
            tokens,
            measured_at: String::new(),
        }
    }
}

/// The daemon-side store: fold state per harness session plus the last
/// running-cost reading. Keyed by `harness_session_id` (law d-e952ed19):
/// session facts are served beside the row, never written to the registry.
static FOLDS: OnceLock<Mutex<HashMap<String, (RunningCost, Option<SessionCost>)>>> =
    OnceLock::new();

/// Sweep-time measurement: absorb new transcript bytes, update the boundary
/// count, reprice when a catalog exists, and remember both under the session id.
/// `now` stamps only the served cost reading.
/// The fold is taken OUT of the map while it absorbs: the read and the price
/// run lock-free, because the agents-list path serves from the same mutex
/// and must not stall behind a large transcript read. Entries the sweeps
/// stopped measuring (exited, reaped) prune after a day.
pub(crate) fn measure_session_cost(
    state: &Path,
    sid: &str,
    transcript: &Path,
    codex_model: Option<&str>,
    route: Option<&str>,
    now: &str,
) {
    let cell = FOLDS.get_or_init(|| Mutex::new(HashMap::new()));
    let (mut fold, last_cost) = {
        let Ok(mut folds) = cell.lock() else {
            return;
        };
        folds
            .remove(sid)
            .unwrap_or_else(|| (RunningCost::new(), None))
    };
    fold.absorb(transcript);
    let Some(book) = price_book(state) else {
        if let Ok(mut folds) = cell.lock() {
            folds.insert(sid.to_string(), (fold, last_cost));
        }
        return;
    };
    let mut cost = fold.session(&book, codex_model, route);
    cost.measured_at = now.to_string();
    if let Ok(mut folds) = cell.lock() {
        if let Ok(stamp) = chrono::DateTime::parse_from_rfc3339(now) {
            let cutoff = stamp - chrono::Duration::hours(24);
            folds.retain(|_, (_, last)| {
                last.as_ref()
                    .and_then(|c| chrono::DateTime::parse_from_rfc3339(&c.measured_at).ok())
                    .is_some_and(|m| m > cutoff)
            });
        }
        folds.insert(sid.to_string(), (fold, Some(cost)));
    }
}

/// Keep the compaction reading unknown when this sweep cannot resolve its
/// transcript, without discarding the independently measured running cost.
pub(crate) fn mark_session_transcript_unavailable(sid: &str) {
    let cell = FOLDS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut folds) = cell.lock() {
        let (mut fold, cost) = folds
            .remove(sid)
            .unwrap_or_else(|| (RunningCost::new(), None));
        fold.compaction_readable = false;
        folds.insert(sid.to_string(), (fold, cost));
    }
}

/// The served triple for one row: cents, tokens, measurement time. All
/// `None` when the sweep never measured the session.
pub(crate) fn served_session_cost(sid: Option<&str>) -> (Option<u64>, Option<u64>, Option<String>) {
    let Some(sid) = sid else {
        return (None, None, None);
    };
    let cell = FOLDS.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(folds) = cell.lock() else {
        return (None, None, None);
    };
    match folds.get(sid).and_then(|(_, last)| last.clone()) {
        Some(cost) => (cost.cents, Some(cost.tokens), Some(cost.measured_at)),
        None => (None, None, None),
    }
}

pub(crate) fn served_compaction_count(sid: Option<&str>) -> Option<u64> {
    let sid = sid.filter(|sid| !sid.is_empty())?;
    let cell = FOLDS.get_or_init(|| Mutex::new(HashMap::new()));
    let folds = cell.lock().ok()?;
    let (fold, _) = folds.get(sid)?;
    fold.compaction_readable.then_some(fold.compaction_count)
}

/// Session metrics served beside the four stored context fields and running
/// cost. One helper keeps daemon.rs (shrink-only) net zero.
pub(crate) fn served_session_metrics_keys(
    e: &RegistryEntry,
) -> serde_json::Map<String, serde_json::Value> {
    let mut keys = serde_json::Map::new();
    keys.insert(
        "context_used_pct".into(),
        serde_json::json!(e.context_used_pct),
    );
    keys.insert(
        "context_used_tokens".into(),
        serde_json::json!(e.context_used_tokens),
    );
    keys.insert(
        "context_window_tokens".into(),
        serde_json::json!(e.context_window_tokens),
    );
    keys.insert(
        "context_measured_at".into(),
        serde_json::json!(e.context_measured_at),
    );
    let (cents, tokens, measured_at) = served_session_cost(e.harness_session_id.as_deref());
    keys.insert("session_cost_cents".into(), serde_json::json!(cents));
    keys.insert("session_tokens".into(), serde_json::json!(tokens));
    keys.insert(
        "session_cost_measured_at".into(),
        serde_json::json!(measured_at),
    );
    let compaction_count = served_compaction_count(e.harness_session_id.as_deref());
    keys.insert(
        "compaction_count".into(),
        serde_json::json!(compaction_count),
    );
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

    /// A two-provider fixture: opus with all four rates, glm with input and
    /// output only.
    fn fixture_book(dir: &Path) -> PathBuf {
        let cache = dir.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let text = r#"{
            "anthropic": {"models": {"claude-opus-5-5": {"cost": {
                "input": 4.0, "output": 20.0, "cache_read": 0.2, "cache_write": 5.0}}}},
            "zai": {"models": {"glm-5.3-flash": {"id": "glm-5.3-flash", "cost": {
                "input": 0.6, "output": 1.8}}}}
        }"#;
        let path = cache.join("models-dev.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
        path
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fno-model-price-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rates_match_normalized_ids_across_providers() {
        let dir = tmpdir("rates");
        let path = fixture_book(&dir);
        let text = std::fs::read_to_string(&path).unwrap();
        let providers: HashMap<String, CatalogProviderCosts> = serde_json::from_str(&text).unwrap();
        let book = PriceBook { providers };
        let rates = book.rates("claude-opus-5-5[1m]", None).unwrap();
        assert_eq!(rates.input, Some(4.0));
        assert_eq!(rates.cache_write, Some(5.0));
        // A route-prefixed id still keys the catalog model.
        let prefixed = book.rates("anthropic/claude-opus-5-5", None).unwrap();
        assert_eq!(prefixed.output, Some(20.0));
        // Route provider first, then the fixed first-party order.
        let glm = book
            .rates("GLM-5.3-FLASH ", Some("unlisted-provider"))
            .unwrap();
        assert_eq!(glm.input, Some(0.6));
        assert_eq!(glm.cache_read, None);
        assert!(book.rates("claude-opus-next", None).is_none());
    }

    #[test]
    fn cost_sums_all_four_kinds_per_million() {
        let rates = Rates {
            input: Some(4.0),
            output: Some(20.0),
            cache_read: Some(0.2),
            cache_write: Some(5.0),
        };
        let t = Tokens {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write: 1_000_000,
        };
        let dollars = cost(t, &rates).unwrap();
        assert!((dollars - 29.2).abs() < 1e-9);
    }

    #[test]
    fn cost_is_none_when_a_nonzero_kind_has_no_rate() {
        let rates = Rates {
            input: Some(0.6),
            output: Some(1.8),
            cache_read: None,
            cache_write: None,
        };
        let t = Tokens {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 0,
            cache_write: 0,
        };
        let dollars = cost(t, &rates).unwrap();
        assert!((dollars - 2.4).abs() < 1e-9);
        let t = Tokens {
            cache_read: 100,
            ..t
        };
        assert!(cost(t, &rates).is_none());
    }

    /// Three rows sharing one message id, then two more appended rows.
    fn claude_transcript() -> String {
        let row = |id: &str, model: &str, inp: u64, out: u64| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":{inp},"output_tokens":{out}}}}}}}"#
            )
        };
        let mut text = String::new();
        text.push_str(&row("msg_1", "claude-opus-5-5[1m]", 100_000, 10_000));
        text.push('\n');
        text.push_str(&row("msg_1", "claude-opus-5-5[1m]", 100_000, 10_000));
        text.push('\n');
        text.push_str(&row("msg_2", "claude-opus-5-5[1m]", 40_000, 4_000));
        text.push('\n');
        text
    }

    #[test]
    fn fold_dedups_and_reads_only_appended_bytes() {
        let dir = tmpdir("fold");
        let path = dir.join("transcript.jsonl");
        std::fs::write(&path, claude_transcript()).unwrap();
        let mut fold = RunningCost::new();
        fold.absorb(&path);
        let book_bookmark = fold.offset;
        let first = fold.session(&PriceBook::default(), None, None);
        assert_eq!(first.tokens, 154_000);

        let mut appended = claude_transcript();
        appended.push_str(&format!(
            r#"{{"type":"assistant","message":{{"id":"msg_3","model":"claude-opus-5-5[1m]","usage":{{"input_tokens":7000,"output_tokens":3000}}}}}}"#
        ));
        appended.push('\n');
        std::fs::write(&path, appended).unwrap();
        fold.absorb(&path);
        let second = fold.session(&PriceBook::default(), None, None);
        // The re-written prefix re-read from the new offset only saw msg_3;
        // msg_1 counted once, msg_2 once.
        assert_eq!(second.tokens, 164_000);
        assert!(fold.offset > book_bookmark);

        // Pricing through the fixture book: 147k input + 17k output tokens.
        let dir = tmpdir("fold-priced");
        fixture_book(&dir);
        let book = price_book(&dir).unwrap();
        let priced = fold.session(&book, None, None);
        let expected: f64 = (147_000.0 * 4.0 + 17_000.0 * 20.0) / 1_000_000.0 * 100.0;
        assert_eq!(priced.cents, Some(expected.round() as u64));
    }

    #[test]
    fn fold_reads_codex_cumulative_totals() {
        let dir = tmpdir("codex");
        let path = dir.join("rollout.jsonl");
        let row = |inp: u64, cached: u64, out: u64| {
            format!(
                r#"{{"type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{inp},"cached_input_tokens":{cached},"output_tokens":{out}}}}}}}}}"#
            )
        };
        let mut text = row(500, 300, 50);
        text.push('\n');
        text.push_str(&row(900, 700, 80));
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        let mut fold = RunningCost::new();
        fold.absorb(&path);
        // tokens is the RAW sum of the last cumulative total (900 + 700 + 80);
        // the cached-token subtraction only applies to the dollars.
        assert_eq!(fold.session(&PriceBook::default(), None, None).tokens, 1680);

        let dir = tmpdir("codex-priced");
        fixture_book(&dir);
        let book = price_book(&dir).unwrap();
        // Codex totals count cached tokens inside input: 200 metered input
        // at the input rate, 700 cached, 80 output -- against the opus row
        // for the rate shape only.
        let rates = book.rates("claude-opus-5-5", None).unwrap();
        let total = fold.codex_total.unwrap();
        let metered = Tokens {
            input: total.input - total.cache_read,
            ..total
        };
        let dollars = cost(metered, &rates).unwrap();
        assert!((dollars - (200.0 * 4.0 + 700.0 * 0.2 + 80.0 * 20.0) / 1_000_000.0).abs() < 1e-12);
    }

    #[test]
    fn session_is_unpriced_when_any_model_with_tokens_is_unpriced() {
        let dir = tmpdir("mixed");
        fixture_book(&dir);
        let book = price_book(&dir).unwrap();
        let path = dir.join("mixed.jsonl");
        let row = |id: &str, model: &str, inp: u64| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":{inp},"output_tokens":0}}}}}}"#
            )
        };
        let mut text = row("m1", "claude-opus-5-5", 1000);
        text.push('\n');
        text.push_str(&row("m2", "some-unknown-model", 2000));
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        let mut fold = RunningCost::new();
        fold.absorb(&path);
        let session = fold.session(&book, None, None);
        assert_eq!(session.cents, None);
        assert_eq!(session.tokens, 3000);
    }

    #[test]
    fn served_keys_carry_the_measured_cost() {
        let dir = tmpdir("served");
        fixture_book(&dir);
        let path = dir.join("t.jsonl");
        let mut transcript = claude_transcript();
        transcript.push_str(
            r#"{"type":"system","subtype":"compact_boundary","timestamp":"2026-10-02T00:00:00Z"}"#,
        );
        transcript.push('\n');
        std::fs::write(&path, &transcript).unwrap();
        measure_session_cost(
            &dir,
            "sess-cost-1",
            &path,
            None,
            None,
            "2026-10-02T00:00:00Z",
        );
        let mut e = RegistryEntry::default();
        e.harness_session_id = Some("sess-cost-1".into());
        e.context_used_pct = Some(48);
        let keys = served_session_metrics_keys(&e);
        assert_eq!(keys.len(), 8);
        assert_eq!(keys["context_used_pct"], serde_json::json!(48));
        assert_eq!(keys["session_tokens"], serde_json::json!(154_000));
        assert_eq!(keys["compaction_count"], serde_json::json!(1));
        assert_eq!(
            keys["session_cost_measured_at"],
            serde_json::json!("2026-10-02T00:00:00Z")
        );
        // 140k input + 14k output opus tokens.
        let expected: f64 = (140_000.0 * 4.0 + 14_000.0 * 20.0) / 1_000_000.0 * 100.0;
        assert_eq!(
            keys["session_cost_cents"],
            serde_json::json!(expected.round() as u64)
        );

        let absent = served_session_metrics_keys(&RegistryEntry::default());
        assert_eq!(absent["session_cost_cents"], serde_json::json!(null));
        assert_eq!(absent["session_cost_measured_at"], serde_json::json!(null));
        assert_eq!(absent["compaction_count"], serde_json::json!(null));

        let mut unresolved = RegistryEntry::default();
        unresolved.harness = Some("pi".into());
        unresolved.harness_session_id = Some("session-without-transcript".into());
        let unresolved = served_session_metrics_keys(&unresolved);
        assert_eq!(
            unresolved["compaction_count"],
            serde_json::json!(null),
            "an identity without a supported transcript stays unknown"
        );

        let no_catalog = tmpdir("no-catalog");
        measure_session_cost(
            &no_catalog,
            "sess-no-catalog",
            &path,
            None,
            None,
            "2026-10-02T00:01:00Z",
        );
        assert_eq!(
            served_compaction_count(Some("sess-no-catalog")),
            Some(1),
            "transcript boundaries are measured without a price catalog"
        );

        mark_session_transcript_unavailable("sess-cost-1");
        assert_eq!(
            served_compaction_count(Some("sess-cost-1")),
            None,
            "an unavailable transcript does not keep a stale count"
        );
    }

    #[test]
    fn price_book_reloads_when_the_cache_file_moves() {
        let dir = tmpdir("reload");
        fixture_book(&dir);
        assert!(price_book(&dir).is_some());
        // A directory with no cache reads as absent.
        let empty = tmpdir("reload-empty");
        assert!(price_book(&empty).is_none());
        assert!(price_book(&dir).is_some());
    }
}
