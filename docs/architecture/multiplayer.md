# Multiplayer: one shared primary for dispatch claims

Two or more machines run one fleet. Each machine dials out to one libSQL server (sqld), the primary. No machine accepts a connection from another, so the machines need no shared file system. The ruling that chose libSQL, its host, and the config key prints with `fno backlog decisions multiplayer-store`.

## The key

`store.remote_url` in the per-machine global config (`~/.fno/config.toml`) turns it on. It holds the primary's URL, for example `http://r2d2:18080` on the tailnet. `store.remote_token` is optional. When it is set, each request carries it as a bearer token. The key is per machine and never enters git. A project config cannot set it, because the reader reads the global file only.

Unset is the stock install. Every store stays a local file and no fno process opens a socket for it. That holds forever: a machine with no account and no network keeps working.

A malformed value refuses every shared claim, with a message that names the key. A typo never falls back to local in silence. Only `http://` URLs are accepted, because the client carries no TLS. The tailnet owns remote reach, the same way `fno web` leaves it.

## What moves to the primary

The claim keys that decide dispatch move: `node:`, `dispatch:`, and `reconcile:` (`SHARED_PREFIXES` in `crates/fno-agents/src/claim_store.rs`). Every statement runs the same SQL on either store, so the local and remote legs cannot drift. Each claim is one conditional statement on the primary, and no decision reads a copy.

Every other claim stays local. `build:cargo`, `test:`, `session:`, `worker:` and `flight:` name resources of one machine. A cargo lock shared across machines serializes builds that never touch each other.

The agent registry, mail and the event store stay local. A second key moves the backlog, as the next section says. Without it, each machine keeps its own backlog in `graph.db`. The node claim is the dispatch decision. So two machines never build one node, even with backlog copies that disagree on status.

## The shared backlog

`store.share_backlog = true` in the global config moves the backlog tables to the primary too. It needs `store.remote_url`. It is off by default. With it off, no backlog write opens a socket.

The tables that move are listed in `SHARED_TABLES` in `crates/fno-agents/src/backlog_share.rs`. The content version, the sync cursor and the search index stay local in `graph.db`. A new table in neither list fails a test.

Reads stay local. This machine's `graph.db` is a replica. One daemon arm per machine applies the primary's change log every 5 seconds. `fno agents claim backlog sync` does it at once.

Every backlog write opens through one seam, `backlog::open_connection`. With the key on, a hook there records each row change. At commit, the changes go to the primary in one request, as one transaction. Each update and delete matches every old column of its row. Each insert must not collide. The same request adds one row to the primary's change log.

A row that a peer changed first refuses the whole write. The local transaction rolls back, so nothing is written on either side. The refusal names the table. A locked mutate syncs the replica and retries by itself. Other writers print the refusal and stop.

A primary that cannot be reached refuses every backlog write, with the URL in the message. Reads keep working from the replica. To work alone, unset `store.share_backlog`.

## Start sharing

1. Copy `graph.db` first. The first sync on a machine replaces that machine's backlog with the primary's. It also writes a `graph.pre-share-<ms>.db` backup beside the store.
2. On one machine, set both keys and run `fno agents claim backlog seed`. It copies this backlog to the primary. It refuses a primary that already holds one.
3. On every other machine, set both keys and run `fno agents claim backlog sync`.

A write to a primary with no seeded backlog refuses and names the seed verb.

A later fno version that adds a backlog column does not change the primary. Writes then refuse with the primary's "no such column" error. Export, unset the keys, and seed again from a current machine.

## The store clock decides a peer's lease

A machine cannot probe a peer's pids. So for a row another machine wrote, only the lease decides, read on the primary's clock. A takeover of a peer's row carries `claims.expires_at <= <store clock>` in its compare-and-swap. Two machines with skewed clocks still agree on the moment a lease ran out.

## The lease heartbeat

A worker renews its node claim at every stop. One turn can run for hours, and no stop comes in that time. So the daemon renews too (`crates/fno-agents/src/lease_heartbeat.rs`). Every 60 seconds it reads the shared claims the primary records for this machine. It keeps the ones whose holder pid is alive here, and moves their leases to 10 minutes past the store clock, in one statement. A claim whose pid is dead is not renewed, so it lapses and a peer can take it.

## Failure: the primary is unreachable

A claim verb refuses. The error names the URL and the key, and says nothing was written locally. A request that timed out can still have reached the primary, so a retry by the same holder is the safe next step. A 4xx reply, such as a bad token, is named as a refusal and never read as an outage. `fno backlog next` refuses selection. `fno do target init` writes a cancel signal with `store_unreachable` and owns no node. A running worker finishes its turn: at its stop, loop-check reads the unreachable renewal and allows the stop, so the worker holds. The machine claims no new work until the network returns. To work alone, unset the key.

## Failure: a peer took the claim

A machine that stays offline past its lease loses the claim, and a peer can take it. When the network returns, the heartbeat names the taker in a `claim_lost` event. At the worker's next stop, loop-check reads the same reason and ends the run with `Interrupted`. The message tells the worker to stop and not to push. A holder change on this same machine is a handover, never a loss.

## Leaving: export

`fno-agents claim export --out <file> [--url <primary>]` copies every table on the primary into a new local SQLite file. The file opens as a normal store. The primary stamps its claims table as the authority at creation, so the local open keeps the exported rows. Use `--url` after the key is unset.

## Test it

The two-machine walk-through is [docs/guides/multiplayer-test.md](../guides/multiplayer-test.md). It uses a throwaway primary and never the live store.
