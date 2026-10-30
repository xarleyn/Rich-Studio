"""Durable, expiring, account-scoped receipt for an explicitly requested forwarded message."""

import json
import secrets
import time

from .db import conn


def create(url: str, sender_id: int) -> dict:
    token = secrets.token_urlsafe(18)
    with conn() as c:
        c.execute("delete from telegram_import_inbox")  # latest explicit request replaces the old one
        c.execute(
            "insert into telegram_import_inbox values(?, ?, ?, ?, 'waiting', null, null)",
            (token, url, sender_id, int(time.time()) + 600),
        )
    return get(token) or {}


def get(token: str) -> dict | None:
    with conn() as c:
        row = c.execute("select * from telegram_import_inbox where id=?", (token,)).fetchone()
    if not row:
        return None
    out = dict(row)
    if out["expires_at"] < time.time() and out["state"] in {"waiting", "armed", "processing"}:
        out["state"] = "expired"
    out["result"] = json.loads(out["result"]) if out["result"] else None
    return out


def arm(token: str, sender_id: int) -> bool:
    with conn() as c:
        return (
            c.execute(
                "update telegram_import_inbox set state='armed' where id=? and sender_id=? and expires_at>=? and state='waiting'",
                (token, sender_id, int(time.time())),
            ).rowcount
            == 1
        )


def claim(sender_id: int) -> dict | None:
    with conn() as c:
        row = c.execute(
            "update telegram_import_inbox set state='processing' where sender_id=? and expires_at>=? and state='armed' returning *",
            (sender_id, int(time.time())),
        ).fetchone()
    return dict(row) if row else None


def finish(token: str, result: dict | None = None, error: str = "") -> None:
    with conn() as c:
        c.execute(
            "update telegram_import_inbox set state=?, result=?, error=? where id=?",
            (
                "failed" if error else "done",
                json.dumps(result, ensure_ascii=False) if result else None,
                error or None,
                token,
            ),
        )
