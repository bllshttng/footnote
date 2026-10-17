# Multiplayer: one shared primary for dispatch claims

Two or more machines run one fleet. Each machine dials out to one libSQL server (sqld), the primary. No machine accepts a connection from another, so the machines need no shared file system. The ruling that chose libSQL, its host, and the config key prints with `fno backlog decisions multiplayer-store`.

## The key

`store.remote_url` in the per-machine global config (`~/.fno/config.toml`) turns it on. It holds the primary's URL, for example `http://r2d2:18080` on the tailnet. `store.remote_token` is optional. When it is set, each request carries it as a bearer token. The key is per machine and never enters git. A project config cannot set it, because the reader reads the global file only.

Unset is the stock install. Every store stays a local file and no fno process opens a socket for it. That holds forever: a machine with no account and no network keeps working.

A malformed value refuses every shared claim, with a message that names the key. A typo never falls back to local in silence. Only `http://` URLs are accepted, because the client carries no TLS. The tailnet owns remote reach, the same way `fno web` leaves it.

## What moves to the primary

The claim keys that decide dispatch move: `node:`, `dispatch:`, and `reconcile:` (`SHARED_PREFIXES` in `crates/fno-agents/src/claim_store.rs`). Every statement runs the same SQL on either store, so the local and remote legs cannot drift. Each claim is one conditional statement on the primary, and no decision reads a copy.

Two `gh-budget:` rows live there too. `gh-budget:fleet` is the shared GitHub refusal hold. `gh-budget:poller` is the status poller lease, described in [One GitHub poller](#one-github-poller). Neither row decides dispatch.

Every other claim stays local. `build:cargo`, `test:`, `session:`, `worker:` and `flight:` name resources of one machine. A cargo lock shared across machines serializes builds that never touch each other.

The agent registry, mail and the event store stay local. A second key moves the backlog, as the next section says. Without it, each machine keeps its own backlog in `graph.db`. The node claim is the dispatch decision. So two machines never build one node, even with backlog copies that disagree on status. Cross-host mail is the exception: the send writes one row on the primary, and the next section carries it.

Head-pinned review attestations travel too, as their own carrier. The event store itself stays local. Every committed `review_attestation` row is mirrored to a `review_attestation` table on the primary at commit time, best-effort after the local commit. Every gate read (loop-check's attestation chain, the merge coverage read) also pulls the repo's rows back and dedups them against the local journals. A review on one machine covers a merge on another. A pull failure degrades to machine-local coverage, the pre-carrier behavior. The carrier verbs are `fno-agents attest push` and `fno-agents attest pull`, and the two-machine proof is `tests/ci/test_multiplayer_attestation.sh`.

The primary is the coverage authority, so write access to it is review authority. Any writer on the primary can add a `review_attestation` row the gate reads as a review, so the set of machines with `store.remote_url` is the set of machines that can vouch a pass. The push stamps the writing machine id into the row (`data.attest_machine`), and the coverage listing shows it. That makes the row auditable, not true. Treat a primary row like the journal it mirrors: trust is the operator's machine list, and a foreign writer is a trust decision, not a bug. A gate read never writes schema: a pull that finds no table reads no rows instead of creating one.

## The shared backlog

`store.share_backlog = true` in the global config moves the backlog tables to the primary too. It needs `store.remote_url`. It is off by default. With it off, no backlog write opens a socket.

The tables that move are listed in `SHARED_TABLES` in `crates/fno-agents/src/backlog_share.rs`. The content version, the sync cursor and the search index stay local in `graph.db`. A new table in neither list fails a test.

Reads stay local. This machine's `graph.db` is a replica. Every 5 seconds, one daemon arm per machine sends the outbox and then applies the primary's change log. `fno agents claim backlog sync` does both at once.

Every backlog write opens through one seam, `backlog::open_connection`. With the key on, triggers there copy each row change into the local `backlog_outbox`, in the writer's own transaction. The commit stays local, so no writer holds the graph lock across the network. A flush then sends the outbox to the primary in one request, as one transaction. Each update and delete matches every old column of its row. Each insert must not collide. The same request adds one row to the primary's change log. The flush takes its own lock file, never the graph lock. A write through `mutate_rows` flushes right after the graph lock drops. The daemon flushes every other write within 5 seconds.

A build from before the shared backlog writes with no outbox, and the primary never sees that write. So a store that shares carries a guard trigger on each shared table. The trigger calls `fno_backlog_writer()`, and only a current build registers that function. An older `fno-agents`, daemon, worker, or a plain `sqlite3` shell fails its write with `no such function: fno_backlog_writer` and changes nothing. When the key is unset, the next open by a current build drops the guard. Before you install an older build, unset the key and run one `fno backlog` command.

A row that a peer changed first refuses the whole batch, and the primary writes nothing. The replica then takes the primary's rows back for every row the batch touched, and the refusal names the table. A write through `mutate_rows` syncs and retries by itself.

The take-back is not silent. It keeps the refused batch in the replica's `backlog_refused` table, exactly as the flush sent it, so a retry or an inspection can read what this machine tried to write. It also prints one notice naming each node and the columns the take-back reverted, and records the same facts as a `backlog_refused_flush` event in the space's `events.jsonl`. A refusal that lands while the writer is between turns still reaches an operator who reads the journal: without it, an update that printed `Updated` can revert minutes later, and nothing says so until a human notices the node back at `idea`.

If the primary cannot be reached, backlog writes still land in the replica and wait in the outbox. The next flush that reaches the primary sends them, and a peer's newer change refuses them then. Reads keep working from the replica. To work alone for good, unset `store.share_backlog`.

## Start sharing

1. Run `fno doctor update` on every machine and restart its daemon. Once the store shares, an older `fno-agents`, `fno-agents-daemon` or `fno-agents-worker` cannot write the backlog.
2. Copy `graph.db`. The first sync on a machine replaces that machine's backlog with the primary's. It also writes a `graph.pre-share-<ms>.db` backup beside the store.
3. On one machine, set both keys and run `fno agents claim backlog seed`. It copies this backlog to the primary. It refuses a primary that already holds one.
4. On every other machine, set both keys and run `fno agents claim backlog sync`.

If the primary holds no seeded backlog, a flush refuses and names the seed verb. The writes wait in the outbox.

A later fno version that adds a backlog column does not change the primary. Writes then refuse with the primary's "no such column" error. Export, unset the keys, and seed again from a current machine.

## A node on a second machine

These facts travel with the store. The backlog rows in `SHARED_TABLES` move: nodes, notes, decisions, sessions and pull requests. The dispatch claims move. Mail to `<name>@<host>` moves.

These facts stay on each machine. The agent registry, the roles, the questions, the event store, the worktrees, the credentials and the harness logins stay. The config stays too. That covers `lead.enabled`, the work map and the agent caps. The plan file stays. A node's `plan_path` is a path on the machine that wrote it, and fno does not copy the file. Review attestations are the one event-store exception. They reach the primary through their own carrier, so machine B's merge gate sees machine A's review.

To build a node on this machine, follow these steps in order:

1. Share the backlog with the steps above. Then `fno backlog get <node>` answers.
2. Copy the first machine's setup with a join, then register this checkout. On the first machine run `fno config setup run --export --out fno-profile.toml`; copy the file here and run `fno do wizard --join fno-profile.toml`. The join applies the portable keys; paths, workspaces and machine names stay behind by class (`classify` in `crates/fno/src/setup_profile.rs` keeps `work.` local on purpose). Tick the wizard's projects screen so it appends this checkout to `work.workspaces.main.projects`. Then `fno backlog get <node> --field _resolved_cwd` must print a directory that exists here. Without the row, it prints the path of the machine that filed the node.
3. Run `fno do target start <node>`. The node claim lives on the primary, so two machines never build one node.

To lead a scope on this machine, follow these steps in order:

1. Run `fno agents register`, so this session has a row in this machine's registry.
2. Check that the scope is an epic. If it is mistyped, `fno backlog update <id> type=epic` fixes it.
3. Run `fno config set lead.enabled true`. While it is false, a grant gives the role and arms no lead loop.
4. A person at an attended shell on this machine runs `fno agents org promote <handle> --scope <scope>`. A lead on another machine cannot grant here, because its role is not in this registry.
5. If the grant armed no manifest, run `fno agents org init --scope <scope> --harness-session-id <full session id>`.

A session with no role still reads, reports and mails a named peer. The lead skill lists the three forms.

## The store clock decides a peer's lease

A machine cannot probe a peer's pids. So for a row another machine wrote, only the lease decides, read on the primary's clock. A takeover of a peer's row carries `claims.expires_at <= <store clock>` in its compare-and-swap. Two machines with skewed clocks still agree on the moment a lease ran out.

## Cross-host mail

A send to `<name>@<host>` writes one row to `fno_mail_bus`, the mail table on the primary. The primary's `seq` is the only row order. Host clocks never sort rows.

Each host's daemon pulls its rows on `agents.remote.pull_interval_secs`. That key lives in the global config, default 15 seconds. The pull injects each row through the receiver's own local send. Lane selection, holds, and receipts stay the local path's.

The sender reads as `<user>@<host>/<session name>` in the row's `from_id`. The sender writes that field itself, so it proves nothing alone. The tailnet is the boundary. The raw allowlist is a second check, never a proof of identity (operator ruling, 2026-10-10). A `raw = 1` row is the pane-typing lane. The pull refuses it unless `agents.remote.raw_senders` lists the sender head. The empty default refuses every cross-host raw row. The label's name part resolves through the identity tiers: `--from-name`, the ambient session's agent name, the session's registry row, then the OS user. A registered agent session with no flags stamps its role name, so a reply can address `<name>@<host>`.

A row waits up to 7 days for its host (`MAIL_TTL_SECS`). The expire leg flips past-due rows to `expired`. A pull that finds the name unknown leaves the row pending until then. `INSERT OR IGNORE` on the unique `msg_id` makes a retry after an ambiguous timeout a no-op. The receiver's seen ledger (`mail_seen.jsonl` in the agents state root) keeps a redelivered row from injecting twice. Delivery is at-least-once at the bus and once at the receiver.

A final refusal does not wait. A pull that sees exit 1 asks the receiver's own style door about the body. The door naming violations is the style refusal, and a retry replays it. The pull marks such a row `refused`, and the sender's `mail status` shows it. Every other refusal keeps waiting for the TTL. The list: an unknown name (exit 16, once 13), a failed durable write (exit 12), a cap or forged-envelope refusal, a signal kill. The durable demotion (exit 14) is not on the refusal list. That exit means the receiver's local send already wrote its durable copy, so the pull marks the row `delivered` and runs no second send. A failed pull attempt leaves one daemon log line naming the msg_id and the exit, so a row that retries forever is never silent. A pending row that just failed backs its next attempt down a ladder. The rung schedule doubles from 15 s and caps at 15 min. The memory is one persisted entry per msg_id beside the seen ledger (`mail_retry.jsonl`). A pending row pays the send's cold start on the ladder, never once a beat.

The primary can be unreachable. The send then queues the row in `mail_outbox.jsonl` and reports `queued (local, store unreachable)`. When the store answers again, the flush leg drains it. No `store.remote_url` at all means the ssh fallback. The remote machine's own `fno` answers, its exit code and receipt pass through, and `--from-self` refuses on that lane. The reason: that flag stamps whichever machine runs the verb, so pass `--from-name`.

Host spellings normalize on write and on pull. Lowercase, and a trailing `.local` drops, so `iMac2D2.local` and `imac2d2` are one host. The two-machine walk-through is the same throwaway-primary drill as [the shared backlog](#start-sharing).

## The lease heartbeat

A worker renews its node claim at every stop. One turn can run for hours, and no stop comes in that time. So the daemon renews too (`crates/fno-agents/src/lease_heartbeat.rs`). Every 60 seconds it reads the shared claims the primary records for this machine. It keeps the ones whose holder pid is alive here, and moves their leases to 10 minutes past the store clock, in one statement. A claim whose pid is dead is not renewed, so it lapses and a peer can take it.

## One GitHub poller

GitHub's secondary limit counts requests for the account, not for the machine. Two hosts that read one PR inside one status TTL spend two requests where one is enough. With `store.remote_url` set, one host reads GitHub for both (`crates/fno-agents/src/gh_poller.rs`).

The host holds a lease: the row `gh-budget:poller` in the `claims` table on the primary. The lease is two status TTLs long, 120 seconds by default. Only a publish renews it. Each live status read writes a local row and then takes or renews the lease. The host that holds the lease copies the row to the table `gh_status_rows` on the primary. The row names no pid, so the daemon heartbeat never renews it. A poller that stops reading GitHub therefore goes quiet and loses the lease after two TTLs.

Every other host asks the primary first. Suppose a peer holds a live lease, and the shared row for the PR is under two TTLs old. The host then serves that row with zero gh calls. It keeps a copy in its local cache. Past that age the host reads GitHub itself. Its publish then takes the lease, because the old poller's lease ran out at the same moment as the row went stale. A holder that wakes after the takeover fails its renewal and publishes nothing.

Every lease expiry and every row age is computed in SQL on the store clock, as in [the store clock decides a peer's lease](#the-store-clock-decides-a-peers-lease). Two hosts with skewed clocks agree on who holds the lease.

An unset `store.remote_url` keeps today's behavior: each machine polls for itself, and no process opens a socket for this. A primary that errors does not stop a status read. The host writes the marker `poller-down` in its status cache directory, skips the primary for 60 seconds, and polls alone. A status read never fails closed here, because the worst case is today's request rate.

The lease assumes one GitHub account behind a primary, as `gh-budget:fleet` does. Hosts on two accounts must use two primaries. A shared row carries the GitHub facts only: the verdict, the check counts and the optional reviews. A follower drops the fields the poller composed from its own journal and graph. The row then reads `ready: false` with the blocker `status_shared`. `fno do pr status` never serves a shared row, so its `ready` and merge fields always come from this host. The waits and the daemon pollers read only the verdict and the settled flag, so they follow. A shared row serves as a status verdict, so write access to the primary is trust, as it is for [review attestations](#what-moves-to-the-primary). A PR that only one host reads saves nothing. The lease moves no request from one host to another, so there is no arm that polls a follower's PRs. The saving is the overlap: reads of one PR by two hosts inside one TTL. The `pr status` and `pr wait` spend notes count it. `, {n} served from poller <host>` is the number of rows this process took from the poller. `, published {m}` is the number it shared. The loop-check adapter and the review count in `pr wait --until reviewed` do not use the shared rows.

## Failure: the primary is unreachable

A claim verb refuses. The error names the URL and the key, and says nothing was written locally. A request that timed out can still have reached the primary, so a retry by the same holder is the safe next step. A 4xx reply, such as a bad token, is named as a refusal and never read as an outage. `fno backlog next` refuses selection. `fno do target init` writes a cancel signal with `store_unreachable` and owns no node. A running worker finishes its turn: at its stop, loop-check reads the unreachable renewal and allows the stop, so the worker holds. The machine claims no new work until the network returns. To work alone, unset the key.

## Failure: a peer took the claim

A machine that stays offline past its lease loses the claim, and a peer can take it. When the network returns, the heartbeat names the taker in a `claim_lost` event. At the worker's next stop, loop-check reads the same reason and ends the run with `Interrupted`. The message tells the worker to stop and not to push. A holder change on this same machine is a handover, never a loss.

## Leaving: export

`fno-agents claim export --out <file> [--url <primary>]` copies every table on the primary into a new local SQLite file. The file opens as a normal store. The primary stamps its claims table as the authority at creation, so the local open keeps the exported rows. Use `--url` after the key is unset.

## Test it

The two-machine walk-through is [docs/guides/multiplayer-test.md](../guides/multiplayer-test.md). It uses a throwaway primary and never the live store.
