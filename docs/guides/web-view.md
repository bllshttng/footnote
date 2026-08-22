# Watch your panes from a browser

`fno mux serve --web` puts a read-only view of a running mux session on an HTTP port. Open the URL on a phone, a tablet, or another machine and watch any pane. Nothing to install on the viewing device.

It is read-only by construction, not by policy. The bridge sends one attach message upstream, then releases the socket's write half. No code path can carry a keystroke back to your terminal. The browser drops every inbound message and sends none.

## Start it

```bash
fno mux serve --web                      # the default session, on 127.0.0.1:8722
fno mux serve --web --session work       # a named session
fno mux serve --web --port 9000          # a different port
```

It prints the URL and a token once at bind. The token is in the query string, and it is the only guard on the port. Anyone holding the URL can watch the session, so treat it like a password.

The same values are written to `~/.fno/mux/web-<session>.json` at mode 0600, so you can recover the URL without restarting. The bridge removes the file on exit.

## Reach it from another device

The bridge binds loopback by default. That is deliberate: it never widens your attack surface unless you ask.

To reach it from a phone, pick one of these.

- **A private network such as tailscale.** Run `fno mux serve --web --bind 0.0.0.0` and open the URL at the host's private address. This is the simplest option, and the private network does the authentication.
- **An SSH tunnel.** Leave the bind on loopback and forward the port: `ssh -L 8722:127.0.0.1:8722 <host>`. Then open `http://127.0.0.1:8722` on the forwarding machine.
- **A reverse proxy you already run.** Terminate TLS there. The bridge does no TLS of its own.

Do not put `--bind 0.0.0.0` on a public interface. The token is the only guard, and it travels in the URL.

## What you see

One pane at a time, chosen from a picker at the top. The picker labels each pane with its agent name where there is one, and shows a coloured dot for the agent's state.

The page fits the grid to your screen width by default. A terminal pane is as wide as its terminal, often far wider than a phone. The type scales down until the whole width fits. Tap **fit** to switch to full size and pan instead. Pinch-zoom works in both modes.

Output that scrolls past is kept in the browser and stays scrollable above the live grid. This is not terminal scrollback and the page says so where it appears. Select a pane and it starts empty there. It covers only the pane you are viewing, and it can only follow a whole-screen scroll. A pane that repaints in place, or one that pins a footer below a scrolling region deeper than a few rows, keeps less or nothing.

## Limits worth knowing before you rely on it

- You cannot type. See the read-only note above.
- There is no history from before you opened the page. A frame carries the current visible grid, and the bridge caches only the latest one per pane.
- An agent pane leaves the picker on death. A plain pane has no death signal on the wire, so it ages out of the list instead.
- The page loads no external resource of any kind. It is served inline under a strict content security policy, so it works offline and on an airgapped host.
