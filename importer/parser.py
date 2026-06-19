"""Discover and parse local Claude Code sessions into a normalized shape.

Claude Code stores one JSONL file per session under
``~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl``. Each line is an event
with a ``type`` (user/assistant/system/attachment/...) and, for user/assistant
events, a ``message`` holding standard Anthropic role+content blocks.

We normalize each session to:

    {
      "session_id": "<uuid>",
      "project": "<decoded cwd>",
      "cwd": "<cwd>",
      "git_branch": "<branch|None>",
      "started_at": "<iso>",
      "last_at": "<iso>",
      "turn_count": <int>,
      "preview": "<first user prompt, truncated>",
      "path": "<jsonl path>",
      "turns": [ {role, text, blocks, ts}, ... ],
    }

``blocks`` mirror Envoy's TypedBlock shapes so the UI renders faithfully on
reload: {kind:"text",content} / {kind:"tool_use",toolUseId,name,input} /
{kind:"tool_result",toolUseId,content}.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any, Iterator

CLAUDE_PROJECTS = Path.home() / ".claude" / "projects"


def _decode_project_dir(name: str) -> str:
    """Project dir names encode the cwd with '/' replaced by '-'. We can't
    perfectly invert it (real dashes are ambiguous), but a leading dash maps to
    a leading slash, which recovers the common case for display."""
    if name.startswith("-"):
        return "/" + name[1:].replace("-", "/")
    return name.replace("-", "/")


def _text_from_content(content: Any) -> str:
    """Extract human-readable text from a message ``content`` (string or list)."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for b in content:
            if isinstance(b, dict) and b.get("type") == "text":
                parts.append(b.get("text", ""))
        return "\n".join(p for p in parts if p)
    return ""


def _tool_result_text(content: Any) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for b in content:
            if isinstance(b, dict) and b.get("type") == "text":
                parts.append(b.get("text", ""))
            elif isinstance(b, str):
                parts.append(b)
        return "\n".join(p for p in parts if p)
    return str(content) if content is not None else ""


def _iter_events(path: Path) -> Iterator[dict]:
    with path.open("r", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue


def parse_session(path: Path) -> dict | None:
    """Parse one .jsonl into the normalized session shape, or None if empty."""
    cwd = None
    git_branch = None
    started_at = None
    last_at = None
    preview = None

    # First pass over tool_use ids -> their result text, so we can attach
    # results back onto the assistant turn that issued them (UI fidelity).
    tool_results: dict[str, str] = {}
    for ev in _iter_events(path):
        if ev.get("type") != "user":
            continue
        content = ev.get("message", {}).get("content")
        if isinstance(content, list):
            for b in content:
                if isinstance(b, dict) and b.get("type") == "tool_result":
                    tid = b.get("tool_use_id")
                    if tid:
                        tool_results[tid] = _tool_result_text(b.get("content"))

    turns: list[dict] = []
    for ev in _iter_events(path):
        t = ev.get("type")
        if cwd is None and ev.get("cwd"):
            cwd = ev.get("cwd")
        if git_branch is None and ev.get("gitBranch"):
            git_branch = ev.get("gitBranch")
        ts = ev.get("timestamp")
        if ts:
            started_at = started_at or ts
            last_at = ts

        # Only main-thread user/assistant turns; skip sub-agent sidechains and
        # meta/system/attachment noise.
        if t not in ("user", "assistant"):
            continue
        if ev.get("isSidechain"):
            continue
        if ev.get("isMeta"):
            continue

        msg = ev.get("message", {})
        role = msg.get("role", t)
        content = msg.get("content")

        if role == "user":
            text = _text_from_content(content)
            if not text.strip():
                continue  # tool-result-only user turn — captured as blocks above
            # Skip CLI command wrapper noise.
            if text.lstrip().startswith("<local-command") or text.lstrip().startswith(
                "<command-"
            ):
                continue
            if preview is None:
                preview = text.strip()[:200]
            turns.append(
                {
                    "role": "user",
                    "text": text,
                    "blocks": [{"kind": "text", "content": text}],
                    "ts": ts,
                }
            )

        elif role == "assistant":
            blocks: list[dict] = []
            texts: list[str] = []
            tool_names: list[str] = []
            if isinstance(content, list):
                for b in content:
                    if not isinstance(b, dict):
                        continue
                    bt = b.get("type")
                    if bt == "text":
                        txt = b.get("text", "")
                        if txt:
                            texts.append(txt)
                            blocks.append({"kind": "text", "content": txt})
                    elif bt == "tool_use":
                        tid = b.get("id", "")
                        name = b.get("name", "tool")
                        tool_names.append(name)
                        blocks.append(
                            {
                                "kind": "tool_use",
                                "toolUseId": tid,
                                "name": name,
                                "input": b.get("input", {}),
                            }
                        )
                        if tid in tool_results:
                            blocks.append(
                                {
                                    "kind": "tool_result",
                                    "toolUseId": tid,
                                    "content": tool_results[tid][:4000],
                                }
                            )
            elif isinstance(content, str):
                if content:
                    texts.append(content)
                    blocks.append({"kind": "text", "content": content})

            if not blocks:
                continue
            text = "\n".join(texts).strip()
            if not text and tool_names:
                text = f"[used tools: {', '.join(dict.fromkeys(tool_names))}]"
            turns.append(
                {"role": "assistant", "text": text, "blocks": blocks, "ts": ts}
            )

    if not turns:
        return None

    return {
        "session_id": path.stem,
        "project": _decode_project_dir(path.parent.name),
        "cwd": cwd or _decode_project_dir(path.parent.name),
        "git_branch": git_branch,
        "started_at": started_at,
        "last_at": last_at,
        "turn_count": len(turns),
        "preview": preview or "(no text prompt)",
        "path": str(path),
        "turns": turns,
    }


def discover_sessions(root: Path = CLAUDE_PROJECTS) -> list[dict]:
    """List all sessions (metadata only — turns parsed but returned too).

    Returns newest-first by last activity.
    """
    if not root.exists():
        return []
    sessions = []
    for proj in root.iterdir():
        if not proj.is_dir():
            continue
        for f in proj.glob("*.jsonl"):
            try:
                s = parse_session(f)
            except Exception:
                s = None
            if s:
                sessions.append(s)
    sessions.sort(key=lambda s: s.get("last_at") or "", reverse=True)
    return sessions


def session_summaries(sessions: list[dict]) -> list[dict]:
    """Strip the heavy ``turns`` for the listing payload."""
    return [{k: v for k, v in s.items() if k != "turns"} for s in sessions]


if __name__ == "__main__":
    found = discover_sessions()
    print(f"Found {len(found)} sessions")
    for s in found[:10]:
        print(
            f"  [{s['turn_count']:>3} turns] {s['last_at']}  {s['project']}\n"
            f"        {s['preview'][:80]}"
        )
