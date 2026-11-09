#!/usr/bin/env python3
"""muesli → Notion: keep one page per meeting in a Notion database, via the `ntn` CLI.

Install as a muesli hook for these events (one script, symlinked):
    meeting-ready.d/50-notion.py     create or refresh the page when notes are done
    meeting-updated.d/50-notion.py   follow title / speaker renames
    meeting-deleted.d/50-notion.py   move the page to Notion's trash (restorable for 30 days)

Config: ~/.config/muesli/notion.json (or $MUESLI_NOTION_CONFIG)
    {"data_source_id": "<id>", "apps": ["zoom", "manual"]}
`apps` limits which calls are synced ("zoom", "discord", "manual"); omit it to sync all.

The page is matched by its "Muesli ID" property, so re-running is safe: it updates the same
page. muesli is the source of truth: the page body is rewritten when the meeting changes.
Needs `ntn login` (or NOTION_API_TOKEN). Standard library only.
"""

import json
import os
import re
import subprocess
import sys

CONFIG = os.path.expanduser(os.environ.get("MUESLI_NOTION_CONFIG", "~/.config/muesli/notion.json"))
NTN = os.environ.get("NTN", "ntn")
APP_LABELS = {"zoom": "Zoom", "discord": "Discord", None: "Manual"}
MAX_TRANSCRIPT_LINES = 4000  # Notion caps a write at 5,000 blocks


def ntn(path, method, body=None):
    args = [NTN, "api", path, "-X", method] + (["-d", "@-"] if body is not None else [])
    r = subprocess.run(args, input=None if body is None else json.dumps(body), capture_output=True, text=True, timeout=180)
    try:
        out = json.loads(r.stdout) if r.stdout.strip() else {}
    except json.JSONDecodeError:
        out = {}
    if r.returncode != 0 or out.get("object") == "error":
        detail = out.get("message") or r.stderr.strip() or r.stdout.strip()[:300]
        sys.exit(f"ntn {method} {path} failed: {detail}")
    return out


_ESCAPE = re.compile(r"([\\`*_\[\]<>~$|])")


def esc(text):
    """Literal text for Notion-flavored markdown (one line)."""
    return _ESCAPE.sub(r"\\\1", " ".join(str(text).split()))


def clock(secs):
    t = int(max(0, secs or 0))
    return f"{t // 3600}:{t // 60 % 60:02}:{t % 60:02}" if t >= 3600 else f"{t // 60:02}:{t % 60:02}"


def at(item):
    s = item.get("source_start_s")
    return f" `{clock(s)}`" if s is not None else ""


def notes_md(meeting):
    notes = meeting.get("notes") or {}
    out = []
    for sec in notes.get("sections", []):
        out.append(f"## {esc(sec['title'])}")
        for it in sec.get("items", []):
            out.append(f"- {esc(it['text'])}{at(it)}")
            if it.get("detail"):
                out.append(f"\t- {esc(it['detail'])}")
        out.append("")
    todos = notes.get("action_items", [])
    if todos:
        out.append("## Action items")
        for a in todos:
            owner = f" — **{esc(a['owner'])}**" if a.get("owner") else ""
            out.append(f"- [{'x' if a.get('done') else ' '}] {esc(a['text'])}{owner}{at(a)}")
        out.append("")
    if not out and meeting.get("user_notes", "").strip():
        out.append("## Your notes")
        out.extend(f"- {esc(l.lstrip('-*• '))}" for l in meeting["user_notes"].splitlines() if l.strip())
        out.append("")
    return out


def transcript_md(path):
    try:
        lines = json.load(open(path))
    except (OSError, ValueError, TypeError):
        return []
    if not lines:
        return []
    shown = lines[:MAX_TRANSCRIPT_LINES]
    body = [f'<span color="gray">{clock(l["start_s"])}</span> **{esc(l["speaker"])}:** {esc(l["text"])}' for l in shown]
    if len(lines) > len(shown):
        body.append(f"*… {len(lines) - len(shown)} more lines in muesli*")
    return ["<toggle>Transcript", "", "\n\n".join(body), "</toggle>"]


def page_markdown(payload):
    m = payload["meeting"]
    parts = notes_md(m) + transcript_md((payload.get("files") or {}).get("transcript_json"))
    parts += ["", '<span color="gray">Synced from muesli. Edits here are replaced when the meeting changes in muesli.</span>']
    return "\n".join(parts)


def properties(m):
    people = [p for p in m.get("people", []) if p != "You" and not p.startswith("Speaker ")]
    date = {"start": m["started_at"]}
    if m.get("ended_at"):
        date["end"] = m["ended_at"]
    return {
        "Name": {"title": [{"text": {"content": m["title"][:2000]}}]},
        "Date": {"date": date},
        "App": {"select": {"name": APP_LABELS.get(m.get("app"), "Manual")}},
        "Duration (min)": {"number": round((m.get("duration_s") or 0) / 60)},
        "People": {"multi_select": [{"name": p.replace(",", " ")[:100]} for p in dict.fromkeys(people)]},
        "To-dos": {"number": (m.get("todos") or {}).get("open", 0)},
        "Muesli ID": {"rich_text": [{"text": {"content": m["id"]}}]},
    }


def find_page(ds, mid):
    r = ntn(f"v1/data_sources/{ds}/query", "POST",
            {"filter": {"property": "Muesli ID", "rich_text": {"equals": mid}}, "page_size": 1})
    return (r.get("results") or [None])[0]


def main():
    payload = json.load(sys.stdin)
    event, m = payload["event"], payload["meeting"]
    try:
        cfg = json.load(open(CONFIG))
    except (OSError, ValueError) as e:
        sys.exit(f"no Notion config at {CONFIG}: {e}")
    ds = cfg["data_source_id"]
    app = m.get("app") or "manual"
    if "apps" in cfg and app not in cfg["apps"]:
        print(f"skipped: {app} calls aren't synced (apps = {cfg['apps']})")
        return

    page = find_page(ds, m["id"])
    if event == "meeting-deleted":
        if page:
            ntn(f"v1/pages/{page['id']}", "PATCH", {"in_trash": True})
            print(f"moved to Notion trash: {page.get('url')}")
        return
    if m.get("state") != "ready":
        print(f"skipped: meeting is {m.get('state')} (synced once notes are ready)")
        return

    md = page_markdown(payload)
    if page:
        ntn(f"v1/pages/{page['id']}", "PATCH", {"properties": properties(m)})
        ntn(f"v1/pages/{page['id']}/markdown", "PATCH",
            {"type": "replace_content", "replace_content": {"new_str": md}, "markdown_version": "v2"})
        print(f"updated {page.get('url')}")
    else:
        page = ntn("v1/pages", "POST", {
            "parent": {"type": "data_source_id", "data_source_id": ds},
            "properties": properties(m),
            "markdown": md,
            "markdown_version": "v2",
        })
        print(f"created {page.get('url')}")


if __name__ == "__main__":
    main()
