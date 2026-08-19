# L01: Install and prove it

**Medium:** Asciinema cast

**The one thing:** The installed Footnote CLI can identify its build, diagnose its wiring, validate its state paths, update itself, and restart its runtimes.

## Setup state

This is the only lesson recorded before the shared demo state exists. Record it in a throwaway operating-system account because the wizard writes the per-user global config. `FNO_CONFIG` changes config reads but does not redirect wizard writes. Close unrelated terminals, hide notifications, and remove secrets from the shell environment before recording. The `fno` executable must already be on `PATH` so the lesson starts at verification rather than package-manager setup.

## 1. Identify the installed build

```run
fno version
```

```expected
fno 0.3.1 (8f830e634118, release)
```

## 2. Diagnose the active front door

```run
set -o pipefail
fno doctor | sed -n -e '/^fno doctor: mux front door:/s# -> .* (active).# -> ACTIVE (active).#p'
```

```expected
fno doctor: mux front door: `fno` -> ACTIVE (active).
```

## 3. Run the setup wizard

```run
fno setup wizard
```

[capture-at-record]

Choose `/Users/Shared/footnote-recording-demo/state` as the state directory, `/Users/Shared/footnote-recording-demo/plans` as the plans directory, and `demo` as the backlog prefix.

## 4. Prove the resolved paths

```run
set -o pipefail
fno config doctor | sed -n -e '/^\[doctor\] state_dir:/p' -e '/^\[doctor\] OK/p'
```

```expected
[doctor] state_dir: /Users/Shared/footnote-recording-demo/state
[doctor] OK; no suspicious paths detected.
```

## 5. Update the installation

```run
fno update
```

[capture-at-record]

## 6. Restart the runtimes

```run
fno restart
```

[capture-at-record]

## Cut list

- Keep the version and normalized doctor lines uncut.
- Cut between wizard prompts, but keep every chosen value visible.
- Keep the final config proof on screen for three seconds.
- Compress package installation and runtime restart progress after their first receipt lines.

## Record and publish

```run
asciinema rec --cols 120 --rows 36 L01-install-and-prove-it.cast
asciinema upload L01-install-and-prove-it.cast
```

[capture-at-record]
