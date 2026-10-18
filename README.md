# muesli

Local AI meeting notes for Linux. muesli notices when you're in a **Zoom** or **Discord**
call (native app or web app), records your mic and the call audio to disk, and when the call
ends it removes speaker echo, trims silence, transcribes via **OpenRouter**, and writes
notes around your own bullets. It never joins the call as a bot.

Built for Omarchy / Hyprland / PipeWire. Rust, GTK4 + libadwaita, an Omarchy bar plugin.

## Install

```bash
./install.sh                     # builds release binaries into ~/.local/bin, enables the user service
muesli-cli set-key sk-or-v1-…    # or export OPENROUTER_API_KEY
```

Then add the bar widget: `{ "id": "ssobhani.muesli" }` in `bar.layout.right` of
`~/.config/omarchy/shell.json` (install.sh links the plugin).

Echo cancellation links the system `webrtc-audio-processing` (≥ 2.1, `pacman -S webrtc-audio-processing`).
Without it, `install.sh` builds WebRTC from source (`--features aec-bundled`, needs meson + ninja).

## Use

Join a call. After ~5 s muesli starts recording (or asks, if auto-record is off). A
notification and the bar widget show the timer; **Stop** / **Discard** are one click away.
Hang up (or Stop) and in a minute or so you get a title suggestion and notes.

```text
muesli-cli status | watch [--json] | toggle | start | stop | discard | record | ignore
muesli-cli auto-record on|off|toggle
muesli-cli list | show <id> | rename <id> "Title" | reprocess <id> | enhance <id> | delete <id>
muesli-cli open [<id>]           # the notes app
muesli-cli detect                # debug: what the call detector sees right now
```

Hyprland keybind example: `bind = SUPER SHIFT, M, exec, muesli-cli toggle`

## Config — `~/.config/muesli/config.toml`

```toml
[openrouter]
stt_models    = ["elevenlabs/scribe-v2", "assemblyai/universal-3-6-pro", "fish-audio/transcribe-1-pro"]
chat_model    = "anthropic/claude-sonnet-5.5"     # titles, Q&A
enhance_model = "anthropic/claude-sonnet-5.5" # notes
zdr           = false                         # true: zero-data-retention providers only

[recording]
auto_record          = true   # false: ask before recording
start_debounce_s     = 5
end_grace_s          = 20
audio_retention_days = 0      # 0 = keep audio forever
echo_cancellation    = true

[apps.discord]
auto_record = false           # per-app override
```

Custom note template: `~/.config/muesli/templates/default.md`.

## How it works

```
PipeWire registry ──► detector ──► session FSM ──► recorder (mic + sink monitor → Ogg Opus, 5-min segments)
     (events)          classify()    auto / ask        │ stop
                                                       ▼
         clean: WebRTC AEC (mic vs speakers) → VAD → speech-only chunks + time maps
                                                       ▼
     OpenRouter STT (fallback chain) → echo backstop → merge → titles → enhanced notes → SQLite
```

- **Detection** is event-driven: a live (uncorked) mic stream from a native Zoom/Discord client,
  or from a browser while a Zoom/Discord window exists (Discord/Zoom web apps).
- **Two channels**: your mic is "You"; the default sink's monitor is everyone else. Speaker
  labels within the call channel come from the STT model and are linked to names by the LLM.
- **Echo**: on speakers, the mic hears the call. The recorded speaker signal is the exact
  reference, so WebRTC's echo canceller removes it offline (measured −26 dB on a real call).
  A text-level backstop drops any "You" line that duplicates the other side.
- **Data** lives in `~/.local/share/muesli/` (`muesli.db` + `meetings/<id>/raw/*.opus`).

## Layout

| crate | what |
|---|---|
| `crates/core` | model, detection rules, session FSM, transcript logic, prompts, SQLite store, IPC protocol, OpenRouter client (pure + tested) |
| `crates/audio` | PipeWire capture, Opus/Ogg, VAD, chunking, echo cancellation |
| `crates/daemon` | `muesli-daemon`: detector, session loop, pipeline, socket server, notifications |
| `crates/cli` | `muesli-cli` |
| `crates/ui` | `muesli`: GTK4/libadwaita notes app |
| `omarchy-plugin/` | Omarchy shell bar widget + panel |

Dev: `MUESLI_HOME=/tmp/muesli-dev` isolates config, data and the socket. `cargo test --workspace`.
