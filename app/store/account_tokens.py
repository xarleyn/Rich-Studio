"""Personal credentials in the root database; every management query binds the account id."""

import time

from .db import conn
from .tokens import USED_EVERY, hash_secret


def list_for(uid: int) -> list[dict]:
    with conn(root=True) as c:
        return [
            dict(r)
            for r in c.execute(
                """select id, name, level, created_at, expires_at, last_used_at, revoked_at
                   from account_tokens where telegram_id=? order by created_at desc, id""",
                (uid,),
            )
        ]


def add(uid: int, tid: str, name: str, secret: str, level: str, expires: int | None, limit: int) -> bool:
    now = int(time.time())
    with conn(root=True) as c:
        c.execute("begin immediate")
        active = c.execute(
            """select count(*) from account_tokens where telegram_id=? and revoked_at is null
               and (expires_at is null or expires_at>?)""",
            (uid, now),
        ).fetchone()[0]
        if active >= limit:
            return False
        # Keep a bounded history of the 100 latest inactive tokens for this account.
        c.execute(
            """delete from account_tokens where id in
               (select id from account_tokens where telegram_id=? and
                (revoked_at is not null or expires_at<=?) order by created_at desc, id limit -1 offset 100)""",
            (uid, now),
        )
        c.execute(
            """insert into account_tokens(id, telegram_id, name, hash, level, created_at, expires_at)
               values(?, ?, ?, ?, ?, ?, ?)""",
            (tid, uid, name, hash_secret(secret), level, now, expires),
        )
        return True


def revoke(uid: int, tid: str) -> bool:
    with conn(root=True) as c:
        return (
            c.execute(
                "update account_tokens set revoked_at=coalesce(revoked_at, ?) where id=? and telegram_id=?",
                (int(time.time()), tid, uid),
            ).rowcount
            == 1
        )


def by_secret(secret: str) -> dict | None:
    now = int(time.time())
    with conn(root=True) as c:
        r = c.execute(
            """select t.id, t.name, t.level, t.telegram_id, t.last_used_at from account_tokens t
               join accounts a on a.telegram_id=t.telegram_id
               where t.hash=? and t.revoked_at is null and (t.expires_at is null or t.expires_at>?)""",
            (hash_secret(secret), now),
        ).fetchone()
        if r and (r["last_used_at"] or 0) < now - USED_EVERY:
            c.execute("update account_tokens set last_used_at=? where id=?", (now, r["id"]))
        return dict(r) if r else None
