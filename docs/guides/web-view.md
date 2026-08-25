# Watch your panes from a browser

`fno mux serve --web` puts a read-only view of a running mux session on an HTTP port. You can open the URL on a phone, a tablet, or another machine, and watch any pane. You install nothing on the device that shows the page.

The view is read-only by construction, not by policy. The bridge sends one attach message upstream, then releases the write half of the socket. No code path can carry a keystroke back to your terminal. The browser drops every inbound message and sends none.

## Start it

```bash
fno mux serve --web                      # the default session, on 127.0.0.1:8722
fno mux serve --web --session work       # a named mux session
fno mux serve --web --port 9000          # a different port
```

The command prints the URL and a token once at bind. The token is in the query string. It is the only guard on the port. Anyone who has the URL can watch the session, so keep the URL secret.

The page's **backlog** link opens the current private `fno backlog view` file through the same token. Render or refresh that file first with `FNO_NO_OPEN=1 fno backlog view`. The `/backlog` response is never cached. A missing file names that command, and a missing or wrong token returns no backlog bytes.

The bridge also writes these values to `<mux dir>/web-<session>.json` at mode 0600. You can get the URL again from this file. The bridge removes the file on exit.

`--session` names the mux SERVER, not an agent. It selects which socket the bridge attaches to. To reach one agent, see the next section.

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
- **An SSH tunnel.** Leave the bind address on loopback. Forward the port with `ssh -L 8722:127.0.0.1:8722 <host>`. Then open `http://127.0.0.1:8722/?t=<token>` on the local machine. Keep the token: a URL without it fails the check and shows no pane.
- **A reverse proxy you already run.** Terminate TLS at the proxy. The bridge does no TLS of its own.

CAUTION: Do not use `--bind 0.0.0.0`. It opens the port on every interface, including your LAN, where no private network authenticates the caller. The token is the only guard, and the URL carries it.

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
