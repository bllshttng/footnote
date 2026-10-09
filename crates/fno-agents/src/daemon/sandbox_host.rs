//! The sandbox host seam: how a worker's substrate is isolated from the host,
//! and what it may reach. Split out of daemon.rs (shrink-only) so sandbox-side
//! additions land here, never back in the daemon file.
//!
//! Ruling Q7 order is shared-state protection first: a `devcontainer` worker
//! gets ONLY its worktree and the daemon sockets bind-mounted, never the
//! agents home, so registry and claims change through daemon ops, never
//! direct file writes. `none` is today: no isolation, direct state access.
//! The remote-box spelling is the same plan with a forwarded socket: the env
//! pin (`FNO_SUPERVISOR_SOCKET`) points at the forward, not a path.

use super::*;

use std::collections::BTreeMap;

/// The carrier name a sandboxed lane declares in
/// `[harness.<harness>.state_root_grant]`: the lane reaches the state root
/// through the daemon socket, so it needs no host state-root grant. The
/// spawn gate probes the socket before believing the declaration.
pub const SOCKET_CARRIER: &str = "socket";

/// Where the mounted state doors live inside the container. The launch env
/// pins (`FNO_SUPERVISOR_SOCKET`, `FNO_STATE_DIR`) all resolve under it, so
/// the container needs no `~/.fno` at all.
pub const CONTAINER_STATE_DIR: &str = "/fno-state";

/// One bind-mount of the launch plan.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BindMount {
    pub host: PathBuf,
    pub container: PathBuf,
    pub readonly: bool,
}

/// What one sandboxed spawn mounts and exports. `none` produces the empty
/// plan: the spawn command runs unchanged.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SandboxLaunch {
    pub provider: &'static str,
    pub mounts: Vec<BindMount>,
    pub env: BTreeMap<String, String>,
}

/// Which isolation a spawn uses. `none` is today; `devcontainer` isolates
/// the worker behind the bind-mount plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxProvider {
    None,
    Devcontainer,
}

impl SandboxProvider {
    /// Resolve `[sandbox] provider`. Absent reads `none` (today). An unknown
    /// spelling is an error, never a silent downgrade to `none`: the operator
    /// configured isolation and must not believe workers are sandboxed when
    /// they are not.
    pub fn from_config(cwd: &Path) -> Result<Self, String> {
        match crate::agents_config::config_lookup(cwd, &["sandbox", "provider"]) {
            None => Ok(Self::None),
            Some(v) => match v.as_str().map(str::trim) {
                Some("none") => Ok(Self::None),
                Some("devcontainer") => Ok(Self::Devcontainer),
                _ => Err(format!(
                    "[sandbox] provider must be \"none\" or \"devcontainer\", got {}",
                    v.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("{v:?}"))
                )),
            },
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Devcontainer => "devcontainer",
        }
    }
}

/// The launch plan for one provider: the worktree (read-write, the worker's
/// product) and the supervisor socket (read-write; connecting to a unix
/// socket needs write access on some filesystems). The container paths are
/// fixed, so a launch recipe is reproducible from the plan alone.
pub fn launch_for(
    provider: SandboxProvider,
    worktree: &Path,
    supervisor_sock: &Path,
) -> SandboxLaunch {
    match provider {
        SandboxProvider::None => SandboxLaunch {
            provider: SandboxProvider::None.as_str(),
            mounts: Vec::new(),
            env: BTreeMap::new(),
        },
        SandboxProvider::Devcontainer => {
            let sock_container = Path::new(CONTAINER_STATE_DIR)
                .join(supervisor_sock.file_name().unwrap_or_default());
            let mut env = BTreeMap::new();
            env.insert(
                "FNO_SUPERVISOR_SOCKET".to_string(),
                sock_container.to_string_lossy().into_owned(),
            );
            env.insert("FNO_STATE_DIR".to_string(), CONTAINER_STATE_DIR.to_string());
            SandboxLaunch {
                provider: SandboxProvider::Devcontainer.as_str(),
                mounts: vec![
                    BindMount {
                        host: worktree.to_path_buf(),
                        container: PathBuf::from("/workspace"),
                        readonly: false,
                    },
                    BindMount {
                        host: supervisor_sock.to_path_buf(),
                        container: sock_container,
                        readonly: false,
                    },
                ],
                env,
            }
        }
    }
}

/// The `docker run`-shaped mount arguments for a plan: one flag per mount,
/// host and container path verbatim. A caller that launches through
/// `devcontainer` instead of raw docker still consumes the same mount list.
pub fn docker_mount_args(launch: &SandboxLaunch) -> Vec<String> {
    launch
        .mounts
        .iter()
        .map(|m| {
            format!(
                "--mount type=bind,src={},dst={}{}",
                m.host.display(),
                m.container.display(),
                if m.readonly { ",readonly" } else { "" }
            )
        })
        .collect()
}

/// Whether the supervisor socket at `path` answers a connect. The gate's
/// `socket` carrier check: a lane that declares the carrier must find a
/// listener, or the declaration is hiding a worker that can claim, mail, or
/// spawn nothing - the exact mute-worker harm R3 exists for.
///
/// The connect is bounded at 500ms: `UnixStream::connect` has no
/// connect_timeout, and a wedged listener (full backlog) would hang every
/// spawn whose carrier is `socket`. The probe rides one detached thread; a
/// timed-out probe leaves that thread blocked in connect (bounded by the
/// kernel backlog, not by us), while the caller moves on inside 500ms.
pub fn supervisor_sock_reachable(path: &Path) -> Result<(), String> {
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let p = path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(UnixStream::connect(&p).map(|_| ()));
    });
    match rx.recv_timeout(Duration::from_millis(500)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("{}: {e}", path.display())),
        Err(_) => Err(format!(
            "{}: connect timed out after 500ms (listener wedged?)",
            path.display()
        )),
    }
}

/// The `socket` carrier leg of the spawn gate's stance check: the only
/// stance that is verified against the world instead of taken as a
/// declaration. Any other stance (or no stance) passes; the missing-stance
/// refusal lives in the gate. `sock` absent means no path to probe, which
/// passes - the same fail-open the empty-roots leg takes.
pub(crate) fn socket_carrier_ok(stance: Option<&str>, sock: Option<&Path>) -> Result<(), String> {
    if stance != Some(SOCKET_CARRIER) {
        return Ok(());
    }
    match sock {
        Some(sock) => supervisor_sock_reachable(sock),
        None => Ok(()),
    }
}

/// `agent.sandbox-plan` - answer the launch plan for one worktree: the
/// configured provider, its mounts, and the env the container-side `fno`
/// reads. The spawn lane (or the operator composing a devcontainer.json)
/// consumes the JSON; the daemon owns the socket paths, so the plan is the
/// one authority a container needs.
pub(crate) fn handle_sandbox_plan(ctx: &Ctx, req: &Request) -> Response {
    let worktree = match req.params.get("worktree").and_then(|v| v.as_str()) {
        Some(w) => PathBuf::from(w),
        None => {
            return Response::err(req.id, ErrorCode::InvalidParams, "missing `worktree`");
        }
    };
    let provider = match SandboxProvider::from_config(&worktree) {
        Ok(p) => p,
        Err(e) => return Response::err(req.id, ErrorCode::InvalidParams, e),
    };
    let launch = launch_for(provider, &worktree, &ctx.home.supervisor_sock());
    Response::ok(
        req.id,
        serde_json::to_value(&launch).unwrap_or_else(|_| json!({"provider": "none"})),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sandbox-host-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn provider_parse_reads_the_config_key_and_refuses_unknown() {
        let dir = tempdir("parse");
        std::fs::write(
            dir.join("config.toml"),
            "[sandbox]\nprovider = \"devcontainer\"\n",
        )
        .unwrap();
        assert_eq!(
            SandboxProvider::from_config(&dir).unwrap(),
            SandboxProvider::Devcontainer
        );
        std::fs::write(dir.join("config.toml"), "[sandbox]\nprovider = \"pod\"\n").unwrap();
        let err = SandboxProvider::from_config(&dir).unwrap_err();
        assert!(err.contains("devcontainer"), "{err}");
        // Absent key reads none: today's behavior is the default.
        std::fs::write(dir.join("config.toml"), "").unwrap();
        assert_eq!(
            SandboxProvider::from_config(&dir).unwrap(),
            SandboxProvider::None
        );
    }

    #[test]
    fn none_provider_launches_unchanged() {
        let launch = launch_for(
            SandboxProvider::None,
            Path::new("/repo/wt"),
            Path::new("/h/s.sock"),
        );
        assert_eq!(launch.provider, "none");
        assert!(launch.mounts.is_empty());
        assert!(launch.env.is_empty());
        assert!(docker_mount_args(&launch).is_empty());
    }

    #[test]
    fn devcontainer_plan_mounts_worktree_and_socket_pins_env() {
        let launch = launch_for(
            SandboxProvider::Devcontainer,
            Path::new("/repo/wt"),
            Path::new("/home/u/.fno/agents/supervisor.sock"),
        );
        assert_eq!(launch.provider, "devcontainer");
        assert_eq!(launch.mounts.len(), 2);
        assert_eq!(launch.mounts[0].host, Path::new("/repo/wt"));
        assert_eq!(launch.mounts[0].container, Path::new("/workspace"));
        assert!(!launch.mounts[0].readonly);
        assert_eq!(
            launch.mounts[1].container,
            Path::new("/fno-state/supervisor.sock")
        );
        assert_eq!(
            launch.env.get("FNO_SUPERVISOR_SOCKET").map(String::as_str),
            Some("/fno-state/supervisor.sock")
        );
        assert_eq!(
            launch.env.get("FNO_STATE_DIR").map(String::as_str),
            Some("/fno-state")
        );
        let args = docker_mount_args(&launch);
        assert_eq!(args.len(), 2);
        assert!(
            args[0].starts_with("--mount type=bind,src=/repo/wt,dst=/workspace"),
            "{}",
            args[0]
        );
    }

    #[test]
    fn reachability_answers_a_live_listener_and_names_a_dead_path() {
        let dir = tempdir("probe");
        let sock = dir.join("supervisor.sock");
        assert!(supervisor_sock_reachable(&sock).is_err());
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(supervisor_sock_reachable(&sock).is_ok());
        drop(listener);
    }

    /// The `socket` carrier is verified, never taken on faith: a live
    /// listener passes the stance leg; a dead path refuses naming the
    /// socket; a stance that is not the carrier is never probed; no path to
    /// probe fails open, the same leg the empty-roots case takes (AC8).
    #[test]
    fn socket_carrier_is_probed_not_declared() {
        let dir = tempdir("carrier");
        let sock = dir.join("supervisor.sock");
        let err = socket_carrier_ok(Some("socket"), Some(&sock))
            .err()
            .expect("dead socket refuses");
        assert!(
            err.starts_with(&sock.display().to_string()),
            "the refusal names the socket path: {err}"
        );
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(socket_carrier_ok(Some("socket"), Some(&sock)).is_ok());
        drop(listener);
        let listener2 = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(socket_carrier_ok(Some("unsandboxed"), Some(&sock)).is_ok());
        assert!(socket_carrier_ok(None, Some(&sock)).is_ok());
        assert!(socket_carrier_ok(Some("socket"), None).is_ok());
        drop(listener2);
    }

    #[test]
    fn sandbox_plan_op_answers_the_launch_json() {
        // The config anchor is the worktree itself: `config_candidates` reads
        // `<cwd>/.fno/config.toml` first, so the op test stages the provider
        // there instead of racing a process env pin.
        let worktree = tempdir("op");
        std::fs::create_dir_all(worktree.join(".fno")).unwrap();
        let home = crate::paths::AgentsHome::at(tempdir("op-home"));
        let ctx = super::super::tests::test_ctx(home, PathBuf::from("fno-agents-worker"));
        let req = Request::new(
            1,
            "agent.sandbox-plan",
            json!({ "worktree": worktree.to_string_lossy() }),
        );
        let res = handle_sandbox_plan(&ctx, &req);
        let body = res.result().expect("ok");
        assert_eq!(body["provider"], "none");
        // Same worktree, config naming devcontainer: the plan isolates.
        std::fs::write(
            worktree.join(".fno/config.toml"),
            "[sandbox]\nprovider = \"devcontainer\"\n",
        )
        .unwrap();
        let res = handle_sandbox_plan(&ctx, &req);
        let body = res.result().expect("ok");
        assert_eq!(body["provider"], "devcontainer");
        assert_eq!(body["mounts"].as_array().map(Vec::len), Some(2));
    }

    #[tokio::test]
    async fn pinned_lane_never_forks_a_daemon_and_fails_naming_the_pin() {
        let home = crate::paths::AgentsHome::at(tempdir("pin"));
        let req = Request::new(1, "agent.status", json!({}));
        // A stale socket file at the pin path answers nobody.
        let pin = tempdir("pin-sock").join("supervisor.sock");
        std::fs::write(&pin, b"").unwrap();
        // SAFETY: test-process env; only the new pin path reads this var and
        // the test restores the unpinned world before it ends.
        std::env::set_var("FNO_SUPERVISOR_SOCKET", &pin);
        let err = crate::client::call(
            &home,
            Path::new("/nonexistent/fno-agents"), // a fork would fail here, not with DaemonNotRunning
            &req,
        )
        .await
        .unwrap_err();
        std::env::remove_var("FNO_SUPERVISOR_SOCKET");
        assert!(
            matches!(err, crate::client::ClientError::DaemonNotRunning),
            "got {err:?}"
        );
        assert_eq!(
            crate::client::client_sock(&home),
            home.supervisor_sock(),
            "the pin removed, the home path is served again"
        );
    }
}
