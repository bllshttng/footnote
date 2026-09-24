use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
pub const ASTRA_PREPARATION_PERCENT: u64 = 20;
pub const ASTRA_ACTION_PERCENT: u64 = 25;
pub const ASTRA_ACTION_TOKENS: u64 = 250_000;
const ASTRA_DEFAULT_CONTEXT_WINDOW: u64 = 272_000;
const ASTRA_MAX_CONTEXT_WINDOW: u64 = 872_000;
const ASTRA_EFFECTIVE_PERCENT: u64 = 95;
const TAIL_BYTES: u64 = 256 * 1024;
const EXPANDED_TAIL_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextWindowReceipt {
    pub model: String,
    pub context_window: Option<u64>,
    pub max_context_window: Option<u64>,
    pub effective_context_window_percent: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextWindowRequest {
    pub model_context_window: Option<u64>,
    pub effective_context_window_percent: Option<u64>,
}

impl ContextWindowRequest {
    pub fn from_config(
        config: Option<&serde_json::Map<String, Value>>,
    ) -> Result<Option<Self>, ContextWindowError> {
        let Some(config) = config else {
            return Ok(None);
        };
        let request = Self {
            model_context_window: configured_u64(config, "model_context_window")?,
            effective_context_window_percent: configured_u64(
                config,
                "effective_context_window_percent",
            )?,
        };
        Ok((request.model_context_window.is_some()
            || request.effective_context_window_percent.is_some())
        .then_some(request))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextUsage {
    pub model: String,
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl ContextUsage {
    pub fn used_tokens(&self) -> Option<u64> {
        self.input_tokens
            .checked_add(self.cache_creation_input_tokens)?
            .checked_add(self.cache_read_input_tokens)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionBand {
    None,
    Prepare,
    Action,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextReading {
    pub usage: ContextUsage,
    pub used_tokens: u64,
    pub window_tokens: u64,
    pub used_pct: u64,
    pub band: CompactionBand,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextWindowError {
    Missing(&'static str),
    Invalid(&'static str),
    Unreadable(String),
}

pub fn effective_window(receipt: &ContextWindowReceipt) -> Result<u64, ContextWindowError> {
    let configured = receipt
        .context_window
        .ok_or(ContextWindowError::Missing("context_window"))?;
    let cap = receipt
        .max_context_window
        .ok_or(ContextWindowError::Missing("max_context_window"))?;
    if configured == 0 || cap == 0 {
        return Err(ContextWindowError::Invalid(
            "context window must be positive",
        ));
    }
    let percent = receipt.effective_context_window_percent.unwrap_or(100);
    if !(1..=100).contains(&percent) {
        return Err(ContextWindowError::Invalid(
            "effective context window percent must be between 1 and 100",
        ));
    }
    let effective = configured
        .min(cap)
        .checked_mul(percent)
        .map(|value| value / 100)
        .ok_or(ContextWindowError::Invalid(
            "context window arithmetic overflow",
        ))?;
    if effective == 0 {
        return Err(ContextWindowError::Invalid(
            "effective context window must be positive",
        ));
    }
    Ok(effective)
}

pub fn window_for_model(model: &str) -> u64 {
    if model.contains("[1m]")
        || model.contains("glm-5.2")
        || model.contains("glm-5.3")
        || model.contains("opus-5")
        || model.contains("sonnet-5")
        || model.contains("fable-5")
        || model.contains("opus-4-8")
        || model.contains("opus-4-7")
        || model.contains("opus-4-6")
        || model.contains("sonnet-4-6")
    {
        1_000_000
    } else {
        DEFAULT_CONTEXT_WINDOW
    }
}

pub fn effective_window_for_model(
    model: &str,
    session_id: &str,
) -> Result<u64, ContextWindowError> {
    if !is_astra_model(model) {
        return Ok(window_for_model(model));
    }
    if session_id.trim().is_empty() {
        return Err(ContextWindowError::Missing("exact Codex session id"));
    }
    let home = crate::paths::AgentsHome::from_env_opt()
        .ok_or(ContextWindowError::Missing("declared Codex registry root"))?;
    let registry = crate::state::load_registry(&home.registry_json())
        .map_err(|error| ContextWindowError::Unreadable(error.to_string()))?;
    let entry = registry
        .find_by_session("codex", session_id)
        .ok_or(ContextWindowError::Missing("Codex session registry row"))?;
    let carry = crate::codex_thread::parse_harness_args(&entry.harness_args)
        .map_err(ContextWindowError::Unreadable)?;
    let request =
        ContextWindowRequest::from_config(Some(&carry.config))?.unwrap_or(ContextWindowRequest {
            model_context_window: None,
            effective_context_window_percent: None,
        });
    let configured = request
        .model_context_window
        .unwrap_or(ASTRA_DEFAULT_CONTEXT_WINDOW);
    let percent = request
        .effective_context_window_percent
        .unwrap_or(ASTRA_EFFECTIVE_PERCENT);
    effective_window(&ContextWindowReceipt {
        model: model.to_string(),
        context_window: Some(configured),
        max_context_window: Some(ASTRA_MAX_CONTEXT_WINDOW),
        effective_context_window_percent: Some(percent),
    })
}

fn configured_u64(
    config: &serde_json::Map<String, Value>,
    key: &'static str,
) -> Result<Option<u64>, ContextWindowError> {
    match config.get(key) {
        None => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or(ContextWindowError::Invalid(
            "Codex context setting is not an unsigned integer",
        )),
    }
}

pub fn is_astra_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("gpt-6-astra")
}

/// Round one usage reading to the same whole-percent display used by hooks.
pub fn used_percent(used_tokens: u64, window_tokens: u64) -> Option<u64> {
    if window_tokens == 0 {
        return None;
    }
    Some(((used_tokens as u128 * 100 + (window_tokens as u128 / 2)) / window_tokens as u128) as u64)
}

pub fn compaction_band(model: &str, used_tokens: u64, window_tokens: u64) -> CompactionBand {
    if !is_astra_model(model) || window_tokens == 0 {
        return CompactionBand::None;
    }
    let Some(used_pct) = used_percent(used_tokens, window_tokens) else {
        return CompactionBand::None;
    };
    if used_pct >= ASTRA_ACTION_PERCENT || used_tokens >= ASTRA_ACTION_TOKENS {
        CompactionBand::Action
    } else if used_pct >= ASTRA_PREPARATION_PERCENT {
        CompactionBand::Prepare
    } else {
        CompactionBand::None
    }
}

pub fn parse_usage_record(record: &Value) -> Option<ContextUsage> {
    if record.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let message = record.get("message")?.as_object()?;
    let usage = message.get("usage")?.as_object()?;
    let number = |key: &str| usage.get(key)?.as_u64();
    Some(ContextUsage {
        model: message
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        input_tokens: number("input_tokens")?,
        cache_creation_input_tokens: number("cache_creation_input_tokens").unwrap_or(0),
        cache_read_input_tokens: number("cache_read_input_tokens").unwrap_or(0),
    })
}

/// Read the session id from the bounded first record of a Codex rollout.
/// The transcript path already resolved by the caller is the identity source
/// when the hook environment does not carry `CODEX_THREAD_ID`.
pub fn rollout_session_id(path: &Path) -> Option<String> {
    let mut prefix = Vec::new();
    File::open(path)
        .ok()?
        .take(64 * 1024)
        .read_to_end(&mut prefix)
        .ok()?;
    let first_line = prefix.split(|byte| *byte == b'\n').next()?;
    let record: Value = serde_json::from_slice(first_line).ok()?;
    record
        .pointer("/payload/id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(str::to_string)
}

pub fn read_last_usage(path: &Path) -> Result<Option<ContextUsage>, ContextWindowError> {
    let mut file =
        File::open(path).map_err(|error| ContextWindowError::Unreadable(error.to_string()))?;
    let size = file
        .metadata()
        .map_err(|error| ContextWindowError::Unreadable(error.to_string()))?
        .len();
    for limit in [TAIL_BYTES, EXPANDED_TAIL_BYTES] {
        let start = size.saturating_sub(limit);
        file.seek(SeekFrom::Start(start))
            .map_err(|error| ContextWindowError::Unreadable(error.to_string()))?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(size.saturating_sub(start))
            .read_to_end(&mut bytes)
            .map_err(|error| ContextWindowError::Unreadable(error.to_string()))?;
        if start > 0 {
            if let Some(first_newline) = bytes.iter().position(|byte| *byte == b'\n') {
                bytes.drain(..=first_newline);
            } else {
                bytes.clear();
            }
        }
        for line in bytes.rsplit(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if let Some(usage) = parse_usage_record(&record) {
                return Ok(Some(usage));
            }
        }
        if size <= limit {
            break;
        }
    }
    Ok(None)
}

pub fn verify_compaction_receipt(
    receipt: &Value,
    thread_id: &str,
) -> Result<Value, ContextWindowError> {
    let reported_thread = receipt
        .get("threadId")
        .or_else(|| receipt.pointer("/params/threadId"))
        .and_then(Value::as_str)
        .ok_or(ContextWindowError::Missing("threadId"))?;
    if reported_thread != thread_id {
        return Err(ContextWindowError::Invalid(
            "compaction receipt thread mismatch",
        ));
    }
    let kind = receipt
        .get("type")
        .or_else(|| receipt.pointer("/item/type"))
        .or_else(|| receipt.pointer("/params/item/type"))
        .and_then(Value::as_str)
        .ok_or(ContextWindowError::Missing("contextCompaction type"))?;
    if kind != "contextCompaction" {
        return Err(ContextWindowError::Invalid(
            "provider did not confirm context compaction",
        ));
    }
    let status = receipt
        .get("status")
        .or_else(|| receipt.pointer("/item/status"))
        .or_else(|| receipt.pointer("/params/item/status"))
        .and_then(Value::as_str)
        .ok_or(ContextWindowError::Missing("contextCompaction status"))?;
    if status != "completed" {
        return Err(ContextWindowError::Invalid(
            "provider compaction receipt is not completed",
        ));
    }
    Ok(receipt.clone())
}

#[cfg(test)]
mod tests {
    use super::{
        compaction_band, effective_window, used_percent, window_for_model, CompactionBand,
        ContextWindowError, ContextWindowReceipt, ASTRA_DEFAULT_CONTEXT_WINDOW,
        ASTRA_EFFECTIVE_PERCENT, ASTRA_MAX_CONTEXT_WINDOW, EXPANDED_TAIL_BYTES,
    };
    use serde_json::Value;
    use std::io::Write;

    #[test]
    fn ac3_hp_rust_owns_the_provider_effective_window_calculation() {
        let receipt = ContextWindowReceipt {
            model: "astra-1".into(),
            context_window: Some(1_000_000),
            max_context_window: Some(800_000),
            effective_context_window_percent: Some(100),
        };

        assert_eq!(effective_window(&receipt).unwrap(), 800_000);
    }

    #[test]
    fn ac3_edge_missing_provider_cap_is_not_treated_as_a_full_window() {
        let receipt = ContextWindowReceipt {
            model: "gpt-6-astra".into(),
            context_window: Some(1_000_000),
            max_context_window: None,
            effective_context_window_percent: Some(95),
        };

        assert_eq!(
            effective_window(&receipt),
            Err(ContextWindowError::Missing("max_context_window"))
        );
    }

    #[test]
    fn effective_window_refuses_a_zero_result_after_percent_rounding() {
        let receipt = ContextWindowReceipt {
            model: "gpt-6-astra".into(),
            context_window: Some(1),
            max_context_window: Some(1),
            effective_context_window_percent: Some(1),
        };

        assert_eq!(
            effective_window(&receipt),
            Err(ContextWindowError::Invalid(
                "effective context window must be positive"
            ))
        );
        assert_eq!(used_percent(1, 0), None);
    }

    #[test]
    fn ac3_hp_astra_enters_preparation_then_action_bands() {
        assert_eq!(
            compaction_band("gpt-6-astra", 200_000, 1_000_000),
            CompactionBand::Prepare
        );
        assert_eq!(
            compaction_band("gpt-6-astra", 250_000, 1_000_000),
            CompactionBand::Action
        );
    }

    #[test]
    fn context_percent_rounds_half_up() {
        assert_eq!(used_percent(307_850, 1_000_000), Some(31));
        assert_eq!(used_percent(265_000, 1_000_000), Some(27));
    }

    #[test]
    fn ac4_hp_non_astra_keeps_the_compaction_policy_unchanged() {
        assert_eq!(
            compaction_band("gpt-5.6-sol", 999_999, 1_000_000),
            CompactionBand::None
        );
    }

    #[test]
    fn non_astra_context_windows_keep_the_model_allowlist() {
        for model in [
            "glm-5.2[1m]",
            "glm-5.2",
            "glm-5.3",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-fable-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
        ] {
            assert_eq!(window_for_model(model), 1_000_000, "{model}");
        }
        for model in ["claude-haiku-4-5", "claude-opus-4-5", "future-model"] {
            assert_eq!(window_for_model(model), 200_000, "{model}");
        }
    }

    #[test]
    fn rollout_session_id_uses_bounded_metadata_not_the_filename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("rollout-file-id-is-not-authoritative.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-exact\"}}\n",
        )
        .unwrap();

        assert_eq!(
            super::rollout_session_id(&path).as_deref(),
            Some("thread-exact")
        );
    }

    #[test]
    fn last_usage_reader_finds_a_recent_record_without_loading_old_transcript_bytes() {
        let path = std::env::temp_dir().join(format!(
            "fno-context-probe-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&vec![b'x'; EXPANDED_TAIL_BYTES as usize + 100])
            .unwrap();
        file.write_all(b"\n").unwrap();
        file.write_all(
            br#"{"type":"assistant","message":{"model":"gpt-6-astra","usage":{"input_tokens":17,"cache_creation_input_tokens":2,"cache_read_input_tokens":3}}}"#,
        )
        .unwrap();
        drop(file);

        let usage = super::read_last_usage(&path).unwrap().unwrap();
        assert_eq!(usage.model, "gpt-6-astra");
        assert_eq!(usage.used_tokens(), Some(22));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn last_usage_reader_does_not_fall_back_to_a_stale_record_before_its_tail_budget() {
        let path = std::env::temp_dir().join(format!(
            "fno-context-probe-stale-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(
            br#"{"type":"assistant","message":{"model":"gpt-6-astra","usage":{"input_tokens":17}}}"#,
        )
        .unwrap();
        file.write_all(b"\n").unwrap();
        file.write_all(&vec![b'x'; EXPANDED_TAIL_BYTES as usize + 100])
            .unwrap();
        drop(file);

        assert!(super::read_last_usage(&path).unwrap().is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn ac3_resume_explicit_context_request_round_trips_into_config() {
        let mut config = serde_json::Map::new();
        config.insert("model_context_window".into(), 800_000.into());
        let request = super::ContextWindowRequest::from_config(Some(&config))
            .unwrap()
            .unwrap();
        assert_eq!(request.model_context_window, Some(800_000));
        assert_eq!(request.effective_context_window_percent, None);
    }

    #[test]
    fn astra_effective_window_uses_the_registry_request_or_default() {
        let explicit = serde_json::json!({
            "model_context_window": 1_000_000,
            "effective_context_window_percent": 95
        });
        let explicit: serde_json::Map<String, Value> = serde_json::from_value(explicit).unwrap();
        let request = super::ContextWindowRequest::from_config(Some(&explicit))
            .unwrap()
            .unwrap();
        let configured = request.model_context_window.unwrap();
        assert_eq!(
            super::effective_window(&ContextWindowReceipt {
                model: "gpt-6-astra".into(),
                context_window: Some(configured),
                max_context_window: Some(ASTRA_MAX_CONTEXT_WINDOW),
                effective_context_window_percent: Some(95),
            })
            .unwrap(),
            828_400
        );
        assert_eq!(
            super::effective_window(&ContextWindowReceipt {
                model: "gpt-6-astra".into(),
                context_window: Some(ASTRA_DEFAULT_CONTEXT_WINDOW),
                max_context_window: Some(ASTRA_MAX_CONTEXT_WINDOW),
                effective_context_window_percent: Some(ASTRA_EFFECTIVE_PERCENT),
            })
            .unwrap(),
            258_400
        );
    }

    #[test]
    fn ac3_err_compaction_requires_a_verified_provider_receipt() {
        let receipt = serde_json::json!({
            "threadId": "thread-a",
            "type": "contextCompaction",
            "status": "started"
        });
        assert!(matches!(
            super::verify_compaction_receipt(&receipt, "thread-a"),
            Err(ContextWindowError::Invalid(_))
        ));
    }
}
