//! The plan-promise gate: did the plan's declared work all ship? The
//! resolve_promise_evidence twin, ported source-of-truth text intact, so the
//! native close cannot bypass the gates the Python close ran. Fails open
//! (outcome Ok) on an absent, unreadable or unparseable plan so a stale
//! plan_path never wedges a close; the warning names the path.

use serde_json::Value;
use std::path::{Path, PathBuf};

use super::merge_evidence::{
    node_pr_refs, query_pr_state, repo_slug_from_url, PrReadError,
};
use crate::acceptance_evidence::decide_probe_run;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromiseOutcome {
    Ok,
    Unmet,
 sloppy sloppy
    Unknown,
}
