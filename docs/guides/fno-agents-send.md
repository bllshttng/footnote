# How-to: send an async message to a peer agent with `fno mail send`

`fno mail send <name> "<message>"` is the async sibling of `ask`. It writes the envelope durably, attempts live delivery by tier, and returns immediately - the caller never blocks waiting for a reply. Like `ask`, it requires the target agent to already exist; an unknown name exits 16 with a "spawn it first" hint (Locked Decision 1 from the cross-agent bus epic: ask and send never create peers).

Use this guide when a script or LLM session needs to hand off work to a peer without waiting for the response, or when the recipient may be offline and you want the message to land in their inbox for pickup at next turn-start.

## Prerequisites

- `fno` CLI installed; the compiled `fno-agents` binary on `$PATH` for codex/gemini hosted delivery.
- The target agent already registered via `fno agents spawn <name> --provider <p>` - see [fno-agents-spawn.md](fno-agents-spawn.md).

## Basic send

```bash
fno mail send frontend-worker "run the failing tests and open a PR"
```

stdout is exactly one line, always exit 0:

```
msg-3a7f1c2e delivered (hosted)
```

or

```
msg-3a7f1c2e queued (durable)
```

`delivered (hosted)` means live PTY injection (codex/gemini) or the `control.sock` `op:'reply'` inject (claude, via the `fno-agents mail-inject` verb) succeeded. `queued (durable)` means the message is in the recipient's inbox store, waiting for their next drain. Both are exit 0. The `msg-<8hex>` id is stable and can be used to correlate a later reply in the bus log.

## Flags

| Flag | Short | Default | Purpose |
|---|---|---|---|
| `--provider` | `-p` | (none) | Optional provider hint. Must match the registered provider if given; mismatch is exit 2. |
| `--cwd` | `-c` | `$PWD` | Working directory context stamped in the dispatch context envelope. |
| `--from-name` | | `footnote` | Identity advertised in the on-the-wire container. Must be XML-attribute-safe (no `"`, `<`, `>`, `&`; max 128 chars). |

## Identify yourself to the recipient

```bash
fno mail send backend-worker "review my PR diff" --from-name "orchestrator-alpha"
```

The recipient sees the body inside the `<fno_mail from="..." harness="..." model="...">` envelope. The framing marks the sender as a peer, not the operator, so the receiving model responds in a directed style rather than treating the injection as a user interrupt. See [docs/architecture/mail-live-inject.md](../architecture/mail-live-inject.md) for the envelope and delivery model.

## Live-inject-first semantics

As of node x-1f23 the order is reversed from the old durable-first design: `send` attempts LIVE delivery first and writes the durable bus ONLY when the recipient is not live-reachable or the live inject does not confirm. A confirmed live (`hosted`) delivery is self-recording in the transcripts and is NOT also queued, so the durable bus is the fallback tier (the offline pending-queue), not a peer to the live path. If live delivery fails for any reason (daemon unreachable, injection gate not passed, not confirmed), the durable copy is written then. The stdout line reflects the actual outcome; `queued (durable)` is a success state, not an error. A busy recipient whose injected turn is queued past the confirm budget can receive both the live inject and the durable copy (a bounded duplicate; see the architecture doc).

When live delivery demotes to durable, a notice goes to stderr:

```
live delivery failed for 'backend-worker'; message queued durable (msg-3a7f1c2e)
```

The process still exits 0.

## Delivery tiers

Send resolves delivery in order:

**claude peers.** Delivery goes through the provider socket path (MCP-channel probe first with a 0.25 s timeout, then the messaging socket). This is fire-and-forget - no reply wait. Claude peers are never PTY-injected (Locked Decision 9); the daemon's `agent.deliver` RPC returns `delivered: false, reason: "claude-routes-via-socket"` for claude names, and the Python dispatcher handles the socket delivery directly.

**codex and gemini hosted peers.** The Python dispatcher calls the daemon's `agent.deliver` RPC over the Unix supervisor socket. The daemon checks the per-provider injection gate, then writes the bracketed-paste frame into the worker's PTY socket. If the daemon is unreachable, `_daemon_rpc` returns None and the message goes durable with a stderr notice - no exception raised, no nonzero exit.

**Everything else** (offline peer, gate not passed, inject failed). The message stays in the durable inbox store. The recipient picks it up at their next drain or turn-start.

## Failure modes

| Condition | Exit | Stderr |
|---|---|---|
| Body exceeds 1 MiB | 2 | `message body exceeds maximum size (1 MiB); got N bytes` |
| Name/message/from-name fails validation | 2 | validation message |
| Unknown agent | 16 | `unknown agent '<name>'; spawn it first: fno agents spawn <name> -p <provider>` |
| Provider mismatch | 2 | mismatch description |
| Registry read error | 12 | `registry read failed: ...` |
| Lock timeout (another send/ask holds the per-agent flock) | 11 | `timed out waiting for agent '<name>' lock (timeout=Ns)` |
| Durable envelope write failed | 12 | `durable envelope write failed: ...` |
| Live delivery demoted | 0 | demotion notice on stderr; stdout says `queued (durable)` |

The body cap (1 MiB) is enforced BEFORE any inbox store write, so a rejected oversized message leaves no partial record.

## Managing the injection gate

Live PTY delivery for codex and gemini requires a passed injection gate. The gate answers whether that provider's TUI safely queues mid-turn typed input without interrupting the running turn or concatenating with existing composer input. Until it passes, messages go durable.

Check the current gate status:

```bash
fno agents gate codex
# provider=codex status=absent (no gate file found at ~/.fno/agents/injection-gate.json)
```

Run an automated probe (currently always returns inconclusive - PTY behavior cannot be reliably observed without human verification):

```bash
fno agents gate codex --probe
# provider=codex probe status=inconclusive
```

Record a manual attestation after you have verified the behavior yourself:

```bash
fno agents gate codex --record passed --notes "verified 2026-06-07 on codex 1.x"
# provider=codex status=passed method=manual
```

Once a provider is marked passed, `send` to hosted peers of that provider delivers live via PTY injection. To revert:

```bash
fno agents gate codex --record failed --notes "regression seen in codex 1.y"
```

`--probe` and `--record` are mutually exclusive. The gate file is never written on a probe result (inconclusive probes must never flip a provider to passed). The daemon must be running for `--probe` or `--record` to work; if it is unreachable the command fails nonzero and prints a stderr notice - the gate file is never written silently on error.

## See also

- [fno-agents-spawn.md](fno-agents-spawn.md) - create the peer before sending
- [fno-agents-ask-followup.md](fno-agents-ask-followup.md) - synchronous ask (blocks for the reply)
- [docs/architecture/fno-agents-deliver-gate.md](../architecture/fno-agents-deliver-gate.md) - daemon RPC internals, injection gate schema, event kinds
