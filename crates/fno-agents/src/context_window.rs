use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
pub const ASTRA_PREPARATION_PERCENT: u64 = 20;
pub const ASTRA_ACTION_PERCENT: u64 = 25;
pub const ASTRA_ACTION_TOKENS: u64 = 250_000;

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
    pub fn from_config(config: Option<&serde_json::Map<String, Value>>) -> Option<Self> {
        let config = config?;
        let request = Self {
            model_context_window: config.get("model_context_window").and_then(Value::as_u64),
            effective_context_window_percent: config
                .get("effective_context_window_percent")
                .and_then(Value::as_u64),
        };
        (request.model_context_window.is_some()
            || request.effective_context_window_percent.is_some())
        .then_some(request)
    }

    pub fn merge_into(&self, config: &mut serde_json::Map<String, Value>) {
        if let Some(window) = self.model_context_window {
            config
                .entry("model_context_window")
                .or_insert_with(|| Value::from(window));
        }
        if let Some(percent) = self.effective_context_window_percent {
            config
                .entry("effective_context_window_percent")
                .or_insert_with(|| Value::from(percent));
        }
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
    configured
        .min(cap)
        .checked_mul(percent)
        .map(|value| value / 100)
        .ok_or(ContextWindowError::Invalid(
            "context window arithmetic overflow",
        ))
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

pub fn is_astra_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("gpt-6-astra")
}

pub fn compaction_band(model: &str, used_tokens: u64, window_tokens: u64) -> CompactionBand {
    if !is_astra_model(model) || window_tokens == 0 {
        return CompactionBand::None;
    }
    let used_pct =
        ((used_tokens as u128 * 100 + (window_tokens as u128 / 2)) / window_tokens as u128) as u64;
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

pub fn read_last_usage(path: &Path) -> Result<Option<ContextUsage>, ContextWindowError> {
    let bytes =
        std::fs::read(path).map_err(|error| ContextWindowError::Unreadable(error.to_string()))?;
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
        compaction_band, effective_window, CompactionBand, ContextWindowError, ContextWindowReceipt,
    };

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
    fn ac4_hp_non_astra_keeps_the_compaction_policy_unchanged() {
        assert_eq!(
            compaction_band("gpt-5.6-sol", 999_999, 1_000_000),
            CompactionBand::None
        );
    }

    #[test]
    fn ac3_resume_explicit_context_request_round_trips_into_config() {
        let mut config = serde_json::Map::new();
        config.insert("model_context_window".into(), 800_000.into());
        let request = super::ContextWindowRequest::from_config(Some(&config)).unwrap();
        let mut resumed = serde_json::Map::new();
        request.merge_into(&mut resumed);
        assert_eq!(resumed, config);
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
