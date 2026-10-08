use super::string_at;
use crate::transcript_tail;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub(super) struct TranscriptFacts {
    pub(super) runs: Vec<ModelRun>,
    pub(super) permissions: Vec<String>,
    pub(super) agent_name: Option<String>,
    pub(super) path: Option<PathBuf>,
}

#[derive(Debug)]
pub(super) struct ModelRun {
    pub(super) model: String,
    pub(super) first_ts: Option<String>,
    pub(super) last_ts: Option<String>,
    pub(super) turns: usize,
}
pub(super) fn find_many(sids: &[String]) -> HashMap<String, TranscriptFacts> {
    let wanted = sids.iter().map(String::as_str).collect::<Vec<_>>();
    transcript_tail::find_transcripts(&wanted)
        .into_iter()
        .filter_map(|(sid, path)| scan(&path).ok().map(|facts| (sid, facts)))
        .collect()
}

pub(super) fn scan(path: &Path) -> Result<TranscriptFacts, String> {
    let file = fs::File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let mut facts = TranscriptFacts::default();
    facts.path = Some(path.to_path_buf());
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
