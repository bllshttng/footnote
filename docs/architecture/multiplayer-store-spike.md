# Multiplayer store spike

This page decides which shared database a second machine dials. It holds the bars, the measurements, and the ruling. Probes ran against a copy of `graph.db`, never the live store. Probe code stays out of the tree.

## Topology under test

The first two machines are the MacBook (M2 Max, 96 GB) and the iMac (Intel i7-10700K, 128 GB). Tailscale joins them. The primary runs on the iMac. The MacBook is the remote client. A Synology DS920+ is scored beside the iMac as a second primary host.

One rule binds every option: SQLite never sits on an SMB or NFS share. Network file systems break SQLite's locks and its WAL shared memory, so two hosts that open one file over a share corrupt it. Every shared option here is a server process that owns its own local disk.

## Bars, set before measuring

The bars were committed before any remote number was taken. Each bar is a p95 over cold requests: a fresh connection per request, because every fno verb today is one process per request.

| Verb | Remote primary bar | Local replica bar |
|---|---|---|
| Claim acquire (one conditional write) | 100 ms | not applicable: a claim never decides from a replica |
| Narrow read (one node by id) | 100 ms | 10 ms |
| Whole-graph read (every node row) | 1,000 ms | at or under today's local read |
| Read-your-writes on one host | the read sees the write | the read sees the write |

A database that misses any bar is named with the failing verb and the measured time, and the ruling recommends against it.

## The load today

The request log is the counter: set `FNO_STORE_EXEC_LOG=<path>` and every `--store-exec` request appends one JSON line. The line holds the method, the op name, read or write, ok, rows in the reply, reply bytes, and serve time in microseconds. Unset, the lane writes nothing. An end-to-end check ran against the graph copy. It logged a two-id read as 2 rows and a whole read as 3,958 rows of 19.5 MB. It logged an `op` call as a write.

A working day of counts needs the counter in the deployed binary, so that run follows the merge. Until then the floor comes from the process table. A `ps` sampler ran for 10 minutes on 2026-10-05 and excluded every probe path. It took 3,191 snapshots, one every 0.19 s, and saw 121 distinct request processes. That is a floor of about 12 requests a minute, or 730 an hour. Each process showed in 7.7 snapshots on average, so a typical request lived about 1.4 s. The sampler misses any request shorter than one gap, and it cannot see direct file readers. The fleet incident lane reported the MacBook overloaded during the window, at about 9 jobs waiting per core.

One whole-graph read today is large and slow, measured with the deployed binary on the copy (15 runs):

| Request | p50 | p95 | Rows | Reply bytes |
|---|---|---|---|---|
| `read` (whole graph) | 3,285 ms | 6,023 ms | 3,958 | 19,500,764 |
| `read_ids` (one id) | 1,625 ms | 2,850 ms | 1 | 2,498 |

The narrow read costs half a whole read because it builds the whole cache to apply the readiness overlay. The MacBook ran a live fleet during the sample, so these times include contention. They are today's local cost, and the remote numbers below compare against them.

## Network

Both Macs reach Tailscale over Wi-Fi today: the MacBook on `en0`, the iMac on `en1`. The path is direct, not relayed. 100 pings measured p50 6.5 ms, p95 78.7 ms, max 155 ms. The p95 network tail alone takes most of a 100 ms bar, so the remote p95 numbers below mostly measure Wi-Fi. A wired primary removes half of that tail.

## Measurements

Servers ran in Docker on the iMac, bound to its tailnet address only: `ghcr.io/tursodatabase/libsql-server` (sqld) and the official `postgres:17` image from its public ECR mirror. The client ran on the MacBook. Both stores held the same two tables: `nodes(id, body)` seeded from the 3,958 reply entries of the copy, and `claims(key, holder, expires_ms)`. The claim is one conditional statement. It inserts a new claim, or takes over a claim whose expiry has passed.

```sql
INSERT INTO claims(key, holder, expires_ms) VALUES (?, ?, ?)
ON CONFLICT(key) DO UPDATE SET holder = excluded.holder, expires_ms = excluded.expires_ms
WHERE claims.expires_ms < ?
```

Each verb ran twice, 30 then 60 samples, with whole reads at a third of that. The table shows the second run. Cold opens a new connection per request. Warm reuses one.

| Verb | libSQL cold | libSQL warm | Postgres cold | Postgres warm | libSQL replica |
|---|---|---|---|---|---|
| Claim acquire p50 / p95 | 22 / 108 ms | 13 / 49 ms | 62 / 184 ms | 10 / 31 ms | goes to the primary |
| Narrow read p50 / p95 | 18 / 105 ms | 10 / 23 ms | 59 / 148 ms | 9 / 61 ms | 0.0 / 0.1 ms |
| Whole read p50 / p95 | 1,176 / 1,340 ms | 1,134 / 1,207 ms | 897 / 1,034 ms | 828 / 967 ms | 24 / 41 ms |

The first run agrees. libSQL cold claim acquire read p50 16 ms and p95 30 ms. Postgres cold read p50 68 ms and p95 165 ms.

Four findings carry the ruling.

1. Postgres pays for a connection per request. Cold, every Postgres verb costs about 60 ms at p50, three times libSQL. The startup and password exchange add round trips that an HTTP request does not. Warm, the two databases tie. Postgres meets the bars only behind a resident pooled connection.
2. No remote whole read meets the bar. libSQL misses it at 1,340 ms, and Postgres misses it at 1,034 ms. Both carry 21.5 MB over the link. By default sqld refuses the read in one response (`RESPONSE_TOO_LARGE`). The probe paged it by id, 1,000 rows a page.
3. The embedded replica answers whole reads in 24 ms, against 3,285 ms for today's local lane. A narrow read takes under a millisecond. Postgres has no embedded replica.
4. libSQL cold claim acquire missed the 100 ms p95 bar by 8 ms in one of two runs. The miss tracks the Wi-Fi tail (ping p95 79 ms). The p50 of 22 ms sits well inside the bar.

## Read-your-writes

A write through embedded replica A, then a read on replica A at once, sees the write. Replica B on the same primary does not see it until B syncs. After `sync()`, B sees it. The write through the replica took 187 ms, one sample.

So a replica is fresh for its own writes and stale for another machine's writes until it syncs. That is why no decision reads a replica: a claim is the conditional statement above, run on the primary.

## Replica sync cost

| Step | p50 | p95 |
|---|---|---|
| First sync of a new replica (24 MB) | 10,094 ms | one sample |
| Sync with nothing new, open connection | 9 ms | 95 ms |
| Open the replica and sync, fresh each time | 303 ms | 469 ms |

A one-shot process that opens and syncs pays 303 ms. So a verb process must never sync. One resident syncer per machine keeps the replica fresh, and each verb process reads the local file.

## Hosting the primary: iMac or DS920+

The iMac was measured. The DS920+ was not: it is not on the tailnet today. Its column is a score from Synology's published spec, not a measurement. The spec lists a 4-core Celeron J4125, 4 GB RAM that grows to 8 GB, two 1 GbE ports, and two M.2 slots.

| Criterion | iMac | DS920+ |
|---|---|---|
| Measured | yes, every number above | no |
| Link | Wi-Fi today; a cable would help | wired 1 GbE |
| Uptime | runs its own worker fleet and Claude daemon; load average 5.9 during the probe; a fleet restart or reboot takes the primary down for both machines | built to run all day; no agents run on it |
| CPU | 8 cores, ample | 4 low-power cores; enough for claim writes and replica sync, slower for a first sync |
| RAM | 128 GB | 4 GB; sqld needs little, Postgres fits but leaves less room |
| Disk | internal SSD | spinning disks unless an SSD volume is set up; each commit waits on a disk flush |
| Docker | a Linux VM on macOS; it ran both servers here | Container Manager, native on x86; the sqld image ships for x86 |
| Backups | Time Machine | volume snapshots and Hyper Backup |
| SQLite on a share | never | never: sqld keeps its data on the NAS's own volume, and no machine opens a file on a NAS share |

The DS920+ wins on the axis that matters most for a primary. If the fleet goes down, the NAS stays up. It loses on disk flush time, and its numbers are unknown. The iMac wins today because it is measured and needs no setup.

## Ruling

The decision record holds this ruling under the subject `multiplayer-store`. Run `fno backlog decisions multiplayer-store` to print it.

**Database: libSQL (sqld), with an embedded replica on every machine.**

- Reads, the whole-graph read included, come from the local replica. The replica answers in 24 ms where today's lane takes 3,285 ms.
- Every claim and every write goes to the primary as one conditional statement. libSQL cold claim acquire is 22 ms at p50.
- One resident syncer per machine keeps its replica fresh. A verb process never syncs.
- The SQL dialect stays SQLite. No second dialect enters the tree.

Against Postgres: it misses every cold bar. Cold claim acquire p95 is 184 ms, narrow read p95 is 148 ms, and whole read p95 is 1,034 ms. It has no embedded replica, so every whole read crosses the network. A pooled resident connection fixes the first two. Nothing fixes the third.

**Primary host: the iMac now, the DS920+ next.** Start the primary on the iMac, because it passed these probes with no new setup. Move it to the DS920+ once the NAS joins the tailnet and passes the same probe at the same bars. The NAS runs sqld in Container Manager with its data on the NAS's own volume. Put the iMac on a cable either way.

**Config key: `store.remote_url`.** It holds the primary's libSQL URL, for example `http://imac2d2:18080` on the tailnet. Unset, the local file stays the store, as it is today.

The day-long request count does not change this ruling. Reads go to the replica at any count. Writes go to the primary, and the counter will size that write load.
