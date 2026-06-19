"""Write normalized Claude sessions into Envoy's per-org Mongo as native chat
sessions, so they can be opened and continued in the Envoy UI.

POC path: direct Mongo write (a trusted local tool). The productized path will
be a thin authenticated core endpoint the desktop app calls with the user's JWT
— this module never ships inside the desktop app.

We faithfully mirror what the orchestrator's CreateSession does:
  * sessions               — the session record (pod spawned lazily on first open)
  * session_participants   — the owner row (so the session is visible/openable)
  * chat_messages          — one doc per turn; ``content`` is what the agent sees
                             on resume, ``turn_payload.blocks`` is what the UI
                             renders.
"""

from __future__ import annotations

import datetime as dt
import re
from pathlib import Path
from typing import Any, Callable

CORE_ENV = (
    Path(__file__).resolve().parents[2] / "zivon-core" / "backend" / ".env"
)

ORG_DB_SUFFIX = "-zivon"


# --------------------------------------------------------------------------- #
# Config / connection
# --------------------------------------------------------------------------- #

def _read_env(path: Path) -> dict[str, str]:
    out: dict[str, str] = {}
    if not path.exists():
        return out
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        out[k.strip()] = v.strip()
    return out


def mongo_uri() -> str:
    env = _read_env(CORE_ENV)
    uri = env.get("MONGODB_ROOT_URI", "").strip()
    if not uri:
        raise RuntimeError(
            f"MONGODB_ROOT_URI not found in {CORE_ENV}. "
            "This POC reads the dev Mongo URI from zivon-core's .env."
        )
    return uri


def get_client():
    import certifi
    from pymongo import MongoClient

    return MongoClient(
        mongo_uri(), tlsCAFile=certifi.where(), serverSelectionTimeoutMS=10000
    )


# --------------------------------------------------------------------------- #
# Targets (orgs + envoys)
# --------------------------------------------------------------------------- #

def list_orgs(client) -> list[str]:
    """Org slugs = names of databases ending in ``-zivon``."""
    slugs = []
    for name in client.list_database_names():
        if name.endswith(ORG_DB_SUFFIX) and name != ORG_DB_SUFFIX:
            slugs.append(name[: -len(ORG_DB_SUFFIX)])
    return sorted(slugs)


def list_envoys(client, org_slug: str) -> list[dict]:
    """Envoys in an org, annotated with owner email/name from ``members``."""
    db = client[f"{org_slug}{ORG_DB_SUFFIX}"]
    members = {m["_id"]: m for m in db["members"].find({})}
    out = []
    for e in db["envoys"].find({}):
        mid = e.get("member_id")
        m = members.get(mid, {})
        out.append(
            {
                "envoy_id": str(e["_id"]),
                "name": e.get("name", "Envoy"),
                "member_id": str(mid) if mid else None,
                "owner_email": m.get("email"),
                "owner_name": m.get("name"),
                "status": e.get("status"),
            }
        )
    return out


# --------------------------------------------------------------------------- #
# Import
# --------------------------------------------------------------------------- #

def _sanitize_email(email: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", (email or "user").lower()).strip("-")


def _parse_ts(ts: str | None) -> dt.datetime | None:
    if not ts:
        return None
    try:
        return dt.datetime.fromisoformat(ts.replace("Z", "+00:00"))
    except ValueError:
        return None


def _next_session_number(db, envoy_oid) -> int:
    doc = db["sessions"].find_one(
        {"envoy_id": envoy_oid}, sort=[("number", -1)], projection={"number": 1}
    )
    return (doc.get("number", 0) + 1) if doc else 1


def import_session(
    client,
    org_slug: str,
    envoy: dict,
    session: dict,
    number: int | None = None,
    progress: Callable[[str, int, int], None] | None = None,
) -> dict:
    """Write one normalized session. Returns {session_id, url, turns_written}."""
    from bson import ObjectId

    db = client[f"{org_slug}{ORG_DB_SUFFIX}"]
    envoy_oid = ObjectId(envoy["envoy_id"])
    member_oid = ObjectId(envoy["member_id"]) if envoy.get("member_id") else None
    if member_oid is None:
        raise RuntimeError("selected envoy has no member_id — cannot import")

    owner_email = envoy.get("owner_email") or "imported@local"
    owner_name = envoy.get("owner_name")

    num = number if number is not None else _next_session_number(db, envoy_oid)
    session_oid = ObjectId()
    now = dt.datetime.now(dt.timezone.utc)

    base_ts = _parse_ts(session.get("started_at")) or now
    turns = session.get("turns", [])
    last_ts = base_ts

    # 1) chat_messages — one per turn.
    total = len(turns)
    for i, turn in enumerate(turns):
        ts = _parse_ts(turn.get("ts")) or (base_ts + dt.timedelta(seconds=i))
        # Guarantee strictly increasing order even if source timestamps tie.
        if ts <= last_ts and i > 0:
            ts = last_ts + dt.timedelta(milliseconds=1)
        last_ts = ts

        is_user = turn["role"] == "user"
        doc: dict[str, Any] = {
            "envoy_id": envoy_oid,
            "session_id": session_oid,
            "type": "user_message" if is_user else "assistant_message",
            "content": turn.get("text", ""),
            "source": "imported_claude",
            "metadata": {"imported": True, "import_source": "claude_code"},
            "turn_payload": {"blocks": turn.get("blocks", [])},
            "member_id": member_oid,
            "created_at": ts,
        }
        if is_user:
            doc["sender_email"] = owner_email
            if owner_name:
                doc["sender_name"] = owner_name
        db["chat_messages"].insert_one(doc)
        if progress:
            progress(session["session_id"], i + 1, total)

    # 2) sessions — the record (pod spawns lazily when opened).
    title = f"[Imported] {Path(session.get('project', 'claude')).name or 'claude'}"
    db["sessions"].insert_one(
        {
            "_id": session_oid,
            "envoy_id": envoy_oid,
            "member_id": member_oid,
            "pod_name": f"{_sanitize_email(owner_email)}-envoy2-{num}",
            "pod_type": "envoy2",
            "number": num,
            "created_at": base_ts,
            "last_message_at": last_ts,
            "source": "local",
            "custom_name": title,
        }
    )

    # 3) session_participants — owner row so the session is visible/openable.
    db["session_participants"].insert_one(
        {
            "session_id": session_oid,
            "member_id": member_oid,
            "email": owner_email,
            "name": owner_name,
            "role": "owner",
            "status": "accepted",
            "added_at": now,
        }
    )

    return {
        "session_id": str(session_oid),
        "url": f"/org/{org_slug}/agent/{session_oid}",
        "turns_written": total,
        "title": title,
    }
