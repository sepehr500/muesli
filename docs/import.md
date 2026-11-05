# Importing external recordings

For recorders outside muesli, e.g. a Discord bot that records each speaker on a separate track.
An import becomes a normal meeting: cleaning, playback, transcript, title and notes all work
as usual. It never touches live desktop capture, so imports can run while muesli is recording.

## Command

```bash
muesli-cli import --manifest /abs/path/import.json --json
```

The command returns once the audio has been **copied into muesli's storage and fsynced**, and
the meeting row is committed. After that, the caller can delete its files. Format conversion,
cleaning, transcription and notes run later in the daemon (`muesli-cli list` / `show <id>`).

| Exit | stdout (`--json`) | Meaning |
|---|---|---|
| 0 | `{"id":"01…","state":"processing","duplicate":false}` | Stored. |
| 0 | `{"id":"01…","state":"<current>","duplicate":true}` | This `source_id` was already imported. Nothing new was stored. |
| 2 | `{"error":"invalid import: …","retryable":false}` | Bad manifest or audio. Retrying won't help. |
| 1 | `{"error":"…","retryable":true}` | Daemon not running, disk full, etc. Retry later. |

`state` can be `processing`, `ready` or `failed`. Socket API (one JSON line):
`{"cmd":"import","manifest":"/abs/import.json"}` replies `{"ok":true,"data":{id,state,duplicate}}`.

## Manifest (version 1)

```json
{
  "version": 1,
  "source_id": "discord:<guild>:<session uuid>",
  "app": "discord",
  "hint": "Channel name",
  "started_at": 1760000000000,
  "ended_at": 1760003600000,
  "tracks": [
    { "name": "Matt Rosenthal", "path": "/abs/session/matt.wav" },
    { "name": "Lena",           "path": "/abs/session/lena.wav" }
  ]
}
```

- `source_id`: 1–200 characters from `A-Z a-z 0-9 - _ . :`. Discord IDs must start with `discord:`.
  This is the idempotency key, so retries and caller restarts never create a duplicate meeting.
  The first successful import wins; later manifests with the same id are ignored, even if
  they differ. If the meeting is deleted in muesli, the id is free again.
- `app`: `"discord"` or `"zoom"`. Optional.
- `hint`: up to 200 characters. Optional. Helps with naming the meeting.
- `started_at` / `ended_at`: Unix **milliseconds**. `started_at` is meeting time zero.
  `ended_at` must be after `started_at`, with a duration of at most 12 h.
- `tracks`: 1–64 tracks.
  - `path`: absolute. No `.` or `..` components. Must end in `.wav` and must not be listed
    twice. After resolving symlinks it must be a regular file **inside the manifest's
    directory** (subfolders are fine).
  - The file must be RIFF WAV, **16 kHz, mono, 16-bit PCM**, aligned/padded to meeting time
    zero. It can be at most 30 s longer than `ended_at − started_at`. Shorter tracks are
    padded with silence.
  - `name`: the speaker's display name (≤ 100 chars; `"You"` is reserved). **Tracks with the
    same name are one speaker.** For resumed sessions, give each segment file the same name;
    the files are summed onto one timeline. Two different people need distinct names.
    If you leave `name` out, the track is treated as mixed (several voices). It is diarized,
    and the notes model names its speakers, as with a normal call.
- Unknown fields are rejected.

## What happens to the names

Each named speaker gets a stable label (`t0`, `t1`, …, in first-appearance order), and the
name is applied to that label as-is. The notes model sees the names, is told never to
rename them, and only names speakers that weren't given. The label-to-name mapping is stored
in the database (`imports` table), so it survives reprocessing, audio retention pruning and
app restarts. Renaming a speaker in the app or with `muesli-cli rename-speaker` is kept across
`reprocess` and `enhance`. Resetting a name (renaming it to empty) brings back the
manifest's name.

The recording has no "You" channel, so transcripts show only names, and step details say
"3 speakers" instead of "You and 2 others". If the muesli user is in the call, they appear
under their own track name.

## Storage

The meeting is created under `~/.local/share/muesli/meetings/<id>/` (the id is derived from
`source_id` and `started_at`):

- `raw/import/` holds the copied WAVs until the pipeline's first step converts them into
  60 s Ogg Opus segments per speaker (`raw/t0-000.opus`, … plus `raw/recording.json`). Then
  the copies are deleted. Conversion streams one segment at a time, so it is safe to rerun
  after a crash.
- Cleaning skips echo cancellation, since each track is one person's own stream. It runs
  voice detection and chunking on each track, and writes the playback mix (level-matched sum
  of all tracks) to `playback/`.
- Memory is bounded by one 60 s segment per track (about 4 MB per track) during cleaning and
  one segment during conversion. It does not depend on meeting length.
