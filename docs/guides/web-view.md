# Watch your panes from a browser

`fno mux serve --web` puts a view of a running mux session on an HTTP port. You can open the URL on a phone, a tablet, or another machine, and watch any pane. You install nothing on the device that shows the page.

The view is read-only by construction, not by policy. The bridge sends one attach message upstream, then releases the write half of the socket. No code path can carry a keystroke back to your terminal. The browser drops every inbound message and sends none.

## Start it

```bash
fno mux serve --web                      # the default server, on 127.0.0.1:8722
fno mux serve --web --server work        # a named mux server
fno mux serve --web --port 9000          # a different port
```

The command prints the URL and a token once at bind. The token is in the query string. It is the only guard on the port. Anyone who has the URL can watch the session, so keep the URL secret.

The page's **backlog** link renders the backlog live from the store through the same token. You get lanes by project or epic, six columns, stats, filters, and a node panel with sessions. It needs no render step and never caches. A missing or wrong token returns no backlog bytes.

On a bridge bound to loopback, the node panel can also act. You can edit a node's title, priority, size and status. You can rank a card to the top or bottom of its column. You can launch a blueprint or a target for the node. Every write runs the same `fno` verb an agent runs, and the verb's own refusal is what you see. Every write answers to three conditions. The bridge must be bound to loopback. The request's Host header must name loopback. When your browser sends an Origin header, it must name loopback too. The reason is plain: a web page on another site must not be able to drive the bridge through your browser. On a bridge bound wider than loopback, the page shows no write controls at all.

The bridge also serves the backlog as JSON through the same token. `/backlog/model.json` takes `lanes`, `project`, `epic`, `status`, `priority`, `size`, `lead`, `q` and `all`. `/backlog/node.json?id=<id>` returns one node. When the store holds no such node, the answer is 404. When the store read failed, it is 503. Each body carries `"schema": 1`. The bridge re-reads at most every 30 seconds unless the store changed.

The **rundown** page serves `rundown.html` (`/role` and `term.html` stay as the one-release aliases). If it is older than five minutes, the bridge re-renders it. The **fleet** page serves `fleet.html`. The fno-agents daemon renders it every 30 minutes. If `/fleet` returns 404, the daemon has not ticked yet.

The bridge also writes these values to `<mux dir>/web-<session>.json` at mode 0600. You can get the URL again from this file. The bridge removes the file on exit.

`--server` names the mux server, not an agent. It selects which socket the bridge attaches to. The old `--session` spelling still works and warns. To reach one agent, see the next section.

## Stop it, and clean up after a dead one

`fno mux serve --web --stop` ends the running bridge for one session.

```bash
fno mux serve --web --stop                # the default session
fno mux serve --web --stop --server work  # the bridge that serves session work
```

Stop reads the bridge's own state file, checks the pid still names that bridge, and sends the gentle signal first. If the bridge hangs, stop escalates. It removes the marker file either way.

A bridge that died some other way, such as a kill or a reboot, leaves the marker behind with its token. `fno mux web reap` finds every marker whose port refuses and removes those corpses. Unreadable markers are named and left alone. It prints one receipt line, and `--json` prints rows instead.

## Get the link for one agent

`fno mux view <selector> --url` prints the browser URL for the pane that hosts one agent. The URL carries `?pane=<id>`, so the page paints that agent first.

```bash
fno mux view t-x2270-mergesafety --url    # by name
fno mux view x2270 --url                  # by node id, which the name carries
fno mux view 3f9c1a --url                 # by the start of a session id
```

The resolver tries three tiers and takes the first tier that matches:

1. An exact match on the name, the session id, or the harness session id.
2. A session id or harness session id that starts with the selector.
3. A part of the name, in any letter case.

Ambiguity inside the winning tier refuses. The command exits `21` and prints one line for each candidate: name, pane, and seconds since the last activity. It never guesses, and it never falls through to a looser tier.

This refusal is what makes a short id safe to type. A claude session id is a UUIDv4, so the first eight characters are unlikely to collide. A codex session id is a UUIDv7. Its first eight characters are a clock bucket of about 65 seconds, so two agents from one minute do collide. The first case resolves. The second case refuses and shows you both agents.

Two rows that share one identity are one candidate, not an ambiguity. A pane-hosted row wins over a paneless duplicate.

NOTE: A no-match exits `16`. An agent that hosts no pane exits `17` and prints the follow command instead.

## Reach it from another device

The bridge binds to loopback by default. This default is deliberate. It does not open the port to your network unless you ask.

To reach the view from a phone, use one of these three methods.

- **A private network such as tailscale.** Bind to the private address of the host, not to every interface: `fno mux serve --web --bind <tailscale-ip>`. Then open the printed URL. Only the private network can reach the port, and it authenticates the connection.
- **An SSH tunnel.** Leave the bind address on loopback. Forward the port with `ssh -L 8722:127.0.0.1:8722 <host>`. Then open `http://127.0.0.1:8722/?t=<token>` on the local machine. Keep the token: a URL without it fails the check and shows no pane. The backlog writes keep working over a tunnel, because the bridge itself is still bound to loopback.
- **A reverse proxy you already run.** Terminate TLS at the proxy. The bridge does no TLS of its own.

CAUTION: Do not use `--bind 0.0.0.0`. It opens the port on every interface, including your LAN, where no private network authenticates the caller. The token is the only guard, and the URL carries it. A wide bind also puts the backlog page back into read-only mode: the write routes refuse everything that does not arrive on loopback.

## Share one pane with someone on another network

A teacher can watch a learner's agent from another network with no tailscale and no SSH setup. The share is experimental, so it is off until you turn it on. Open the mux sideline menu and switch on `pane share` under `experimental`.

The quick way: focus the pane and press `prefix X`, or pick `share focused pane` in the menu. The mux starts a tunnelled share, copies the join link, and keeps running it until you pick `stop sharing` or close the mux. Press `prefix X` again to copy the link again. This path needs `cloudflared`.

When a viewer knocks, a popup appears on whatever pane you are on. The cursor starts on deny, so a stray Enter while you type never lets anyone in. Press Up, then Enter, to admit. If you dismiss the popup, the menu shows how many knocks wait.

To run the share by hand, use a terminal you can see, for example a mux pane beside the agent:

```bash
fno mux pane ls                       # find the pane id
fno mux serve --share --pane 3 --tunnel
```

The share prints a join link. Send the whole link to the viewer. The secret is in the part after `#`, so it never goes into a request line or a proxy log. A link without that part opens nothing. The page removes the secret from the address bar once it reads it, so a viewer who reloads the tab opens the join link again.

The flags:

- `--share` starts a share instead of the web view. It needs a terminal, because you admit viewers by typing in it, unless you pass `--managed`.
- `--managed` is for a parent process, such as the mux. It needs no terminal. It prints `link`, `knock`, `gone` and `end` lines on stdout, and it reads `a <n>` and `d <n>` on stdin.
- `--pane <id>` names the one pane to show. It has no default.
- `--tunnel` starts a Cloudflare quick tunnel and prints a public `https://<name>.trycloudflare.com` link. Install `cloudflared` first (macOS: `brew install cloudflared`). Without `--tunnel`, the link works on this machine only. Forward the port with `ssh -R` or your own tunnel to reach another network.
- `--steer` lets viewers send notes to the agent. It is off by default.
- `--port <n>` sets the loopback port. The default is 8723.
- `--server <name>` names the mux session.

The viewer opens the link and types a name. The share prints the knock in your terminal. Type `a <n>` to admit the viewer or `d <n>` to decline. The name is a label you read, not a proof of identity. Your admission is the only check. Type `q`, or close the terminal, to end the share for every viewer.

An admitted viewer can send two kinds of note:

- **Coach** goes to you, the person at the keyboard. It prints in the share terminal. It never reaches the agent and uses no model time.
- **Steer** goes to the agent. It needs a share started with `--steer`. Each steer waits in your terminal until you type `y <n>` to send it or `n <n>` to drop it. A sent steer arrives as mail from `observer:<name>`. Your agent reads it, so it bills your account.

Only two things leave the machine. The first is the cells of the one shared pane, masked. The second is up to 500 lines of that pane's earlier output, masked, sent once to each viewer you admit. Nothing else leaves. The viewer gets no other pane, no agent list, no backlog, and no fleet page.

CAUTION: Masking is best effort. It hides known token shapes: API keys, GitHub and AWS tokens, JWTs and `password=` values. It also hides any run of 40 or more token characters, and the user name in a home path. A secret of an unknown shape, or one that wraps across two rows, can leak. With `--tunnel`, Cloudflare carries the stream and can read the masked text. Quick tunnels are for testing: they have no uptime promise.

## Watch one pane from a terminal

A viewer with ssh access to your machine can watch one pane in a terminal instead of a browser. The viewer runs this on your machine:

```bash
ssh -t your-host fno mux serve --observe --pane 3 --name teacher
```

- `--observe` starts a read-only watch. It needs a terminal, so keep the `-t`.
- `--pane <id>` picks the pane. List panes with `fno mux pane ls`.
- `--name <label>` sets the name you see. The default is the user name and the ssh client address.
- `--server <name>` names the mux session.

The watcher sees the same masked cells the browser share sends. Press `q` or Ctrl-C to leave. No key typed in the watch reaches the pane or the agent. A watch cannot send coach or steer notes. Use the browser share for those.

You see every watcher. The pane border shows a count such as `◉ 2` on its top edge. The sideline lists each watcher with the name, the route (`ssh` or `browser`) and the pane. A browser viewer you admit through the share appears the same way. Both kinds of watcher disappear from your view as soon as they leave.

The ssh login is the only check. Anyone who can run `fno` as your user can already attach to the mux, so the watch adds no new door. The same masking limits apply as for the browser share.

## What you see

The page shows one pane at a time. You choose the pane from a picker at the top. The picker gives each pane its agent name where there is one. It also shows a colored dot for the state of the agent.

The page fits the grid to your screen width by default. A terminal pane is as wide as its terminal, often much wider than a phone. The type becomes smaller until the full width fits. The **fit** button changes the page to full size, where you pan instead. Pinch-zoom works in both modes.

The browser keeps output that leaves the top of the grid. This output stays scrollable above the live grid. It is not terminal scrollback, and the page says so where it appears. When you select a pane, the browser starts with none of it.

The kept output covers only the pane you view. The browser can follow a whole-screen scroll only. A pane that repaints in place keeps less. A pane with a fixed footer deeper than six rows keeps nothing.

## Limits worth knowing before you rely on it

- You cannot type. The read-only paragraph at the start of this page gives the reason.
- There is no history from before you open the page. A frame carries the current visible grid, and the bridge keeps only the latest frame for each pane.
- When an agent exits, its pane leaves the picker. A plain pane has no exit signal on the wire, and no timer removes it. Its entry stays until 128 panes push it out.
- The page loads no external resource. `fno` serves it inline under a strict content-security policy, so it works offline and on an airgapped host.
