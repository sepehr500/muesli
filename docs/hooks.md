# Hooks

Run your own scripts when something happens to a meeting: push notes to Notion, copy them
into a notes vault, upload audio, post to Slack.

Put an executable file in the folder named after the event:

```
~/.config/muesli/hooks/
  meeting-ready.d/
    50-notion.py
  recording-started.d/
    10-slack-status.sh
```

No registration: muesli runs every executable in the folder, in name order. Files that are
hidden or end in `~`, `.sample`, `.disabled`, `.bak`, `.swp` or `.orig` are skipped, so
renaming a script to `x.disabled` turns it off.

## Events

| Event | When |
|---|---|
| `recording-started` | a recording starts |
| `recording-stopped` | a recording ends, before processing |
| `meeting-ready` | transcript and notes are done (again after Re-enhance) |
| `meeting-updated` | the title or a speaker's name changes |
| `meeting-failed` | processing gave up |
| `meeting-deleted` | you delete a meeting (its files are already gone) |

## What a script gets

The full payload is JSON on **stdin**:

```json
{
  "version": 1,
  "event": "meeting-ready",
  "event_id": "ev_01m6…",
  "meeting": {
    "id": "01example0000000000000000", "title": "Q4 roadmap with Lena",
    "app": "zoom", "hint": null, "state": "ready",
    "started_at": "2026-10-18T10:30:00-04:00", "ended_at": "2026-10-18T11:12:00-04:00",
    "duration_s": 2520, "people": ["You", "Lena", "Marco", "Priya"],
    "todos": {"total": 3, "open": 3}, "cost_usd": 0.13, "error": null,
    "notes": {
      "sections": [{"title": "Decisions", "items": [
        {"text": "Ship offline mode before Christmas", "detail": "Lena wants it in the Dec 12 release.",
         "from_user": true, "source_start_s": 10.0}]}],
      "action_items": [{"text": "Draft January pricing options", "owner": "Priya", "done": false,
                        "from_user": false, "source_start_s": 61.0}]
    },
    "user_notes": "- what you typed"
  },
  "files": {
    "dir": "…/meetings/<id>/export",
    "notes_md": "…/export/notes.md",
    "transcript_md": "…/export/transcript.md",
    "transcript_json": "…/export/transcript.json",
    "meeting_json": "…/export/meeting.json"
  }
}
```

`files` is `null` for `recording-started` and `meeting-deleted`. Shell scripts can use the
environment instead: `MUESLI_EVENT`, `MUESLI_EVENT_ID`, `MUESLI_MEETING_ID`, `MUESLI_TITLE`,
`MUESLI_APP`, `MUESLI_STARTED_AT`, `MUESLI_DURATION_S`, `MUESLI_EXPORT_DIR`,
`MUESLI_NOTES_MD`, `MUESLI_TRANSCRIPT_MD`. The working directory is the export folder.

```bash
#!/bin/sh
cp "$MUESLI_NOTES_MD" ~/vault/Meetings/"$(date +%F) $MUESLI_TITLE.md"
```

## Delivery

- Events are saved in muesli's database before any script runs, so a crash or reboot can't
  lose one; unfinished runs continue when the service starts again.
- Scripts run one at a time in the background with a 5-minute limit. They never delay
  recording or processing.
- A failed run (non-zero exit or timeout) is retried after 1 minute, then after 10. After
  that a notification offers Retry. Use `event_id` if your script must not act twice.
- The OpenRouter key isn't passed to scripts.
- stdout and stderr are kept in the run log (last 4,000 characters).

## CLI

```text
muesli-cli hooks list                      scripts found per event
muesli-cli hooks run <event> <meeting-id>  run now on any meeting and print the result
muesli-cli hooks log [<meeting-id>] [-v]   recent runs and their output
muesli-cli hooks retry <event-id>          queue an event's failed runs again
```

`hooks run` is the way to develop a script: write it, then try it on yesterday's meeting.

## Example: Notion

[`hooks/notion-sync.py`](../hooks/notion-sync.py) keeps one page per meeting in a Notion
database using Notion's official CLI, [`ntn`](https://www.npmjs.com/package/ntn). Each page
has Date, App, Duration, People and open To-dos columns, the notes with source timestamps,
and the transcript in a collapsed toggle. Renames update the page; deleting the meeting
moves the page to Notion's trash.

1. `ntn login`
2. Create a database with these properties: Name (title), Date (date), App (select),
   Duration (min) (number), People (multi-select), To-dos (number), Muesli ID (text).
3. Find its data source ID: `ntn datasources resolve <database-id>`.
4. Link it for the three events. `install.sh` puts bundled scripts in
   `~/.local/share/muesli/hooks/`, so linking there keeps you on the latest version:

```bash
H=~/.config/muesli/hooks
mkdir -p $H/meeting-ready.d $H/meeting-updated.d $H/meeting-deleted.d
for e in meeting-ready meeting-updated meeting-deleted; do
  ln -sfn ~/.local/share/muesli/hooks/notion-sync.py $H/$e.d/50-notion.py
done
echo '{"data_source_id": "<id>"}' > ~/.config/muesli/notion.json
```

Add `"apps": ["zoom"]` to `notion.json` to sync only some kinds of call (`zoom`, `discord`,
`manual`). To send meetings you already have:

```bash
for id in $(muesli-cli list --limit 1000 --json | jq -r '.[] | select(.state == "ready") | .id'); do muesli-cli hooks run meeting-ready $id; done
```
