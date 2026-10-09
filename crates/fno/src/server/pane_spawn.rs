//! How a pane spawn leaves the core loop. Phase one runs on the loop: the
//! pane id is reserved (one bounded store mutation) and the gesture's tail
//! is parked under that id. Phase two runs on the blocking pool: the keeper
//! launch, its Identify handshake (bounded at 3 s), and the inline fallback.
//! The outcome comes back as [`CoreMsg::PaneSpawnReady`]; the loop registers
//! the pane and runs the parked tail, so every placement decision still sees
//! live state. Unit fixtures drive no loop, so under `cfg(test)` the job and
//! its tail run inline and every synchronous assert keeps holding.

use super::*;

/// The gesture's continuation: it receives the registered pane id (or the
/// spawn's refusal) on the core loop, exactly where the synchronous call
/// used to return.
pub(super) type SpawnTail = Box<dyn FnOnce(&mut Core, Result<u64, String>) -> Flow + Send>;

/// One spawn's inputs, everything the blocking half needs and nothing it
/// would have to read from `Core`.
pub(super) struct SpawnOrder {
    pub(super) argv: Vec<String>,
    pub(super) rows: u16,
    pub(super) cols: u16,
    pub(super) cwd: String,
    pub(super) permit: crate::process_admission::AdmissionPermit,
    /// `true` routes through a `fno-agents-worker --pane` keeper; the inline
    /// pty is then only the named fallback for a keeper that cannot start.
    pub(super) keeper: bool,
}

/// The production road is keeper-hosted; unit fixtures keep the inline pty
/// (short-lived `/bin/cat` children can exit before a keeper answers).
pub(super) const KEEPER_ROAD: bool = cfg!(not(test));

struct SpawnJob {
    id: u64,
    order: SpawnOrder,
    session: String,
    out_tx: mpsc::Sender<(u64, PaneChunk)>,
    exit_tx: mpsc::Sender<u64>,
}

/// A spawned child, not yet registered. `ring` is the keeper's handshake
/// replay; `fell_back` names the keeper failure that sent the pane inline.
pub(crate) struct SpawnedPane {
    shell: PtyShell,
    ring: Vec<u8>,
    fell_back: Option<String>,
}

/// A spawn in flight: the tail to run when it lands, plus the output its
/// reader thread produced before the pane was registered (the reader starts
/// inside the blocking half, so that output can win the race to the loop).
struct PendingSpawn {
    argv: Vec<String>,
    rows: u16,
    cols: u16,
    cwd: String,
    client: Option<u64>,
    tail: SpawnTail,
    early_output: Vec<u8>,
}

/// The in-flight state the async boundary needs. Gates are REQUIRED: once a
/// spawn no longer completes inside one loop turn, a second click lands
/// while the first is still in the handshake and would spawn twice.
#[derive(Default)]
pub(super) struct SpawnFlight {
    pending: HashMap<u64, PendingSpawn>,
    /// Attach ids and portal row keys whose viewer is still spawning.
    rows: HashSet<String>,
    /// Portal indices whose seat is still spawning.
    portals: HashSet<u8>,
}

impl SpawnJob {
    /// The blocking half: keeper launch + handshake, else the inline pty.
    fn run(self) -> Result<SpawnedPane, String> {
        let SpawnJob {
            id,
            order,
            session,
            out_tx,
            exit_tx,
        } = self;
        let SpawnOrder {
            argv,
            rows,
            cols,
            cwd,
            permit,
            keeper,
        } = order;
        let dir = Some(std::path::Path::new(&cwd)).filter(|_| !cwd.is_empty());
        if !keeper {
            return PtyShell::spawn_cmd_with_permit(
                &argv, rows, cols, dir, &session, id, out_tx, exit_tx, permit,
            )
            .map(|shell| SpawnedPane {
                shell,
                ring: Vec::new(),
                fell_back: None,
            })
            .map_err(|e| e.to_string());
        }
        match PtyShell::spawn_cmd_keeper_with_permit(
            &keeper_worker_bin(),
            &argv,
            rows,
            cols,
            dir,
            &session,
            id,
            out_tx.clone(),
            exit_tx.clone(),
            permit,
        ) {
            Ok((shell, ring)) => Ok(SpawnedPane {
                shell,
                ring,
                fell_back: None,
            }),
            // A keeper that cannot start (missing binary, failed handshake,
            // held seat) must not cost the pane: fall back to the inline pty
            // and say so; the entry is marked `unkept` at registration.
            Err(keeper_err) => {
                let fallback_permit =
                    crate::process_admission::admit_fleet().map_err(|e| e.to_string())?;
                let shell = PtyShell::spawn_cmd_with_permit(
                    &argv,
                    rows,
                    cols,
                    dir,
                    &session,
                    id,
                    out_tx,
                    exit_tx,
                    fallback_permit,
                )
                .map_err(|e| e.to_string())?;
                Ok(SpawnedPane {
                    shell,
                    ring: Vec::new(),
                    fell_back: Some(keeper_err.to_string()),
                })
            }
        }
    }
}

impl Core {
    /// Spawn an explicit `argv` as a pane (the `pane run` / agents-spawn path)
    /// - no shell candidate fallback: an unspawnable argv is the caller's
    /// error, surfaced verbatim. Same atomic ordering as [`Core::spawn_pane`]
    /// (PTY first, model second), so a spawn failure mutates nothing.
    pub(super) fn spawn_pane_cmd(
        &mut self,
        argv: &[String],
        rows: u16,
        cols: u16,
        cwd: &str,
    ) -> Result<u64, String> {
        // Before admission: the ceiling probe must own the wall. At one free
        // descriptor the admission census EMFILEs first and the operator
        // would read a measurement failure where the truth is the ceiling.
        if let Some(err) = crate::pty::fd_ceiling_refusal() {
            return Err(err.to_string());
        }
        let permit = crate::process_admission::admit_fleet().map_err(|e| e.to_string())?;
        self.spawn_pane_cmd_with_permit(argv, rows, cols, cwd, permit)
    }

    pub(super) fn spawn_pane_cmd_with_permit(
        &mut self,
        argv: &[String],
        rows: u16,
        cols: u16,
        cwd: &str,
        permit: crate::process_admission::AdmissionPermit,
    ) -> Result<u64, String> {
        self.spawn_pane_shell_with_permit(argv, rows, cols, cwd, permit, KEEPER_ROAD)
    }

    /// The synchronous road, for the callers that still spawn inside one
    /// loop turn (restore, resume, recruit, placeholders). `keeper = true`
    /// routes a pane through a keeper that owns the pty master
    /// out-of-process, so the pane child outlives this server and a fresh
    /// server re-adopts it; the inline pty is the named fallback for a
    /// keeper that cannot start, and the pane entry is then marked `unkept`.
    pub(super) fn spawn_pane_shell_with_permit(
        &mut self,
        argv: &[String],
        rows: u16,
        cols: u16,
        cwd: &str,
        permit: crate::process_admission::AdmissionPermit,
        keeper: bool,
    ) -> Result<u64, String> {
        let order = SpawnOrder {
            argv: argv.to_vec(),
            rows,
            cols,
            cwd: cwd.to_string(),
            permit,
            keeper,
        };
        let job = self.spawn_job(order)?;
        let id = job.id;
        let spawned = job.run()?;
        self.register_spawned(id, argv, rows, cols, cwd, spawned, &[])
    }

    /// Phase one, on the loop: refuse an empty argv and reserve the id.
    fn spawn_job(&mut self, order: SpawnOrder) -> Result<SpawnJob, String> {
        if order.argv.is_empty() {
            return Err("pane run needs a command (empty argv)".into());
        }
        Ok(SpawnJob {
            id: self.reserve_pane_id()?,
            order,
            session: self.session_name.clone(),
            out_tx: self.out_tx.clone(),
            exit_tx: self.exit_tx.clone(),
        })
    }

    /// Spawn a pane OFF the core loop and run `tail` on the loop once it is
    /// registered. `client` names the gesture's requester, so a parked
    /// control-door reply waits for this spawn before it answers. The caller
    /// returns right after this call; `tail` owns everything that used to
    /// follow the synchronous spawn.
    pub(super) fn spawn_then(
        &mut self,
        order: SpawnOrder,
        client: Option<u64>,
        tail: impl FnOnce(&mut Core, Result<u64, String>) -> Flow + Send + 'static,
    ) -> Flow {
        let (argv, rows, cols, cwd) = (
            order.argv.clone(),
            order.rows,
            order.cols,
            order.cwd.clone(),
        );
        let job = match self.spawn_job(order) {
            Ok(job) => job,
            Err(e) => return tail(self, Err(e)),
        };
        let id = job.id;
        self.spawn_flight.pending.insert(
            id,
            PendingSpawn {
                argv,
                rows,
                cols,
                cwd,
                client,
                tail: Box::new(tail),
                early_output: Vec::new(),
            },
        );
        if cfg!(test) {
            let outcome = job.run();
            return self.pane_spawn_ready(id, outcome);
        }
        let core_tx = self.self_tx.clone();
        tokio::task::spawn_blocking(move || {
            // A panicking job must still land: the parked tail is what
            // releases this gesture's row and portal gates.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run()))
                .unwrap_or_else(|_| Err(format!("pane {id} spawn panicked")));
            // A closed loop (server shutting down) drops the outcome: a
            // keeper-hosted child survives for the next server to adopt.
            let _ = core_tx.blocking_send(CoreMsg::PaneSpawnReady {
                id,
                outcome: Box::new(outcome),
            });
        });
        Flow::Continue
    }

    /// Phase two's landing, on the loop: register the pane, replay the
    /// output it produced in flight, then run the gesture's tail.
    pub(super) fn pane_spawn_ready(
        &mut self,
        id: u64,
        outcome: Result<SpawnedPane, String>,
    ) -> Flow {
        let Some(pending) = self.spawn_flight.pending.remove(&id) else {
            if let Ok(spawned) = outcome {
                spawned.shell.kill();
            }
            return Flow::Continue;
        };
        let PendingSpawn {
            argv,
            rows,
            cols,
            cwd,
            client,
            tail,
            early_output,
        } = pending;
        let result = outcome.and_then(|spawned| {
            self.register_spawned(id, &argv, rows, cols, &cwd, spawned, &early_output)
        });
        let flow = tail(self, result);
        if let Some(client) = client {
            self.finish_parked_reply_after_spawn(client);
        }
        flow
    }

    /// Output from a pane whose spawn is still in flight. True when the
    /// bytes were kept for replay at registration.
    pub(super) fn buffer_in_flight_output(&mut self, pid: u64, bytes: &[u8]) -> bool {
        match self.spawn_flight.pending.get_mut(&pid) {
            Some(pending) => {
                pending.early_output.extend_from_slice(bytes);
                true
            }
            None => false,
        }
    }

    /// Whether a spawn requested by `client` has not landed yet.
    pub(super) fn spawn_in_flight_for(&self, client: u64) -> bool {
        self.spawn_flight
            .pending
            .values()
            .any(|p| p.client == Some(client))
    }

    /// Claim a row key (attach id or portal row) for one in-flight viewer
    /// spawn. False when another spawn for that row has not landed yet.
    pub(super) fn claim_row_spawn(&mut self, key: &str) -> bool {
        self.spawn_flight.rows.insert(key.to_string())
    }

    pub(super) fn row_spawning(&self, key: &str) -> bool {
        self.spawn_flight.rows.contains(key)
    }

    pub(super) fn release_row_spawn(&mut self, key: &str) {
        self.spawn_flight.rows.remove(key);
    }

    pub(super) fn claim_portal_spawn(&mut self, idx: u8) -> bool {
        self.spawn_flight.portals.insert(idx)
    }

    pub(super) fn release_portal_spawn(&mut self, idx: u8) {
        self.spawn_flight.portals.remove(&idx);
    }

    pub(super) fn portal_spawning(&self, idx: u8) -> bool {
        self.spawn_flight.portals.contains(&idx)
    }

    /// The control door parks its reply while a reach it drove is still
    /// spawning; answer it once the last such spawn landed.
    fn finish_parked_reply_after_spawn(&mut self, client: u64) {
        if self.spawn_in_flight_for(client) {
            return;
        }
        match self.pending_thread_reply.take() {
            Some(parked) if parked.client == client => self.finish_pending_thread_reply(parked),
            other => self.pending_thread_reply = other,
        }
    }

    /// Answer a parked control-door reply now, unless the reach it drove is
    /// still spawning: then it stays parked and the spawn's landing answers.
    pub(super) fn finish_or_repark_thread_reply(
        &mut self,
        parked: super::portal_reach::PendingThreadReply,
    ) {
        if self.spawn_in_flight_for(parked.client) {
            self.pending_thread_reply = Some(parked);
        } else {
            self.finish_pending_thread_reply(parked);
        }
    }

    /// Record a spawned child as a pane: provenance from its argv, the
    /// keeper ring and any in-flight output fed into the fresh VT, and the
    /// `unkept` mark plus notice when the keeper fell back inline.
    #[allow(clippy::too_many_arguments)]
    fn register_spawned(
        &mut self,
        id: u64,
        argv: &[String],
        rows: u16,
        cols: u16,
        cwd: &str,
        spawned: SpawnedPane,
        early_output: &[u8],
    ) -> Result<u64, String> {
        let SpawnedPane {
            shell,
            ring,
            fell_back,
        } = spawned;
        self.register_pane(
            id,
            shell,
            rows,
            cols,
            node_from_argv(argv),
            agent_self_from_argv(argv),
            cwd.to_string(),
            cmd_from_argv(argv),
            account_from_argv(argv),
            resume_target_from_argv(argv),
            refused_worker_from_argv(argv),
            portal_hold_from_argv(argv),
            transient_view_from_argv(argv),
        )?;
        if let Some(keeper_err) = fell_back {
            if let Some(entry) = self.panes.get_mut(&id) {
                entry.unkept = true;
            }
            // `notice_all` only reaches attached clients, so a keeper failure
            // with nobody attached (the common case at spawn) would never
            // reach the server's own log - the one place a headless caller
            // (a stress script, CI) can see why a pane came up unkept.
            eprintln!(
                "fno mux: keeper unavailable for pane {id} ({keeper_err}); running unkept inline"
            );
            self.notice_all(format!(
                "keeper unavailable for pane {id} ({keeper_err}); running unkept inline"
            ));
        }
        // The keeper's handshake replay carries everything the child printed
        // before the reader thread existed, and `early_output` what the
        // reader sent before this registration; the VT only now exists.
        if let Some(entry) = self.panes.get_mut(&id) {
            for bytes in [ring.as_slice(), early_output] {
                if !bytes.is_empty() {
                    entry.vt.feed(bytes);
                }
            }
        }
        Ok(id)
    }
}
