use super::string_at;
use crate::transcript_tail;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Debug, Default)]
pub(super) struct TranscriptFacts {
    pub(super) runs: Vec<ModelRun>,
    pub(super) permissions: Vec<String>,
    pub(super) agent_name: Option<String>,
}

#[derive(Debug)]
pub(super) struct ModelRun {
    pub(super) model: String,
    pub(super) first_ts: Option<String>,
    pub(super) last_ts: Option<String>,
    pub(super) turns: usize,
}
pub(super) fn find_transcript(sid: &str) -> Option<TranscriptFacts> {
    let found = transcript_tail::find_transcripts(&[sid]);
    let path = found.get(sid)?;
    scan(path).ok()
}

pub(super) fn scan(path: &Path) -> Result<TranscriptFacts, String> {
    let file = fs::File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let mut facts = TranscriptFacts::default();
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|err| format!("{}: {err}", path.display()))?;
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match record["type"].as_str().unwrap_or_default() {
            "assistant" => {
                let Some(model) = record["message"]["model"].as_str() else {
                    continue;
                };
                if model == "<synthetic>" {
                    continue;
                }
                let timestamp = string_at(&record, &["timestamp"]);
                if let Some(run) = facts.runs.last_mut().filter(|run| run.model == model) {
                    run.turns += 1;
                    if timestamp.is_some() {
                        run.last_ts = timestamp;
                    }
                } else {
                    facts.runs.push(ModelRun {
                        model: model.to_string(),
                        first_ts: timestamp.clone(),
                        last_ts: timestamp,
                        turns: 1,
                    });
                }
            }
            "turn_context" => {
                let Some(model) = record["payload"]["model"].as_str() else {
                    continue;
                };
                let timestamp = string_at(&record, &["timestamp"]);
                if let Some(run) = facts.runs.last_mut().filter(|run| run.model == model) {
                    run.turns += 1;
                    if timestamp.is_some() {
                        run.last_ts = timestamp;
                    }
                } else {
                    facts.runs.push(ModelRun {
                        model: model.to_string(),
                        first_ts: timestamp.clone(),
                        last_ts: timestamp,
                        turns: 1,
                    });
                }
            }
            "permission-mode" => {
                if let Some(mode) = record["permissionMode"].as_str().filter(|s| !s.is_empty()) {
                    facts.permissions.push(mode.to_string());
                }
            }
            "agent-name" => {
                if facts.agent_name.is_none() {
                    facts.agent_name = record["agentName"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                }
            }
            _ => {}
        }
    }
    Ok(facts)
}
