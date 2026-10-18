# Muesli — local AI meeting notes for Linux

A local, lightweight Rust app. It notices when you're in a Zoom or Discord call, records it to disk, and when the call ends turns your rough notes plus the transcript into clean notes, using OpenRouter. It never joins the call as a bot.

Target system: Arch/Omarchy, Wayland (Hyprland), PipeWire, Quickshell-based `omarchy-shell`.

---

## 0. Implementation notes (v0.1)

- **Discord on this machine is the Chrome web app.** The mic stream belongs to Chrome's audio service (`application.process.binary=chrome`), whose parent owns a window with class `chrome-discord.com__…`. Detection therefore also accepts "browser has a live, uncorked mic stream **and** a Zoom/Discord window exists". The `pw-dump` + `hyprctl` fixtures live in `crates/core/tests/fixtures/`.
- **Echo, measured on a real call (speakers):** the mic picks up the speakers about 28 ms late, with correlation 0.42. After offline WebRTC AEC3 it drops to 0.04 (−26 dB), and false "you" speech falls from 17.3 s to 0 s in a 20 s clip.
- **fish-audio/transcribe-1-pro:**
  - Accepts Ogg Opus.
  - About 30× realtime.
  - Costs $0.006/min.
  - Inline speaker markers look like `<|speaker:0|>`, with word timestamps in `verbose_json`.
- **API key:** stored in `OPENROUTER_API_KEY` or `~/.config/muesli/openrouter.key` (mode 600), instead of the Secret Service. This avoids a D-Bus dependency.
- **Notifications:** sent with `notify-send` (libnotify 0.8 actions), not `notify-rust`.
- **IPC:** a Unix socket with JSON lines (`crates/core/src/ipc.rs`). There's no D-Bus.
- **Opus:** links the system libopus through `opusic-sys`. WebRTC APM uses the system `webrtc-audio-processing-2`, or builds from source with `--features aec-bundled`.

## 1. Decisions so far

| Topic | Decision |
|---|---|
| Language | Rust |
| LLM + STT provider | OpenRouter (one API key) |
| Transcription | **After the meeting**, no live transcript |
| Default STT model | `fish-audio/transcribe-1-pro` (meeting-tuned, inline speaker labels, $0.006/min). Fallbacks: `elevenlabs/scribe-v2` → `openai/whisper-large-v3-turbo` |
| Audio | **Always recorded to disk**, crash-safe, two channels (Me / Them) |
| Echo | Must handle it, because you use speakers. Run WebRTC AEC offline on the saved audio |
| Auto-record | Setting `auto_record = true/false` (details below) |
| Shell integration | Omarchy shell plugin (bar widget + panel) |
| Main UI | GTK4 + libadwaita |
| Visual identity | Swiss Alps / *Sound of Music* meadows, with Swiss typographic style. Not a Granola clone |
| Calendar | Not yet |

## 2. First principles

- **Detecting a meeting:** a call app opens a **mic capture stream** when you join. PipeWire announces every stream as a node with `application.process.binary` and `media.class`, through events, so the app doesn't need to poll.
- **Telling "me" from "them":** we capture the default **source** (your mic) and the **monitor of the default sink** (everything you hear) separately. The channel already says who is speaking.
- **Echo:** with speakers, mic = you + (room response ∗ what the speakers played) + noise. We record *exactly* what the speakers played (the sink monitor), so this is textbook acoustic echo cancellation: estimate the room response and subtract it. Both streams run on the same PipeWire graph clock, so they stay aligned and drift isn't a problem.
- **Cost and latency:** STT is billed per audio second. Each channel is mostly silence, so stripping silence before upload roughly halves the cost.
- **Lightweight:** during a meeting the only work is capture → encode → write. Everything heavy happens once, after the call.

## 3. Architecture

```
┌──────────────────────── muesli-daemon (systemd --user) ────────────────────────┐
│                                                                                 │
│  detector ── MeetingDetected/Ended ──► session FSM ──► recorder                 │
│  (PipeWire registry + hyprctl hints)   (auto/ask)      (mic + sink monitor →    │
│                                                         Ogg/Opus segments)      │
│                                              │                                  │
│                                   on end ──► post-pipeline job queue            │
│                                              AEC → VAD trim → split → STT       │
│                                              (fallback chain) → merge → LLM     │
│                                              (title + enhance) → SQLite         │
│                                                                                 │
│  IPC: Unix socket, JSON lines  ($XDG_RUNTIME_DIR/muesli.sock)                   │
│       commands: status, start, stop, discard, confirm, rename, list ...         │
│       events:   detected, recording, stopped, processing{stage}, ready, error   │
└───────────▲──────────────────────────▲─────────────────────────▲────────────────┘
            │                          │                         │
   muesli (GTK4 notes app)     muesli-cli (+ `watch`)    omarchy plugin (QML)
                                                         runs `muesli-cli watch --json`
                                                         as a long-lived Process
```

- **Two processes.** The daemon must keep running with no window open. Target is under 15 MB RSS and about 0% CPU while idle. The GTK app costs memory only while it's open.
- **IPC is a Unix socket with JSON lines**, not D-Bus. Quickshell can consume it easily by running `muesli-cli watch --json` as a `Process` and parsing stdout line by line (the same pattern as your `ai-wallpapers` plugin), which keeps everything push-based. The CLI and GTK app use the same protocol.

### Cargo workspace

```
muesli/
  crates/
    core/        # domain types, config, session FSM, storage (rusqlite + FTS5), prompts
    detect/      # PipeWire node watcher, app rules, hyprctl hints  (pure classify())
    capture/     # pipewire-rs capture → Opus segment writer
    dsp/         # decode, AEC (webrtc-audio-processing), VAD, trim + time map
    stt/         # OpenRouter STT client, fallback chain, response normalisation
    llm/         # OpenRouter chat (streaming), title + enhance
    pipeline/    # post-meeting job: dsp → stt → merge → llm, resumable per stage
    daemon/      # bin: muesli-daemon
    cli/         # bin: muesli-cli
    ui/          # bin: muesli (GTK4/libadwaita, relm4)
  omarchy-plugin/  # <user>.muesli: manifest.json, BarWidget/Panel QML
  packaging/       # PKGBUILD, systemd user unit, .desktop, icons
```

Functional core, imperative shell: `classify`, the session FSM, VAD trim and time map, chunk planning, transcript merge, and prompt building are pure functions over plain data. They're tested with recorded fixtures. Only the shell does I/O.

## 4. Meeting detection and auto-record

Pure rule: `classify(nodes, windows) -> Option<Candidate { app, hint }>`

| App | Signal | Name hint |
|---|---|---|
| Zoom (native) | `Stream/Input/Audio` node from `zoom` | Window title / meeting topic |
| Discord / Vesktop | Input stream from `Discord`, `vesktop`, Electron (Discord) / `WEBRTC VoiceEngine` | `#channel \| Server - Discord` → "Server · #channel" |
| Later | Browser (Meet, Zoom web) | Tab title |

- **Start debounce:** the stream has to stay open for at least 5 s. This filters out mic tests and Discord's input-sensitivity preview.
- **End:** the stream has been gone for at least 20 s, or you press Stop.

Session FSM, with `auto_record` read from config:

```
Idle ─detect─► auto_record=true  ─► Recording ──(notification + bar: [Stop] [Discard])
             └ auto_record=false ─► Prompting ──(notification + bar: [Record] [Ignore])
                                     └─ Record ─► Recording
                                     └─ Ignore / call ends ─► Idle
Recording ─end/Stop─► Processing ─► Ready (name it)
```

There's an optional per-app override in config (`[apps.discord] auto_record = false`). Manual start/stop is available through `muesli-cli` and can be bound in Hyprland.

## 5. Recording to disk

- `~/.local/share/muesli/meetings/<id>/raw/{mic,sys}-NNN.opus`. These are rolling **5-min Ogg/Opus segments**, mono, 48 kHz capture, about 32 kbps (around 15 MB per channel-hour).
- Ogg pages are self-delimiting, so a crash loses at most the last second. Each segment is fsynced when it's closed.
- Every segment carries a start timestamp from the PipeWire graph clock, so mic and sys line up exactly. AEC depends on this.
- `meeting.json` holds metadata (app, hints, segment list, state), so an interrupted pipeline can resume.
- **Retention:** keep audio by default. Optional `audio_retention_days` setting.

## 6. Post-meeting pipeline (resumable, one stage at a time)

1. **Decode** the segments to 16 kHz mono f32. This is streamed, so there's never a full hour in RAM.
2. **AEC** on mic, using sys as the far-end reference, with `webrtc-audio-processing` 2.x (it links the system `webrtc-audio-processing-1`, already installed with PipeWire). Out comes `mic_clean`.
3. **VAD trim** on `mic_clean` and on sys. Speech-only spans are kept with a **time map**, so trimmed time maps back to meeting time.
4. **Split** each channel's speech into requests of about 10 min (or below the size limit), cutting only in pauses.
5. **STT** with a fallback chain: `fish-audio/transcribe-1-pro` → `elevenlabs/scribe-v2` → `openai/whisper-large-v3-turbo`. A timeout or 5xx retries once, then falls back to the next model. The model used is recorded for each piece.
6. **Normalise and merge:**
   - Mic text is always speaker = **Me**.
   - Sys text uses the model's speaker labels as **Them A/B/C**.
   - Labels are lined up across pieces in one cheap LLM pass ("Them B is Sarah").
   - The two channels are interleaved by mapped timestamps.
7. **Echo backstop:** drop any Me segment whose text fuzzily matches a Them segment within ±2 s. This catches leftover echo that AEC missed.
8. **LLM:** suggest a title and enhance the notes.
9. Mark the meeting ready and send a notification: "Notes ready — name this meeting".

The cost for an hour-long call with silence trimmed is roughly $0.20–0.40.

**Optional later (`eager_transcribe`):** run steps 1–7 on each finished 5-min segment during the meeting, with nothing shown in the UI, so only the last segment is left at the end.

## 7. LLM features (OpenRouter)

```toml
[openrouter]
base_url      = "https://openrouter.ai/api/v1"
stt_models    = ["fish-audio/transcribe-1-pro", "elevenlabs/scribe-v2", "openai/whisper-large-v3-turbo"]
chat_model    = "google/gemini-2.5-flash"      # titles, speaker linking, Q&A
enhance_model = "anthropic/claude-sonnet-4.5"  # note enhancement
zdr           = true                           # zero-data-retention routing only

[recording]
auto_record          = true
audio_retention_days = 0     # 0 = keep forever
```

- **Enhance notes:** your rough notes plus the speaker-labelled transcript go in. Out come structured markdown notes that keep your bullets as the skeleton and fill them in. AI-added text is marked so the UI can render it differently.
- **Title:** three suggestions built from the transcript plus app/window hints.
- **Templates:** markdown prompt files in `~/.config/muesli/templates/` (1:1, standup, interview, customer call).
- **Ask this meeting:** chat with the transcript in context. Ask across all meetings comes later (FTS5 retrieval).
- The OpenRouter key is stored in the Secret Service keyring (`oo7`/`keyring`), with an `OPENROUTER_API_KEY` env fallback.

## 8. Naming flow

1. The call ends. The bar shows "Processing…", then a notification says "Notes ready — name this meeting".
2. The title field is pre-filled with the best suggestion, with alternatives shown as chips. The offline fallback is `"{App} call · {Mon DD, HH:MM}"`, plus the window hint (for example "Acme · #standup").
3. You can rename at any time from the app, the CLI, or the bar panel.

## 9. Omarchy plugin (`<user>.muesli`)

A `bar-widget` plugin with `Panel.qml` (the same contract as `ssobhani.ai-wallpapers`), plus a menu extension entry.

- **Bar icon states:**
  - idle (dim mountain glyph)
  - prompting (pulsing)
  - recording (red dot + `mm:ss`)
  - processing (spinner)
  - ready (dot badge)
- **Panel:** the current state with **Record / Ignore** or **Stop / Discard**, a title field after the call, the last 5 meetings (click one to open it in the GTK app), and an auto-record toggle.
- **Source of truth:** `muesli-cli watch --json` events; actions go through `muesli-cli <cmd>`. No polling.
- **Menu:** `~/.config/omarchy/extensions/omarchy-menu.jsonc` gets "Muesli → Start/Stop recording, Open notes".
- Colours in the bar follow the Omarchy theme, so it fits whatever theme is active. The main app keeps its own alpine identity.

## 10. Visual design: "Alpine meadow, Swiss precision"

Mood: the opening of *The Sound of Music*. Bright high-altitude air, rolling meadow, snowy peaks, wildflowers. It's set in a **Swiss International Typographic Style** layout: strict grid, flush-left text, sans type, lots of air.

- **Palette:**

  | Role | Name | Hex |
  |---|---|---|
  | Background | Snow | `#FFFFFF` |
  | Sidebar | Glacier | `#F1F4F6` |
  | Hairlines | | `#E3E8EC` |
  | Ink | Granite | `#1B2024` |
  | Muted / AI text | Slate | `#5B6770` |
  | Primary accent | Gentian blue (*Enzian*) | `#2147A6` |
  | Illustration only | Rock | `#C9D5DE` |
  | Illustration only | Meadow | `#A9BE84` / `#93AA6E` |
  | Recording only | Swiss red | `#E2231A` |

  Mockups: Paper → Scratchpad → page "Muesli".
  The Omarchy plugin does **not** use this palette. It inherits the active Omarchy theme (`~/.local/state/omarchy/current/theme/colors.toml`).


  A dark mode, "Alpenglow at dusk": deep slate-blue background, snow ink, gentian accent warmed slightly.
- **Type:**
  - Titles: Inter Tight 800, 52/54, −0.035em.
  - Body: Inter 15/24. Your own text is granite 500; AI additions are slate 400 on their own line.
  - Section labels: Inter 600, 11 px, uppercase, +0.08em.
  - Timestamps and model names: JetBrains Mono 12.
- **Layout:**
  - Sidebar with meetings grouped by day.
  - Centred editor column about 700 px wide, on an 8 px grid.
  - Transcript in a collapsible drawer.
- **Signature details:**
  - A thin line-art ridge silhouette (a stylised Matterhorn) as the empty-state art and app icon.
  - A soft sky-to-snow gradient in the header area only.
  - Small flower glyphs (edelweiss and gentian) as template icons.
- **Restraint:** flat surfaces, hairline dividers, no heavy shadows, 150 ms transitions.
- Implemented with libadwaita's StyleManager plus a single `style.css`.

## 11. Storage

SQLite at `~/.local/share/muesli/muesli.db`:

- `meetings(id, title, app, hint, started_at, ended_at, state, template, audio_dir)`
- `segments(id, meeting_id, speaker, t_start, t_end, text, stt_model)` + FTS5
- `notes(meeting_id, user_md, enhanced_md, updated_at)`
- `chats(meeting_id, role, content, created_at)`

Optional markdown export per meeting, Obsidian-friendly.

## 12. Key crates

`tokio`, `pipewire`, `opus` + `ogg`, `rubato`, `webrtc-audio-processing` (2.x, system lib), `webrtc-vad` (start here; Silero via `ort` if quality needs it), `reqwest` (rustls) + `eventsource-stream`, `rusqlite` (bundled, FTS5), `serde` + `toml`, `oo7`, `notify-rust`, `strsim` (echo backstop), `gtk4` + `libadwaita` + `relm4`.

## 13. Roadmap

**Phase 0 — Spikes (1–2 days)**
- Install the toolchain: `mise use -g rust`, `gtk4`, `libadwaita`, `opus`.
- `pw-dump` fixtures:
  - idle
  - Zoom in a meeting
  - Discord in voice
  - Discord mic test
  - Discord after leaving
- A throwaway recorder for mic and sys on speakers, with an offline AEC pass. Measure how much echo is left.
- Send a 10-min trimmed file to each STT model. Measure latency, timeout behaviour, and speaker-label quality.

**Phase 1 — Detect and record (headless)**
- Detector, session FSM with `auto_record`, Opus segment recorder, socket IPC, CLI, notifications, systemd unit.
- *Exit criterion:* join a Discord call on speakers, and both channels land on disk without you touching anything. With `auto_record=false`, it waits for Record.

**Phase 2 — Omarchy plugin**
- Bar widget and panel driven by `muesli-cli watch --json`, plus the menu entry.
- *Exit criterion:* the full record/stop/discard/prompt loop works from the bar.

**Phase 3 — Post-meeting pipeline**
- AEC → VAD trim → split → STT fallback chain → merge → echo backstop. Resumable stages.
- *Exit criterion:* a labelled, echo-free transcript within a few minutes of hanging up.

**Phase 4 — LLM**
- Title suggestions, speaker linking, enhance notes, templates. `muesli-cli enhance <id>`.

**Phase 5 — GTK app**
- Meeting list, notes editor (usable during the call), transcript drawer, naming flow, chat, settings.

**Phase 6 — Polish and ship**
- Search, export, retention, `eager_transcribe`, AUR `PKGBUILD`, release profile (`lto`, `opt-level="s"`, strip).

**Later:** calendar, browser meetings, local whisper offline mode, Ollama provider.

## 14. Budgets

| Metric | Target |
|---|---|
| Daemon idle | < 15 MB RSS, ~0% CPU |
| Daemon while recording | < 2% of one core, < 30 MB |
| Post-processing, 1 h call | AEC + VAD in < 30 s CPU; total wall time dominated by STT |
| GTK app open | < 80 MB |
| Binaries | daemon < 10 MB, UI < 15 MB |

## 15. Risks and open questions

- **Discord stream lifetime:** Discord may keep the mic open after you leave voice. The Phase 0 fixtures decide this.
- **AEC on laptop speakers:** loud playback distorts the echo in ways the canceller can't model fully. The text-level backstop covers what's left. Measure it in Phase 0.
- **The 60 s STT processing timeout:** the request size may need to be smaller than 10 min. The fallback chain covers failures.
- **Speaker-label consistency** across pieces from fish-audio. Bigger pieces plus LLM linking should handle it; validate in Phase 0.
- **Consent:** recording laws vary. Keep a clear recording indicator.
