//! The cross-door property (the x-5a62 PR-shape ruling, 2026-09-08): every
//! removal door leaves a row absent from ALL THREE stores, still absent
//! after a restart.
//!
//! Twenty earlier PRs this tree already tested one door at a time. The
//! doors this test drives, in one fleet:
//!
//! 1. the registry sweep (`fno-agents reap --apply`): the only automatic
//!    registry drain, re-armed by the provenance cascade;
//! 2. the roster sweep (`fno-agents roster-reap --apply`): claude rows no
//!    fno row names;
//! 3. the squad prune (`fno mux workspace prune`): a member keyed by a
//!    reaped row's session id drains through the receipts leg, and a
//!    name-only member the cascade resolves drains through node-route.
//!
//! The restart half is load-bearing: every door runs in a fresh process,
//! and the full pass re-runs every door and must change nothing. A reap
//! that clears a view and not the store reads green here otherwise.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

const NODE: &str = "x-prop";
const ROW1: &str = "target-x-prop-worker";
// A name-only squad member has no session id, so the cascade's transcript
// source can never answer for it: the name must resolve the done node.
const ROW2_NAME: &str = "target-x-prop-audit";
const ROW3: &str = "target-x-prop-alt";
/// The converse gate: a row whose node is done but whose transcript is
/// FRESH is live, and every door must leave it in all three stores. A
/// liveness hold that fails in the deletion direction would reap the
/// operator's own row here.
const ROW4: &str = "target-x-prop-live";
const U1: &str = "11111111-2222-4333-8444-555555555555";
const U2: &str = "22222222-3333-7444-8888-999999999999";
const U3: &str = "33333333-4444-5666-8777-888888888888";
const U4: &str = "44444444-5555-6666-8777-999999999999";
const S1: &str = "aaa1b2c3";
const S2: &str = "bbb2c3d4";
const S3: &str = "ccc3d4e5";
const S4: &str = "ddd4e5f6";
/// The isolated account the third row launched under. Its `config_dir` is
/// the fleet's alt claude root; the shim lists different rows per root.
const ALT_ACCOUNT: &str = "alt";

/// Resolve the sibling fno-agents binary. The two crates share a repo but
/// not a cargo target dir; the fno-agents test leg (preflight runs it
/// first) builds it there.
fn fno_agents_bin() -> PathBuf {
    if let Ok(bin) = std::env::var("FNO_AGENTS_BIN") {
        if !bin.is_empty() && Path::new(&bin).exists() {
            return PathBuf::from(bin);
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .unwrap()
        .join("fno-agents")
        .join("target")
        .join("debug")
        .join("fno-agents")
}

/// The seeded registry must carry the schema version the binary under test
/// stamps. A hardcoded number here goes stale on every schema bump, and the
/// source-run binary then refuses to raise the file (SourceAheadSchemaBump),
/// so the reap retires nothing. Read the single owner instead.
fn registry_schema_version() -> u32 {
    let toml = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("fno-agents")
        .join("src")
        .join("registry_schema.toml");
    let text = std::fs::read_to_string(toml).unwrap();
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with("version"))
        .unwrap();
    line.split('=').nth(1).unwrap().trim().parse().unwrap()
}

struct Fleet {
    dir: PathBuf,
}

impl Drop for Fleet {
    // The fleet dir is a fake HOME; a panic must reap it too, or the leaked
    // scratch outlives the run that made it (x-7ca7 wave 2).
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

impl Fleet {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("cross-door-{tag}-{}", std::process::id()));
        let home = dir.join("agents");
        std::fs::create_dir_all(&home).unwrap();
        Fleet { dir }
    }

    fn agents_home(&self) -> PathBuf {
        self.dir.join("agents")
    }

    fn home(&self) -> PathBuf {
        self.dir.join("home")
    }

    /// Seed the fleet: graph, registry, squads, transcripts.
    fn seed(&self) {
        std::fs::create_dir_all(self.dir.join("work")).unwrap();
        let agents = self.dir.join("agents");
        std::fs::write(
            self.dir.join("graph.json"),
            format!(
                r#"{{"entries": [
                {{"id": "{NODE}", "type": "feature", "status": "done",
                  "merge_status": "merged",
                  "sessions": [
                    {{"session_id": "{U1}", "phase": "review"}},
                    {{"session_id": "{U3}", "phase": "review"}}
                  ]}}
            ]}}"#
            ),
        )
        .unwrap();
        std::fs::write(
            agents.join("registry.json"),
            format!(
                r#"{{"schema_version": {}, "agents": [
                {{"name": "{ROW1}",
                "harness": "claude", "harness_session_id": "{U1}",
                "short_id": "{S1}", "origin": "spawn", "status": "exited",
                "cwd": "{}", "created_at": "2026-09-01T00:00:00Z"}},
                {{"name": "{ROW3}",
                "harness": "claude", "harness_session_id": "{U3}",
                "short_id": "{S3}", "origin": "spawn", "status": "exited",
                "launch_account": "{ALT_ACCOUNT}",
                "cwd": "{}", "created_at": "2026-09-01T00:00:00Z"}},
                {{"name": "{ROW4}",
                "harness": "claude", "harness_session_id": "{U4}",
                "short_id": "{S4}", "origin": "spawn", "status": "exited",
                "cwd": "{}", "created_at": "2026-09-01T00:00:00Z"}}]}}"#,
                registry_schema_version(),
                self.dir.join("work").display(),
                self.dir.join("work").display(),
                self.dir.join("work").display(),
            ),
        )
        .unwrap();
        // The config-declared account root the blocking finding names: an
        // isolated claude account whose agent list and transcripts live
        // outside the ambient root. The roster reader, the transcript index
        // and the `rm` routing must all see it.
        std::fs::write(
            agents.join("config.toml"),
            format!(
                r#"[[accounts.records]]
id = "{ALT_ACCOUNT}"
config_dir = "{}"
"#,
                self.alt_root().display()
            ),
        )
        .unwrap();
        std::fs::write(
            agents.join("squads.json"),
            format!(
                r#"{{"version": 1, "squads": [{{"name": "", "key": "prop0feeddeadbeef",
                "origins": ["{WORK}"], "created_at": "2026-09-01T00:00:00Z",
                "members": [
                  {{"worker": "{ROW1}", "harness": "claude",
                   "harness_session_id": "{U1}"}},
                  {{"worker": "{ROW2_NAME}", "harness": "claude"}},
                  {{"worker": "{ROW3}", "harness": "claude",
                   "harness_session_id": "{U3}"}},
                  {{"worker": "{ROW4}", "harness": "claude",
                   "harness_session_id": "{U4}"}}
                ]}}]}}"#,
                WORK = self.dir.join("work").display()
            ),
        )
        .unwrap();
        self.quiet_transcript(U1);
        self.quiet_transcript_at(&self.alt_root(), U3);
        self.fresh_transcript(U4);
        // The daemon roster the reap's stop gate reads: the row is live
        // when the fleet starts, and `claude stop` (the shim) empties it.
        let daemon = self.home().join(".claude").join("daemon");
        std::fs::create_dir_all(&daemon).unwrap();
        std::fs::write(
            daemon.join("roster.json"),
            format!(
                r#"{{"proto":1,"workers":{{"{S1}":{{"sessionId":"{U1}","pid":4242}},"{S3}":{{"sessionId":"{U3}","pid":4243}}}}}}"#
            ),
        )
        .unwrap();
    }

    fn alt_root(&self) -> PathBuf {
        self.dir.join("altclaude")
    }

    /// Seed the roster-only row: a claude agent-list row with no fno
    /// registry row, whose transcript names its node.
    fn seed_roster_only_row(&self) {
        let proj = self.home().join(".claude").join("projects").join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let path = proj.join(format!("{U2}.jsonl"));
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"user\",\"message\":{{\"content\":\"dispatch to {NODE} 2 now\"}}}}\n"
            ),
        )
        .unwrap();
        old_mtime(&path);
    }

    fn quiet_transcript(&self, sid: &str) {
        self.quiet_transcript_at(&self.home().join(".claude"), sid);
    }

    /// A transcript written NOW: liveness the quiet gate must respect even
    /// when the resolved node reads done.
    fn fresh_transcript(&self, sid: &str) {
        let proj = self.home().join(".claude").join("projects").join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join(format!("{sid}.jsonl")), "{\"message\":{}}\n").unwrap();
    }

    fn quiet_transcript_at(&self, root: &Path, sid: &str) {
        let proj = root.join("projects").join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let path = proj.join(format!("{sid}.jsonl"));
        std::fs::write(&path, "{\"message\":{}}\n").unwrap();
        old_mtime(&path);
    }
}

fn old_mtime(path: &Path) {
    let old = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - 10_000;
    let f = std::fs::File::options().append(true).open(path).unwrap();
    f.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(old as u64))
        .unwrap();
}

fn write_executable(path: &Path, script: &str) {
    use std::io::Write;
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(script.as_bytes()).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The shimmed claude: two marker files, one per row. `claude agents` lists
/// the rows whose marker exists; `claude rm <id>` removes that row's
/// marker, so the "agent list" is exactly the store the doors drain.
fn write_claude_shim(fleet_dir: &Path, shim_dir: &Path) {
    let c1 = fleet_dir.join("claude-c1.json");
    let c2 = fleet_dir.join("claude-c2.json");
    let c3 = fleet_dir.join("claude-c3.json");
    let c4 = fleet_dir.join("claude-c4.json");
    let alt = fleet_dir.join("altclaude");
    let roster = fleet_dir
        .join("home")
        .join(".claude")
        .join("daemon")
        .join("roster.json");
    std::fs::write(
        &c1,
        format!(
            r#"{{"kind":"background","id":"{S1}","sessionId":"{U1}","name":"{ROW1}","cwd":"{WORK}","state":"done"}}"#,
            WORK = fleet_dir.join("work").display()
        ),
    )
    .unwrap();
    std::fs::write(
        &c2,
        format!(
            r#"{{"kind":"background","id":"{S2}","sessionId":"{U2}","name":"{ROW2_NAME}","state":"stopped"}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        &c3,
        format!(
            r#"{{"kind":"background","id":"{S3}","sessionId":"{U3}","name":"{ROW3}","cwd":"{WORK}","state":"done"}}"#,
            WORK = fleet_dir.join("work").display()
        ),
    )
    .unwrap();
    std::fs::write(
        &c4,
        format!(
            // The LIVE row must read live everywhere: a terminal roster
            // state now retires a row whatever its transcript recency, so
            // staging it `done` would make it dead by the harness's own
            // word.
            r#"{{"kind":"background","id":"{S4}","sessionId":"{U4}","name":"{ROW4}","cwd":"{WORK}","state":"working"}}"#,
            WORK = fleet_dir.join("work").display()
        ),
    )
    .unwrap();
    let script = format!(
        r#"#!/bin/sh
C1="{c1}"
C2="{c2}"
C3="{c3}"
C4="{c4}"
ALT="{alt}"
ROSTER="{roster}"
if [ "$1" = "agents" ]; then
  if [ "$CLAUDE_CONFIG_DIR" = "$ALT" ]; then
    if [ -f "$C3" ]; then printf '[%s]\n' "$(cat "$C3")"; else printf '[]\n'; fi
    exit 0
  fi
  rows=""
  [ -f "$C1" ] && rows="$rows$(cat "$C1"),"
  [ -f "$C2" ] && rows="$rows$(cat "$C2"),"
  [ -f "$C4" ] && rows="$rows$(cat "$C4"),"
  rows=$(printf '%s' "$rows" | /usr/bin/sed -e 's/,$//')
  if [ -z "$rows" ]; then
    printf '[]\n'
  else
    printf '[%s]\n' "$rows"
  fi
  exit 0
fi
if [ "$1" = "rm" ] && [ -n "$2" ]; then
  [ "$2" = "{S1}" ] && /bin/rm -f "$C1" && exit 0
  [ "$2" = "{S2}" ] && /bin/rm -f "$C2" && exit 0
  [ "$2" = "{S3}" ] && /bin/rm -f "$C3" && exit 0
  [ "$2" = "{S4}" ] && /bin/rm -f "$C4" && exit 0
  exit 2
fi
if [ "$1" = "stop" ] && [ -n "$2" ] && [ -f "$ROSTER" ]; then
  printf '{{"proto":1,"workers":{{}}}}' > "$ROSTER"
  exit 0
fi
exit 2
"#,
        c1 = c1.display(),
        c2 = c2.display(),
        c3 = c3.display(),
        c4 = c4.display(),
        alt = alt.display(),
        roster = roster.display(),
        S1 = S1,
        S2 = S2,
        S3 = S3,
        S4 = S4,
    );
    write_executable(&shim_dir.join("claude"), &script);
}

static BUILD: Once = Once::new();

#[test]
fn every_removal_door_leaves_the_row_absent_from_all_three_stores() {
    let fleet = Fleet::new("full");
    fleet.seed();
    fleet.seed_roster_only_row();
    let shim_dir = fleet.dir.join("shims");
    std::fs::create_dir_all(&shim_dir).unwrap();
    write_claude_shim(&fleet.dir, &shim_dir);
    // A `fno` shim: the prune's pane probe answers no panes, and the
    // reaper's truth probe answers the wire the Rust reader parses (a
    // keyed map for `--handles`, a bare payload for one handle): quiet
    // rows report old ages, the live row a young one. It must speak only
    // when addressed, so an unexpected call is loud.
    write_executable(
        &shim_dir.join("fno"),
        format!(
            r#"#!/bin/sh
if [ "$1" = "agents" ] && [ "$2" = "truth" ]; then
  if [ "$3" = "--handles" ]; then
    printf '{{'
    first=1
    for h in $(printf '%s' "$4" | /usr/bin/tr ',' ' '); do
      if [ "$h" = "{S4}" ]; then
        row='{{"state":"working","last_activity_age_s":2}}'
      else
        row='{{"state":"stalled","last_activity_age_s":100000}}'
      fi
      if [ "$first" -eq 1 ]; then first=0; else printf ','; fi
      printf '"%s":%s' "$h" "$row"
    done
    printf '}}\n'
  elif [ "$3" = "{S4}" ]; then
    printf '{{"state":"working","last_activity_age_s":2}}\n'
  else
    printf '{{"state":"stalled","last_activity_age_s":100000}}\n'
  fi
  exit 0
fi
if [ "$1" = "mux" ] && [ "$2" = "pane" ] && [ "$3" = "ls" ]; then
  printf '[]\n'
  exit 0
fi
exit 2
"#,
            S4 = S4,
        )
        .as_str(),
    );
    BUILD.call_once(|| {
        let status = Command::new("cargo")
            .args(["build", "--bin", "fno-agents"])
            .current_dir(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .unwrap()
                    .join("fno-agents"),
            )
            .status()
            .expect("could not run cargo for the sibling binary");
        assert!(status.success(), "building the fno-agents binary failed");
    });

    let bin = fno_agents_bin();
    let fno_bin: &Path = env!("CARGO_BIN_EXE_fno").as_ref();

    // Door 1: the registry sweep. The row leaves the registry, the receipt
    // it stages is what door 3's receipts (claude,U1) pair-leg reads.
    let d1 = fleet.run(&shim_dir, &bin, &["reap"]);
    println!("door 1 (reap): {d1}");
    let registry = std::fs::read_to_string(fleet.agents_home().join("registry.json")).unwrap();
    assert!(
        !registry.contains(ROW1),
        "door 1: registry still holds the row"
    );
    // Door 2: the roster sweep. The agent list drops the roster-only row.
    let d2 = fleet.run(&shim_dir, &bin, &["roster-reap", "--apply"]);
    println!("door 2 (roster-reap): {d2}");
    let c2 = fleet.dir.join("claude-c2.json");
    assert!(
        !c2.exists(),
        "door 2: the agent list still holds the roster-only row"
    );
    // Door 3: the squad prune. m1 reads dead through the receipt its
    // removal staged; m2 reads dead through the cascade (node-route).
    let d3 = fleet.run(
        &shim_dir,
        fno_bin,
        &["mux", "workspace", "prune", "--include-named"],
    );
    println!("door 3 (squad prune): {d3}");
    fleet.assert_live_stays("after all doors");
    fleet.assert_property("after all doors");

    // The restart half: every door re-runs in fresh processes and must
    // change nothing.
    let before = std::fs::read_to_string(fleet.agents_home().join("squads.json"));
    let r1 = fleet.run(&shim_dir, &bin, &["reap"]);
    println!("restart (reap): {r1}");
    let r2 = fleet.run(&shim_dir, &bin, &["roster-reap", "--apply"]);
    println!("restart (roster-reap): {r2}");
    let r3 = fleet.run(
        &shim_dir,
        fno_bin,
        &["mux", "workspace", "prune", "--include-named"],
    );
    println!("restart (squad prune): {r3}");
    fleet.assert_live_stays("after restart re-run");
    fleet.assert_property("after restart re-run");
    let after = std::fs::read_to_string(fleet.agents_home().join("squads.json"));
    match (before, after) {
        (Ok(b), Ok(a)) => assert_eq!(b, a, "the re-run rewrote the squad store"),
        (Err(_), Err(_)) => {}
        (b, a) => panic!("squad store changed shape on re-run: {b:?} vs {a:?}"),
    }
}

impl Fleet {
    /// Run one door as a fresh process. The env IS the fleet.
    fn run(&self, shim_dir: &Path, program: &Path, args: &[&str]) -> String {
        let output = Command::new(program)
            .args(args)
            .env("FNO_AGENTS_HOME", self.agents_home())
            .env("HOME", self.home())
            // The config-declared account roots: the settings path makes the
            // roster reader and the transcript index read the fleet's
            // config.toml (its parent dir), never the developer's.
            .env(
                "FNO_GLOBAL_SETTINGS_PATH",
                self.agents_home().join("settings.toml"),
            )
            .env("FNO_AGENTS_BIN", fno_agents_bin())
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    shim_dir.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env_remove("FNO_CONFIG")
            .output()
            .expect("door process failed to start");
        let out = format!(
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "door {args:?} failed: {out}");
        out
    }

    /// The converse half of the property (review finding on x-0d08): the
    /// LIVE row survives every door in all three stores. A sweep that
    /// flips a live row to dead fails HERE, not on an operator's machine.
    fn assert_live_stays(&self, after: &str) {
        let registry = std::fs::read_to_string(self.agents_home().join("registry.json")).unwrap();
        assert!(
            registry.contains(ROW4),
            "{after}: the sweep reaped the live row from the registry"
        );
        let squads = std::fs::read_to_string(self.agents_home().join("squads.json")).unwrap();
        assert!(
            squads.contains(ROW4),
            "{after}: the squad store dropped the live row's membership"
        );
        let c4 = self.dir.join("claude-c4.json");
        assert!(
            c4.exists(),
            "{after}: the claude agent list dropped the live row"
        );
    }

    /// The property, asserted after every door: the row is absent from all
    /// three stores.
    fn assert_property(&self, after: &str) {
        let registry = std::fs::read_to_string(self.agents_home().join("registry.json")).unwrap();
        assert!(
            !registry.contains(ROW1),
            "{after}: registry still holds {ROW1}: {registry}"
        );
        let squads = std::fs::read_to_string(self.agents_home().join("squads.json")).unwrap();
        assert!(
            !squads.contains(ROW1) && !squads.contains(ROW2_NAME) && !squads.contains(ROW3),
            "{after}: the squad store still holds dead member(s): {squads}"
        );
        for (marker, id) in [
            ("claude-c1.json", S1),
            ("claude-c2.json", S2),
            ("claude-c3.json", S3),
        ] {
            let path = self.dir.join(marker);
            if path.exists() {
                let raw = std::fs::read_to_string(&path).unwrap();
                assert!(
                    !raw.contains(id),
                    "{after}: the claude agent list still holds {id}: {raw}"
                );
            }
        }
    }
}
