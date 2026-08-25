use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Open,
    Working,
    Delegating,
    Sealing,
    Closed,
    Aborted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEvent {
    DispatchClassified,
    PrepareHandoff,
    SuccessorProven,
    SuccessorUnproven,
    TerminalDecided,
    FinalizeDone,
    Cancel,
    Abort,
    ReleaseClaim,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid transition {from:?} + {event:?}")]
pub struct InvalidTransition {
    pub from: RunState,
    pub event: RunEvent,
}

pub fn step(from: RunState, event: RunEvent) -> Result<RunState, InvalidTransition> {
    let to = match (from, event) {
        (RunState::Open, RunEvent::DispatchClassified) => RunState::Working,
        (RunState::Working, RunEvent::DispatchClassified) => RunState::Working,
        (RunState::Working, RunEvent::PrepareHandoff) => RunState::Delegating,
        (RunState::Delegating, RunEvent::SuccessorProven) => RunState::Closed,
        (RunState::Delegating, RunEvent::SuccessorUnproven) => RunState::Working,
        (RunState::Working, RunEvent::TerminalDecided) => RunState::Sealing,
        (RunState::Sealing, RunEvent::FinalizeDone) => RunState::Closed,
        (
            RunState::Open | RunState::Working | RunState::Delegating | RunState::Sealing,
            RunEvent::Cancel | RunEvent::Abort,
        ) => RunState::Aborted,
        _ => return Err(InvalidTransition { from, event }),
    };
    Ok(to)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TransitionRecord {
    run_id: String,
    from: RunState,
    event: RunEvent,
    to: RunState,
}

#[derive(Debug, thiserror::Error)]
pub enum RunStateError {
    #[error("run log {path} unreadable: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("run log {path} line {line} is malformed: {error}")]
    MalformedLine {
        path: PathBuf,
        line: usize,
        error: serde_json::Error,
    },
    #[error("run log {path} line {line} violates the transition chain: {message}")]
    InvalidRecord {
        path: PathBuf,
        line: usize,
        message: String,
    },
    #[error(transparent)]
    InvalidTransition(#[from] InvalidTransition),
}

pub fn fold_run_state(path: &Path, run_id: &str) -> Result<RunState, RunStateError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(RunState::Open),
        Err(source) => {
            return Err(RunStateError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    fold_reader(file, path, run_id)
}

pub fn append_transition(
    path: &Path,
    run_id: &str,
    event: RunEvent,
) -> Result<RunState, RunStateError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| RunStateError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .map_err(|source| RunStateError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    file.try_lock().map_err(|source| RunStateError::Io {
        path: path.to_path_buf(),
        source: source.into(),
    })?;

    file.seek(SeekFrom::Start(0))
        .map_err(|source| RunStateError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let from = fold_reader(&mut file, path, run_id)?;
    let to = step(from, event)?;
    let record = TransitionRecord {
        run_id: run_id.to_string(),
        from,
        event,
        to,
    };
    file.seek(SeekFrom::End(0))
        .map_err(|source| RunStateError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    serde_json::to_writer(&mut file, &record).map_err(|error| RunStateError::InvalidRecord {
        path: path.to_path_buf(),
        line: 0,
        message: error.to_string(),
    })?;
    file.write_all(b"\n").map_err(|source| RunStateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.flush().map_err(|source| RunStateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    File::unlock(&file).map_err(|source| RunStateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(to)
}

fn fold_reader(
    reader: impl std::io::Read,
    path: &Path,
    run_id: &str,
) -> Result<RunState, RunStateError> {
    let mut state = RunState::Open;
    for (index, line) in BufReader::new(reader).lines().enumerate() {
        let line_number = index + 1;
        let line = line.map_err(|source| RunStateError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let record: TransitionRecord =
            serde_json::from_str(&line).map_err(|error| RunStateError::MalformedLine {
                path: path.to_path_buf(),
                line: line_number,
                error,
            })?;
        if record.run_id != run_id {
            continue;
        }
        if record.from != state {
            return Err(RunStateError::InvalidRecord {
                path: path.to_path_buf(),
                line: line_number,
                message: format!("record starts at {:?}, fold is at {state:?}", record.from),
            });
        }
        let expected = step(state, record.event)?;
        if record.to != expected {
            return Err(RunStateError::InvalidRecord {
                path: path.to_path_buf(),
                line: line_number,
                message: format!("record ends at {:?}, expected {expected:?}", record.to),
            });
        }
        state = record.to;
    }
    Ok(state)
}
