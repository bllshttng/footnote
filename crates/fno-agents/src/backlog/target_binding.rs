//! `fno backlog target-binding`: which node a target run may bind when the
//! node it names already carries a PR. One owner for `fno do target init`,
//! `fno do target start` and a direct `init-target-state.sh` run; each caller
//! only forwards its input and acts on the receipt.
//!
//! Four verdicts. `continue`: nothing shipped, bootstrap as before.
//! `adopt`: the caller stands in the open PR's own worktree, on its head
//! branch, so it is the author resuming that PR. `forked`: follow-up scope was
//! given, so an independent child carries it and the parent keeps its own
//! status, plan and PR. `refused`: the exact missing fact is named and nothing
//! was claimed or written.

use serde_json::{json, Value};
use std::path::Path;

use super::create_cli::{self, AddArgs};
use super::node_ref;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Init,
    Start,
}

pub struct Request {
    pub input: String,
    pub node: Option<String>,
    pub plan_path: Option<String>,
    pub phase: Phase,
    pub allow_in_review: bool,
}

#[derive(Debug, Default, PartialEq)]
pub struct Receipt {
    pub verdict: &'static str,
    pub reason: &'static str,
    pub source_node: Option<String>,
    pub effective_node: Option<String>,
    pub pr: Option<i64>,
    pub scope: Option<String>,
    pub reused: bool,
    pub effective_plan: Option<String>,
    pub message: String,
    pub next: Option<String>,
}

/// The two facts adoption reads: the PR's own state and head branch.
pub struct PrFact {
    pub state: String,
    pub head_ref: String,
}

/// Leading target modifiers that are never follow-up scope.
const MODIFIERS: [&str; 16] = [
    "s",
    "m",
    "l",
    "small",
    "medium",
    "large",
    "bg",
    "agent",
    "fork",
    "clean",
    "adversarial",
    "auto-merge",
    "beastmode",
    "beast",
    "batched",
    "--no-merge",
];

fn receipt(verdict: &'static str, reason: &'static str, source: Option<&str>) -> Receipt {
    Receipt {
        verdict,
        reason,
        source_node: source.map(str::to_string),
        effective_node: source.map(str::to_string),
        ..Default::default()
    }
}

fn str_field<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn set(row: &Value, key: &str) -> bool {
    row.get(key).is_some_and(|v| !v.is_null())
}

/// A shipped-but-open delivery: a PR is bound and the node is not closed.
/// The same ladder the derived `in_review` status reads.
fn is_delivered(row: &Value) -> bool {
    set(row, "pr_number")
        && !set(row, "completed_at")
        && !set(row, "superseded_by")
        && !set(row, "deferred_at")
}

fn id_of(row: &Value) -> String {
    str_field(row, "id").unwrap_or_default().to_string()
}

/// The node the request names: the caller's resolved id when it passed one,
/// else exactly one distinct id-shaped token the graph confirms.
fn resolve_source<'a>(req: &Request, rows: &'a [Value]) -> Option<&'a Value> {
    if let Some(node) = req.node.as_deref().filter(|n| !n.is_empty()) {
        return node_ref::find_node(rows, node);
    }
    let mut hits: Vec<&Value> = Vec::new();
    for tok in req.input.split_whitespace() {
        if !node_ref::is_wellformed_node_id(tok) {
            continue;
        }
        if let Some(row) = node_ref::find_node(rows, tok) {
            if !hits.iter().any(|h| id_of(h) == id_of(row)) {
                hits.push(row);
            }
        }
    }
    match hits.as_slice() {
        [one] => Some(one),
        _ => None,
    }
}

/// The follow-up scope: the input minus leading modifiers and every spelling
/// of the source node, whitespace collapsed. None when nothing is left.
pub fn scope_of(input: &str, node_id: &str, rows: &[Value]) -> Option<String> {
    let names_node = |tok: &str| {
        tok == node_id || node_ref::find_node(rows, tok).is_some_and(|r| id_of(r) == node_id)
    };
    let mut words: Vec<&str> = Vec::new();
    let mut leading = true;
    let mut tokens = input.split_whitespace().peekable();
    while let Some(tok) = tokens.next() {
        let lower = tok.to_lowercase();
        if leading && MODIFIERS.contains(&lower.as_str()) {
            if lower == "beast"
                && tokens
                    .peek()
                    .is_some_and(|t| t.eq_ignore_ascii_case("mode"))
            {
                tokens.next();
            }
            continue;
        }
        if names_node(tok) {
            leading = false;
            continue;
        }
        leading = false;
        words.push(tok);
    }
    let scope = words.join(" ");
    let scope = scope.trim_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace());
    (!scope.is_empty()).then(|| scope.to_string())
}

/// Adoption proof: the PR is OPEN and its head branch is the one this linked
/// worktree has checked out. Any unread fact fails the proof.
pub fn adoption_proven(pr: Option<&PrFact>, branch: Option<&str>) -> bool {
    match (pr, branch) {
        (Some(pr), Some(branch)) => {
            pr.state == "OPEN" && !pr.head_ref.is_empty() && pr.head_ref == branch
        }
        _ => false,
    }
}

/// The branch checked out in `cwd` when it is a linked worktree, else None.
fn linked_branch(cwd: &Path) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(cwd).args([
        "rev-parse",
        "--git-dir",
        "--git-common-dir",
        "--abbrev-ref",
        "HEAD",
    ]);
    let out = crate::bounded_cmd::output_with_timeout(cmd, 10)?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    match lines.as_slice() {
        [git_dir, common, branch]
            if git_dir != common && !branch.is_empty() && *branch != "HEAD" =>
        {
            Some(branch.to_string())
        }
        _ => None,
    }
}

/// `fno do pr info <n>` under a wall-clock bound, read as typed JSON.
fn read_pr(cwd: &Path, number: i64) -> Option<PrFact> {
    let mut cmd = std::process::Command::new(crate::scrape::fno_bin());
    cmd.current_dir(cwd)
        .args(["do", "pr", "info", &number.to_string()]);
    let out = crate::bounded_cmd::output_with_timeout(cmd, 60)?;
    if !out.status.success() {
        return None;
    }
    let info: Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(PrFact {
        state: str_field(&info, "state")?.to_uppercase(),
        head_ref: str_field(&info, "head_ref")?.to_string(),
    })
}

fn follow_up_recipe(node: &str) -> String {
    format!("fno do target start \"{node} <follow-up scope, one sentence>\"")
}

pub fn prepare(req: &Request, cwd: &Path) -> Receipt {
    let rows = match crate::graph_store::read_rows_strict(&super::settings::graph_path()) {
        Ok(rows) => rows,
        Err(err) => {
            let named = req.node.as_deref().is_some_and(|n| !n.is_empty())
                || req
                    .input
                    .split_whitespace()
                    .any(node_ref::is_wellformed_node_id);
            if !named {
                return receipt("continue", "no_node", None);
            }
            let mut r = receipt("refused", "graph_unreadable", None);
            r.message = format!(
                "target binding: REFUSED: the backlog graph is unreadable ({err}), so nothing \
                 can prove the named node has no PR. Nothing was claimed. Fix the graph and retry."
            );
            return r;
        }
    };
    let Some(source) = resolve_source(req, &rows) else {
        return receipt("continue", "no_node", None);
    };
    let id = id_of(source);
    if !is_delivered(source) {
        return receipt("continue", "not_delivered", Some(&id));
    }
    let pr = source.get("pr_number").and_then(Value::as_i64);
    let pr_label = pr.map(|n| format!(" #{n}")).unwrap_or_default();
    let scope = scope_of(&req.input, &id, &rows);
    let branch = || linked_branch(cwd);

    let Some(scope) = scope else {
        if req.allow_in_review {
            let mut r = receipt("refused", "missing_scope", Some(&id));
            r.pr = pr;
            r.message = format!(
                "target binding: REFUSED: node {id} already has PR{pr_label}. The in-review \
                 allowance mints a child node for follow-up work, and no follow-up scope was given."
            );
            r.next = Some(follow_up_recipe(&id));
            return r;
        }
        if req.phase == Phase::Start {
            // Start may be re-entering the PR's own worktree; init, run from
            // that tree, owns the adoption proof.
            return receipt("continue", "defer_to_init", Some(&id));
        }
        return adopt_or_refuse(source, pr, cwd, branch().as_deref());
    };

    // Prose inside the open PR's own worktree is repair work on that PR, not
    // a new delivery; only the explicit allowance forks from there.
    if !req.allow_in_review && req.phase == Phase::Init {
        if let (Some(n), Some(b)) = (pr, branch()) {
            if adoption_proven(read_pr(cwd, n).as_ref(), Some(&b)) {
                return adopted(&id, n, &b);
            }
        }
    }
    fork(req, source, &scope, cwd)
}

fn adopted(id: &str, pr: i64, branch: &str) -> Receipt {
    let mut r = receipt("adopt", "open_pr_worktree", Some(id));
    r.pr = Some(pr);
    r.message = format!(
        "target binding: ADOPTED: re-binding this session to node {id} on the open PR #{pr} \
         (branch {branch} is that PR's head). No new PR: drive this one with /fno:ship pr check."
    );
    r
}

fn adopt_or_refuse(source: &Value, pr: Option<i64>, cwd: &Path, branch: Option<&str>) -> Receipt {
    let id = id_of(source);
    if let (Some(n), Some(b)) = (pr, branch) {
        if adoption_proven(read_pr(cwd, n).as_ref(), Some(b)) {
            return adopted(&id, n, b);
        }
    }
    let pr_label = pr.map(|n| format!(" #{n}")).unwrap_or_default();
    let mut r = receipt("refused", "existing_pr", Some(&id));
    r.pr = pr;
    r.message = format!(
        "target binding: REFUSED: node {id} is in_review (PR{pr_label}). A fresh run would \
         bind a second PR to a node whose own PR already shipped.\n\n\
         Pick ONE:\n  \
         1) Address review on the existing PR: /fno:ship pr check, from that PR's own worktree\n  \
         2) New work beyond that PR gets its own child node:\n       {}",
        follow_up_recipe(&id)
    );
    r.next = Some(follow_up_recipe(&id));
    r
}

/// A child born for `scope` under `parent`: the same parent, the same
/// source node and the same normalized details. Closed and set-aside rows
/// never match.
fn is_follow_up_of(row: &Value, parent: &str, scope: &str) -> bool {
    str_field(row, "parent") == Some(parent)
        && str_field(row, "source_node_id") == Some(parent)
        && !set(row, "superseded_by")
        && !set(row, "deferred_at")
        && str_field(row, "details")
            .is_some_and(|d| d.split_whitespace().collect::<Vec<_>>().join(" ") == scope)
}

fn child_args(parent: &Value, scope: &str) -> AddArgs {
    let id = id_of(parent);
    let headline: String = scope.chars().take(90).collect();
    let type_ = match str_field(parent, "type") {
        Some("bug") => "bug",
        _ => "feature",
    };
    AddArgs {
        title: format!("Follow-up to {id}: {headline}"),
        domain: str_field(parent, "domain").unwrap_or("code").to_string(),
        priority: str_field(parent, "priority").unwrap_or("p2").to_string(),
        blocks_everything: parent
            .get("blocks_everything")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        difficulty: Some(
            str_field(parent, "difficulty")
                .unwrap_or("medium")
                .to_string(),
        ),
        parent: Some(id.clone()),
        type_: type_.to_string(),
        project: str_field(parent, "project").map(str::to_string),
        cwd: str_field(parent, "cwd").map(str::to_string),
        details: Some(scope.to_string()),
        source_node: Some(id),
        source_kind: "from_supervisor".to_string(),
        ..Default::default()
    }
}

fn fork(req: &Request, parent: &Value, scope: &str, cwd: &Path) -> Receipt {
    let id = id_of(parent);
    let mut r = receipt("refused", "actor_unknown", Some(&id));
    r.scope = Some(scope.to_string());
    if create_cli::session_provenance(cwd, Some(&id), None)
        .0
        .is_none()
    {
        r.message = format!(
            "target binding: REFUSED: follow-up work on {id} needs a child node, and this \
             process cannot prove which session would own its birth. Run it from a harness \
             session."
        );
        return r;
    }
    let args = child_args(parent, scope);
    let born = match create_cli::create_node(&args, &|rows| {
        rows.iter()
            .find(|row| is_follow_up_of(row, &id, scope))
            .cloned()
    }) {
        Ok(born) => born,
        Err(refusal) => {
            r.reason = "birth_refused";
            r.message = format!(
                "target binding: REFUSED: the follow-up child for {id} could not be filed: {}",
                refusal.message
            );
            return r;
        }
    };
    if let Some(row) = &born.reused {
        r.effective_node = Some(born.id.clone());
        if set(row, "completed_at") {
            r.reason = "already_delivered";
            r.message = format!(
                "target binding: REFUSED: follow-up {} already delivered this scope for {id}. \
                 File new work with different scope.",
                born.id
            );
            return r;
        }
        if let Some(n) = row.get("pr_number").and_then(Value::as_i64) {
            if let Some(b) = linked_branch(cwd) {
                if adoption_proven(read_pr(cwd, n).as_ref(), Some(&b)) {
                    return adopted(&born.id, n, &b);
                }
            }
            r.reason = "child_has_pr";
            r.pr = Some(n);
            r.message = format!(
                "target binding: REFUSED: follow-up {} already carries PR #{n} for this scope. \
                 Drive that PR from its own worktree with /fno:ship pr check.",
                born.id
            );
            return r;
        }
    }
    let parent_plan = str_field(parent, "plan_path");
    let effective_plan = req
        .plan_path
        .clone()
        .filter(|p| !p.is_empty() && Some(p.as_str()) != parent_plan);
    let verb = if born.reused.is_some() {
        "reusing"
    } else {
        "filed"
    };
    Receipt {
        verdict: "forked",
        reason: if born.reused.is_some() {
            "reused_child"
        } else {
            "new_child"
        },
        source_node: Some(id.clone()),
        effective_node: Some(born.id.clone()),
        pr: parent.get("pr_number").and_then(Value::as_i64),
        scope: Some(scope.to_string()),
        reused: born.reused.is_some(),
        effective_plan,
        message: format!(
            "target binding: FORKED: {id} already has a PR, so this follow-up binds child {} \
             ({verb}). {id} stays open with its own PR and plan.",
            born.id
        ),
        next: Some(format!("fno do target start {} --no-merge", born.id)),
    }
}

impl Receipt {
    pub fn to_json(&self) -> Value {
        json!({
            "verdict": self.verdict,
            "reason": self.reason,
            "source_node": self.source_node,
            "effective_node": self.effective_node,
            "pr": self.pr,
            "scope": self.scope,
            "reused": self.reused,
            "effective_plan": self.effective_plan,
            "message": self.message,
            "next": self.next,
        })
    }

    /// Exit code for the shell form: 0 proceed, 3 forked, 1 refused.
    pub fn exit_code(&self) -> i32 {
        match self.verdict {
            "forked" => 3,
            "refused" => 1,
            _ => 0,
        }
    }
}

const USAGE: &str = "usage: fno backlog target-binding (--stdin | --input <text> [--node <id>] \
[--plan-path <path>] [--phase init|start] [--allow-in-review]) [--env]";

/// `fno backlog target-binding`: internal, called by target init/start and
/// the init hook. JSON on stdout by default (exit 0); `--env` prints
/// `key=value` lines and the message on stderr, the verdict in the exit code.
pub fn run(tail: &[String]) -> i32 {
    let mut req = Request {
        input: String::new(),
        node: None,
        plan_path: None,
        phase: Phase::Init,
        allow_in_review: false,
    };
    let mut env_out = false;
    let mut it = tail.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned();
        match arg.as_str() {
            "--stdin" => {
                let mut text = String::new();
                let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut text);
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    eprintln!("target-binding: --stdin wants a JSON object\n{USAGE}");
                    return 2;
                };
                req.input = str_field(&v, "input").unwrap_or_default().to_string();
                req.node = str_field(&v, "node").map(str::to_string);
                req.plan_path = str_field(&v, "plan_path").map(str::to_string);
                req.phase = if str_field(&v, "phase") == Some("start") {
                    Phase::Start
                } else {
                    Phase::Init
                };
                req.allow_in_review = v
                    .get("allow_in_review")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            }
            "--input" => req.input = value().unwrap_or_default(),
            "--node" => req.node = value().filter(|v| !v.is_empty()),
            "--plan-path" => req.plan_path = value().filter(|v| !v.is_empty()),
            "--phase" => match value().as_deref() {
                Some("init") => req.phase = Phase::Init,
                Some("start") => req.phase = Phase::Start,
                _ => {
                    eprintln!("{USAGE}");
                    return 2;
                }
            },
            "--allow-in-review" => req.allow_in_review = true,
            "--env" => env_out = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            _ => {
                eprintln!("target-binding: unknown argument {arg}\n{USAGE}");
                return 2;
            }
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let r = prepare(&req, &cwd);
    if !env_out {
        println!("{}", r.to_json());
        return 0;
    }
    if !r.message.is_empty() {
        eprintln!("{}", r.message);
    }
    let field = |v: &Option<String>| v.clone().unwrap_or_default();
    println!("verdict={}", r.verdict);
    println!("reason={}", r.reason);
    println!("source_node={}", field(&r.source_node));
    println!("effective_node={}", field(&r.effective_node));
    println!("pr={}", r.pr.map(|n| n.to_string()).unwrap_or_default());
    println!("next={}", field(&r.next));
    r.exit_code()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, extra: Value) -> Value {
        let mut v = json!({"id": id, "status": "in_review"});
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    }

    #[test]
    fn follow_up_binding_reads_scope_delivery_and_adoption_proof() {
        let rows = vec![
            row(
                "x-aaaa",
                json!({"pr_number": 2868, "plan_path": "/p/plan.md"}),
            ),
            row("x-bbbb", json!({})),
        ];
        // Scope is the prose beside the node, modifiers and spellings dropped.
        assert_eq!(
            scope_of(
                "beast mode x-aaaa watch-expiry recovery only",
                "x-aaaa",
                &rows
            ),
            Some("watch-expiry recovery only".into())
        );
        assert_eq!(scope_of("xaaaa", "x-aaaa", &rows), None);
        assert_eq!(scope_of("L --no-merge x-aaaa \"\"", "x-aaaa", &rows), None);
        // A modifier word inside the prose is scope, not a modifier.
        assert_eq!(
            scope_of("x-aaaa clean up roots", "x-aaaa", &rows),
            Some("clean up roots".into())
        );

        // Delivered: a PR is bound and the node is not closed.
        assert!(is_delivered(&rows[0]));
        assert!(!is_delivered(&rows[1]));
        assert!(!is_delivered(&row(
            "x-d0ne",
            json!({"pr_number": 1, "completed_at": "t"})
        )));

        // Adoption needs an OPEN PR whose head is this worktree's branch.
        let open = PrFact {
            state: "OPEN".into(),
            head_ref: "feature/a".into(),
        };
        let merged = PrFact {
            state: "MERGED".into(),
            head_ref: "feature/a".into(),
        };
        assert!(adoption_proven(Some(&open), Some("feature/a")));
        assert!(!adoption_proven(Some(&open), Some("feature/b")));
        assert!(!adoption_proven(Some(&merged), Some("feature/a")));
        assert!(!adoption_proven(None, Some("feature/a")));
        assert!(!adoption_proven(Some(&open), None));

        // Retry dedupe: same parent, source and normalized scope; a set-aside
        // child or a different scope stays distinct.
        let child = row(
            "x-cccc",
            json!({"parent": "x-aaaa", "source_node_id": "x-aaaa",
            "details": "watch-expiry  recovery only"}),
        );
        assert!(is_follow_up_of(
            &child,
            "x-aaaa",
            "watch-expiry recovery only"
        ));
        assert!(!is_follow_up_of(&child, "x-aaaa", "another scope"));
        let shelved = row(
            "x-dddd",
            json!({"parent": "x-aaaa", "source_node_id": "x-aaaa",
            "details": "watch-expiry recovery only", "superseded_by": "x-9"}),
        );
        assert!(!is_follow_up_of(
            &shelved,
            "x-aaaa",
            "watch-expiry recovery only"
        ));

        // The child copies identity fields, never the parent's plan or PR.
        let args = child_args(&rows[0], "watch-expiry recovery only");
        assert_eq!(args.parent.as_deref(), Some("x-aaaa"));
        assert_eq!(args.source_node.as_deref(), Some("x-aaaa"));
        assert_eq!(args.details.as_deref(), Some("watch-expiry recovery only"));
        assert!(args.title.starts_with("Follow-up to x-aaaa: "));

        // Source resolution: exactly one confirmed id, else no node.
        let req = |input: &str| Request {
            input: input.into(),
            node: None,
            plan_path: None,
            phase: Phase::Init,
            allow_in_review: false,
        };
        assert_eq!(
            resolve_source(&req("x-aaaa more"), &rows).map(id_of),
            Some("x-aaaa".into())
        );
        assert!(resolve_source(&req("x-aaaa x-bbbb"), &rows).is_none());
        assert!(resolve_source(&req("fix the login bug"), &rows).is_none());

        // Exit codes the hook reads.
        let mut r = receipt("forked", "new_child", Some("x-aaaa"));
        assert_eq!(r.exit_code(), 3);
        r.verdict = "refused";
        assert_eq!(r.exit_code(), 1);
        r.verdict = "adopt";
        assert_eq!(r.exit_code(), 0);
    }
}
