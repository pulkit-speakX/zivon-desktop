# Claude → Envoy session importer (POC)

A standalone local GUI that finds your local **Claude Code** sessions, lets you
pick which to import (all selected by default), writes them into Envoy as native
chat sessions, and gives you a link to **open and continue** each one in Envoy.

> **POC scope.** This is a throwaway tool to prove the concept. It writes
> **directly to the dev Mongo** (reads the URI from `zivon-core/backend/.env`).
> The productized path will be a thin authenticated endpoint the desktop app
> calls with your JWT — this script never ships inside the app.

## What it does

1. **Discovers** every session under `~/.claude/projects/*/*.jsonl`.
2. **Normalizes** each: user/assistant turns → Envoy `content` (what the agent
   sees on resume) + `turn_payload.blocks` (text / tool_use / tool_result, what
   the UI renders). Sub-agent sidechains and CLI meta-noise are filtered out.
3. **Writes** a native session per import: `sessions` + `session_participants`
   (owner) + one `chat_messages` doc per turn, titled `[Imported] <project>`.
4. When you open it in Envoy and send a message, the orchestrator resumes the
   session, loads the seeded history, and the agent continues with full context.

## Run it

```bash
# Uses pymongo/certifi already in the envoy venv.
cd zivon-v2/zivon-desktop/importer
/Users/pulkitnagpal/Desktop/speakx/zivon-v2/zivon-envoy/.venv/bin/python app.py
```

It prints a `http://127.0.0.1:<port>` URL and opens your browser. Then:

1. Pick the **Organization** and target **Envoy** (dropdowns are populated from
   Mongo — the envoy you'd normally chat as).
2. The session list shows all your Claude sessions, **all checked**. Uncheck any
   you don't want, or use **Select none** + pick a few. Filter by project/text.
3. Click **Import selected** → watch per-session, per-turn progress.
4. Each finished session shows **Open in Envoy ↗** (`localhost:3000/org/<slug>/agent/<id>`).
   Open it, send a message, and continue where Claude left off.

## Try it safely first

Start with **one** short session (uncheck the rest) to confirm it opens and
continues cleanly before bulk-importing.

## Known POC limitations

- **Not idempotent** — re-importing the same session creates a duplicate Envoy
  session. (The productized version will dedupe on the Claude session id.)
- **Conversational continuity only** — the agent inherits the *transcript* and
  context, not the local working-tree state. Operating on the same local files
  needs the desktop **bridge** (Phase 2/3).
- **Project path display** may show `/` where the original had `-` (the Claude
  dir-name encoding is lossy); cosmetic only.
- Tool outputs are truncated to 4 KB per result in the rendered blocks.

## Files

- `parser.py` — discovery + JSONL parse + normalize (no backend; `python parser.py` lists sessions)
- `ingest.py` — Mongo connection, org/envoy listing, session write
- `app.py` — local GUI server (stdlib HTTP + streaming progress)
- `index.html` — the GUI
