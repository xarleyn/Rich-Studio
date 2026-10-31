"""Root bot contacts contain no account data; each account owns its notification outbox."""

import json
import time

from .db import conn


def contact(uid: int) -> dict | None:
    with conn(root=True) as c:
        row = c.execute("select * from bot_contacts where telegram_id=?", (uid,)).fetchone()
        return dict(row) if row else None


def touch(uid: int, language: str, mode: str | None = None, *, start: bool = False) -> dict:
    """Only an explicit start or setting opts in; repeated /start preserves the chosen mode."""
    with conn(root=True) as c:
        c.execute(
            """insert into bot_contacts(telegram_id, language, mode, updated_at) values(?, ?, ?, ?)
               on conflict(telegram_id) do update set language=excluded.language,
               revision=revision + case when ? is not null and mode != ? then 1 else 0 end,
               mode=coalesce(?, bot_contacts.mode), updated_at=excluded.updated_at""",
            (uid, language, mode or ("all" if start else "off"), int(time.time()), mode, mode, mode),
        )
    return contact(uid) or {}


def disable(uid: int) -> None:
    with conn(root=True) as c:
        c.execute(
            "update bot_contacts set mode='off', revision=revision+1, updated_at=? where telegram_id=? and mode!='off'",
            (int(time.time()), uid),
        )


def enqueue(key: str, recipient: int, category: str, body: dict) -> None:
    with conn() as c:
        c.execute(
            """insert or ignore into bot_outbox(event_key, recipient, category, body, created_at)
               values(?, ?, ?, ?, ?)""",
            (key, recipient, category, json.dumps(body, ensure_ascii=False), int(time.time())),
        )


def due(limit: int = 10) -> list[dict]:
    with conn() as c:
        # No stale report after a long outage, and no unbounded accumulation of delivery receipts.
        c.execute("delete from bot_outbox where created_at<?", (int(time.time()) - 7 * 86400,))
        rows = c.execute(
            "select * from bot_outbox where state='pending' and retry_at<=? order by created_at, rowid limit ?",
            (int(time.time()), limit),
        )
        return [{**dict(r), "body": json.loads(r["body"])} for r in rows]


def claim(key: str) -> bool:
    """Persist before send: a crash or timeout must never duplicate a private notification."""
    with conn() as c:
        return (
            c.execute(
                "update bot_outbox set state='attempted', attempts=attempts+1 where event_key=? and state='pending'",
                (key,),
            ).rowcount
            == 1
        )


def finish(key: str, state: str, *, retry_at: int = 0) -> None:
    with conn() as c:
        c.execute("update bot_outbox set state=?, retry_at=? where event_key=?", (state, retry_at, key))


def sending() -> list[str]:
    with conn() as c:
        return [r[0] for r in c.execute("select id from posts where status='sending'")]


def waiting_comments(before: int) -> list[str]:
    with conn() as c:
        return [r[0] for r in c.execute("select id from posts where comment_status='waiting' and sent_at<?", (before,))]
