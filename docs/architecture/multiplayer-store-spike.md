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
