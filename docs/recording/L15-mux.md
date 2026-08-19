# L15: Mux

**Medium:** Asciinema cast

**The one thing:** Mux keeps a server-owned shell alive as a pane that scripts can list, write, read, and reattach to.

## Setup state

Run the shared setup in [README.md](README.md). Use two terminals: one for the named server and one for the scriptable client commands.

## 1. Prove the shell-integration snippet

```run
fno mux shell-init zsh --json | jq -c '{shell,has_preexec:(.snippet|contains("preexec"))}'
```

```expected
{"shell":"zsh","has_preexec":true}
```

## 2. Start the named server

In the first terminal, run this command and leave it open.

```run
fno mux server --session recording15
```

[capture-at-record]

## 3. List the live session

```run
fno mux ls --json | jq -c '.[] | select(.session == "recording15") | {session,state,stale}'
```

```expected
{"session":"recording15","state":"live","stale":false}
```

## 4. Create an interactive shell pane

```run
set -o pipefail
fno mux pane run --session recording15 --json -- zsh -f -i | tee "$DEMO_ROOT/l15-pane.json" | jq -c '{created:(.pane_id != null)}'
PANE_ID="$(jq -r .pane_id "$DEMO_ROOT/l15-pane.json")"
```

```expected
{"created":true}
```

## 5. Send a command and read its completed block

```run
fno mux pane send "$PANE_ID" --session recording15 --text 'eval "$(fno mux shell-init zsh)"' --json
fno mux pane send "$PANE_ID" --session recording15 --text $'\r' --json
sleep 1
fno mux pane send "$PANE_ID" --session recording15 --text 'printf "mux-ready\\n"' --json
fno mux pane send "$PANE_ID" --session recording15 --text $'\r' --json
sleep 1
set -o pipefail
fno mux pane read "$PANE_ID" --session recording15 --block last --json | jq -r '.text | gsub("\r"; "") | split("\n") | map(select(test("[^ ]"))) | .[0]'
```

```expected
{"ok":true}
{"ok":true}
{"ok":true}
{"ok":true}
mux-ready
```

## 6. Attach the interactive client

```run
fno mux attach recording15
```

[capture-at-record]

Exit the client with its detach key, then stop the server terminal after the cast file closes. The shell pane must remain visible after client detach and until server shutdown.

## Cut list

- Keep the shell-integration proof and live-session row uncut.
- Keep the pane creation receipt, four send receipts, and read-back value at normal speed.
- Compress no attach transition. The persistent pane is the lesson.
- Keep one detach and reattach cycle visible before server shutdown.

## Record and publish

```run
asciinema rec --cols 120 --rows 36 L15-mux.cast
asciinema upload L15-mux.cast
```

[capture-at-record]
