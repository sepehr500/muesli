# ssobhani.muesli — Omarchy bar plugin

Bar widget + panel for [Muesli](../../PLAN.md) (§9). Talks to `muesli-daemon`
only through `muesli-cli`: one long-lived `muesli-cli watch --json` stream
(status / levels / disconnected events), plus one short-lived
`muesli-cli <cmd>` per action. No polling.

| Bar | Meaning |
|---|---|
| dim ridge | listening (very dim: daemon offline) |
| `○ Record?` pill, pulsing | call detected, auto-record off |
| `● mm:ss` pill | recording |
| spinner `n/4` | wrapping up (the meeting is named for you; a "Notes ready" notification follows) |

Left click = panel · right = `muesli-cli toggle` · middle = open notes.
Panel keys: `r` record / `i` ignore (prompt), `a` auto-record, `o` open notes,
Enter = record (prompt), Esc = close.

## Install

```bash
ln -sfn "$PWD/omarchy-plugin/ssobhani.muesli" ~/.config/omarchy/plugins/ssobhani.muesli
omarchy-shell shell rescanPlugins
omarchy plugin enable ssobhani.muesli    # or add {"id":"ssobhani.muesli"} to bar.layout.right
```

Settings (shell.json entry): `showTimer` (bool), `cliPath` (default `muesli-cli`).

## Develop

- `omarchy-plugin/dev/check.sh` loads the QML in a throwaway Quickshell and
  prints load errors + derived state.
- `omarchy-plugin/dev/mock_daemon.py [--state S] [--cycle SECS]` fakes the
  daemon socket; `--set idle|prompting|recording|processing`
  jumps a running mock. It refuses to start if the real daemon answers.
- The plugin dir is a symlink, so the shell's inotify hot-reload doesn't see
  edits, and `rescanPlugins` re-instantiates the stale compiled QML. Use
  `omarchy restart shell` to load changes into the real bar.
