# Two-machine test for the shared claim store

This guide runs the shared primary end to end on a throwaway database. The design is in [docs/architecture/multiplayer.md](../architecture/multiplayer.md). Plan about 30 minutes.

Two rules hold for the whole test. Never point a test at the live store. Never put a SQLite file on an SMB or NFS share, because network file systems break SQLite's locks.

## 1. Start a throwaway primary

Run sqld on the host that will be the primary, with its data on that host's own disk:

```bash
docker run -d --name fno-sqld-test -p 18080:8080 -e SQLD_NODE=primary \
  -v "$HOME/fno-sqld-test:/var/lib/sqld" ghcr.io/tursodatabase/libsql-server:latest
```

The primary starts empty. The first claim creates the tables, so no seed step is needed.

## 2. Set the key on both machines

Add this to `~/.fno/config.toml` on each machine. Use the primary's tailnet name:

```toml
[store]
remote_url = "http://r2d2:18080"
```

Release every node claim on a machine before you set the key. A claim in the local file stays there, and the primary never sees it. Restart the daemon after a change, because the daemon reads the key once.

To rehearse on one machine instead, give each simulated machine its own home. Set `HOME`, `FNO_AGENTS_HOME`, `FNO_STATE_DIR`, and `FNO_CLAIMS_ROOT` to a fresh temp directory per machine. Put the key in that home's `.fno/config.toml`.

## 3. Prove one node goes to one machine

On machine A, take a node:

```bash
fno agents claim acquire node:x-test --holder a-test --ttl 30m
```

On machine B, try the same node:

```bash
fno agents claim acquire node:x-test --holder b-test --ttl 30m
```

Expected: B exits 1 with `held_by_other` and names A's holder. Then run real targets on both machines from one backlog. No node is dispatched twice.

## 4. Pull the network on one machine

Turn off Wi-Fi on machine B during a run. Expected:

1. `fno agents claim acquire` on B exits 3. The message names `store.remote_url` and says nothing was written locally.
2. A worker on B finishes its turn. At its stop, loop-check prints `holding:` and allows the stop.
3. A takes none of B's claims for the 10-minute lease.

Keep B offline past 10 minutes, then take one of B's nodes on A. Bring B back. Expected: B's daemon writes a `claim_lost` event, and B's worker ends at its next stop with `claim lost:` and the taker's name.

## 5. Share the backlog on rehearsal homes

Run this step only on the per-machine temp homes from step 2, never on `~/.fno`. Copy the real store into home A so the test has real rows:

```bash
cp ~/.fno/db/graph.db "$HOME_A/.fno/db/graph.db"
```

Add `share_backlog = true` under `[store]` in both homes' config. Then, with home A's environment:

```bash
fno agents claim backlog seed
```

The receipt names each table and its row count. With home B's environment, run `fno agents claim backlog sync`. Expected: `snapshot: true`, and `fno backlog get <id>` on B prints the node A holds.

File a node on A, run the sync on B, and read it on B. Then change one node's title on A, and change the same node on B before B syncs. Expected: B's write refuses, names the `nodes` row, and leaves B's row as it was. After the next sync, B shows A's title.

## 6. Roll back

Remove the `[store]` table on both machines, and restart each daemon. Then copy the primary into a local file:

```bash
fno-agents claim export --url http://r2d2:18080 --out ~/fno-primary-export.db
```

The receipt names each table and its row count. Read a claim back from the file:

```bash
sqlite3 ~/fno-primary-export.db "SELECT key, holder, host FROM claims"
```

Then stop and remove the throwaway primary with `docker rm -f fno-sqld-test`.
