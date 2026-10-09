//! The tool set. Names and input fields match Claude Code's, so hooks.json
//! matchers, hook scripts and `effect_gate::map_tool_call` apply unchanged.

use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BASH_DEFAULT: Duration = Duration::from_secs(120);
const BASH_MAX: Duration = Duration::from_secs(600);

/// Read-only tools may re-run on resume; every other name, known or not,
/// counts as effect-capable. `map_tool_call` never makes a tool read-only:
/// it answers None for most Bash.
pub fn effect_capable(name: &str) -> bool {
    !matches!(name, "Read" | "Glob" | "Grep" | "Skill")
}

pub fn schemas() -> Value {
    let s = |name: &str, desc: &str, props: Value, req: &[&str]| {
        json!({"name": name, "description": desc,
            "input_schema": {"type": "object", "properties": props, "required": req}})
    };
    json!([
        s(
            "Read",
            "Read a file. Lines are numbered from 1.",
            json!({"file_path": {"type": "string"}, "offset": {"type": "integer"}, "limit": {"type": "integer"}}),
            &["file_path"]
        ),
        s(
            "Glob",
            "List files matching a glob pattern.",
            json!({"pattern": {"type": "string"}, "path": {"type": "string"}}),
            &["pattern"]
        ),
        s(
            "Grep",
            "Search file contents with a regex (ripgrep).",
            json!({"pattern": {"type": "string"}, "path": {"type": "string"}, "glob": {"type": "string"}}),
            &["pattern"]
        ),
        s(
            "Edit",
            "Replace one exact, unique string in a file.",
            json!({"file_path": {"type": "string"}, "old_string": {"type": "string"},
                "new_string": {"type": "string"}, "replace_all": {"type": "boolean"}}),
            &["file_path", "old_string", "new_string"]
        ),
        s(
            "Write",
            "Write a whole file.",
            json!({"file_path": {"type": "string"}, "content": {"type": "string"}}),
            &["file_path", "content"]
        ),
        s(
            "Bash",
            "Run a shell command in the session cwd.",
            json!({"command": {"type": "string"}, "timeout": {"type": "integer", "description": "ms, max 600000"}}),
            &["command"]
        ),
        s(
            "Skill",
            "Load a footnote skill's SKILL.md by name.",
            json!({"skill": {"type": "string"}, "args": {"type": "string"}}),
            &["skill"]
        ),
    ])
}

/// Stamp footnote's identity on a child. The supervisor already scrubbed
/// the spawner's identity from this process's env at launch.
pub fn child_env(cmd: &mut Command, fno_id: &str) {
    cmd.env("FNO_HARNESS_NAME", "footnote")
        .env("FNO_HARNESS_SESSION_ID", fno_id);
}

pub struct Ctx<'a> {
    pub cwd: &'a Path,
    pub fno_id: &'a str,
}

/// Run one tool. Returns (output, is_error). `Skill` is served by the loop.
pub fn run(name: &str, input: &Value, ctx: &Ctx) -> (String, bool) {
    let res = match name {
        "Read" => read(input, ctx),
        "Glob" => rg(ctx, &["--files", "-g", s(input, "pattern")], input),
        "Grep" => {
            let mut args = vec!["-n", "--", s(input, "pattern")];
            if let Some(g) = input["glob"].as_str() {
                args.splice(0..0, ["-g", g]);
            }
            rg(ctx, &args, input)
        }
        "Edit" => edit(input, ctx),
        "Write" => write(input, ctx),
        "Bash" => bash(input, ctx),
        other => Err(format!("unknown tool {other:?}")),
    };
    match res {
        Ok(out) => (out, false),
        Err(e) => (e, true),
    }
}

fn s<'a>(input: &'a Value, key: &str) -> &'a str {
    input[key].as_str().unwrap_or("")
}

fn path(input: &Value, ctx: &Ctx) -> Result<PathBuf, String> {
    let p = s(input, "file_path");
    if p.is_empty() {
        return Err("file_path is required".into());
    }
    Ok(ctx.cwd.join(p))
}

fn read(input: &Value, ctx: &Ctx) -> Result<String, String> {
    let p = path(input, ctx)?;
    let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = input["limit"].as_u64().unwrap_or(2000) as usize;
    Ok(text
        .lines()
        .enumerate()
        .skip(offset - 1)
        .take(limit)
        .map(|(i, l)| format!("{:>6}\t{l}\n", i + 1))
        .collect())
}

fn edit(input: &Value, ctx: &Ctx) -> Result<String, String> {
    let p = path(input, ctx)?;
    let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    let old = s(input, "old_string");
    if old.is_empty() {
        return Err("old_string is empty".into());
    }
    let count = text.matches(old).count();
    let all = input["replace_all"].as_bool() == Some(true);
    if count == 0 {
        return Err("old_string not found; the file is unchanged".into());
    }
    if count > 1 && !all {
        return Err(format!("old_string matches {count} times; give a unique string or replace_all. The file is unchanged"));
    }
    let new = s(input, "new_string");
    let out = if all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    std::fs::write(&p, out).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(format!("edited {} ({count} replacement(s))", p.display()))
}

fn write(input: &Value, ctx: &Ctx) -> Result<String, String> {
    let p = path(input, ctx)?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = s(input, "content");
    std::fs::write(&p, content).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(format!("wrote {} ({} bytes)", p.display(), content.len()))
}

fn rg(ctx: &Ctx, args: &[&str], input: &Value) -> Result<String, String> {
    let mut cmd = Command::new("rg");
    cmd.args(args);
    if let Some(p) = input["path"].as_str() {
        cmd.arg(p);
    }
    child_env(&mut cmd, ctx.fno_id);
    let out = cmd
        .current_dir(ctx.cwd)
        .output()
        .map_err(|e| format!("rg: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Some(1) => Ok("no matches".into()),
        _ => Err(String::from_utf8_lossy(&out.stderr).into_owned()),
    }
}

fn bash(input: &Value, ctx: &Ctx) -> Result<String, String> {
    let command = s(input, "command");
    let limit = input["timeout"]
        .as_u64()
        .map(Duration::from_millis)
        .unwrap_or(BASH_DEFAULT)
        .min(BASH_MAX);
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(command).current_dir(ctx.cwd);
    child_env(&mut cmd, ctx.fno_id);
    let (code, stdout, stderr) = run_bounded(cmd, None, limit)?;
    let out = stdout + &stderr;
    match code {
        Some(0) => Ok(out),
        Some(c) => Err(format!("{out}\n[exit {c}]")),
        None => Err(format!("{out}\n[killed: timeout or interrupt]")),
    }
}

/// Run a child with optional stdin, killed at
/// `limit` or on an interrupt. Returns (exit code, stdout, stderr); None = killed.
/// The child leads its own process group: a background process it leaves
/// holding the pipes is killed with the group, or the reads never end.
pub fn run_bounded(
    mut cmd: Command,
    stdin: Option<&str>,
    limit: Duration,
) -> Result<(Option<i32>, String, String), String> {
    use std::os::unix::process::CommandExt;
    let mut child = cmd
        .process_group(0)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let text = text.to_string();
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = pipe.write_all(text.as_bytes());
        });
    }
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|mut p| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = p.read_to_end(&mut buf);
            buf
        })
    })
    .collect();
    let pgid = child.id() as libc::pid_t;
    let kill_group = || unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    };
    let start = Instant::now();
    let code = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status.code();
        }
        if start.elapsed() > limit || crate::interrupted() {
            kill_group();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let grace = Instant::now();
    while !readers.iter().all(|r| r.is_finished()) {
        if grace.elapsed() > Duration::from_secs(1) {
            kill_group();
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut outs = readers
        .into_iter()
        .map(|r| String::from_utf8_lossy(&r.join().unwrap_or_default()).into_owned());
    let stdout = outs.next().unwrap_or_default();
    Ok((code, stdout, outs.next().unwrap_or_default()))
}
