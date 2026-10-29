"""Telegram identities and one-use login challenges, always in the root database."""

import time

from .db import conn
from .tokens import hash_secret


def save(user: dict) -> dict:
    now = int(time.time())
    with conn(root=True) as c:
        c.execute(
            """insert into accounts values(?, ?, ?, ?, ?, ?) on conflict(telegram_id) do update set
               first_name=excluded.first_name, last_name=excluded.last_name, username=excluded.username,
               updated_at=excluded.updated_at""",
            (user["id"], user["first_name"], user.get("last_name"), user.get("username"), now, now),
        )
        return dict(c.execute("select * from accounts where telegram_id=?", (user["id"],)).fetchone())


def ids() -> list[int]:
    with conn(root=True) as c:
        return [r[0] for r in c.execute("select telegram_id from accounts order by telegram_id")]


def session_by_id(uid: int) -> dict | None:
    with conn(root=True) as c:
        r = c.execute("select * from accounts where telegram_id=?", (uid,)).fetchone()
        return dict(r) if r else None


def challenge(secret: str, ttl: int) -> None:
    now = int(time.time())
    with conn(root=True) as c:
        c.execute("delete from login_states where expires_at<?", (now,))
        c.execute("insert into login_states values(?, ?)", (hash_secret(secret), now + ttl))


def consume(secret: str) -> bool:
    with conn(root=True) as c:
        return (
            c.execute(
                "delete from login_states where hash=? and expires_at>=?", (hash_secret(secret), int(time.time()))
            ).rowcount
            == 1
        )


def add_session(secret: str, uid: int, ttl: float) -> None:
    now = int(time.time())
    with conn(root=True) as c:
        c.execute("delete from account_sessions where expires_at<?", (now,))
        c.execute("insert into account_sessions values(?, ?, ?)", (hash_secret(secret), uid, int(now + ttl)))


def session(secret: str) -> dict | None:
    with conn(root=True) as c:
        r = c.execute(
            """select a.* from account_sessions s join accounts a on a.telegram_id=s.telegram_id
               where s.hash=? and s.expires_at>=?""",
            (hash_secret(secret), int(time.time())),
        ).fetchone()
        return dict(r) if r else None


def logout(secret: str) -> None:
    with conn(root=True) as c:
        c.execute("delete from account_sessions where hash=?", (hash_secret(secret),))


def take_budget(key: str, limit: int, seconds: int = 60) -> bool:
    now = int(time.time())
    with conn(root=True) as c:
        c.execute("delete from public_limits where expires_at<=?", (now,))
        hits = c.execute(
            """insert into public_limits values(?, ?, 1) on conflict(key) do update set hits=hits+1
               returning hits""",
            (hash_secret(key), now + seconds),
        ).fetchone()[0]
        return hits <= limit
