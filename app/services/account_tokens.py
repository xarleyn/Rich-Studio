"""Issue scoped credentials for the signed-in Telegram account, never for the operator."""

import secrets
import time
import uuid

import account_context
import settings
from errors import Conflict, NotFound, StudioError
from store import account_tokens, accounts

MAX_ACTIVE = 20


def _uid() -> int:
    uid = account_context.user_id()
    if uid is None or not settings.get().public:
        raise StudioError("Войдите в учётку через Telegram", status=403)
    return uid


def listing() -> dict:
    return {"tokens": account_tokens.list_for(_uid()), "mcp_url": settings.get().public_url + "/mcp"}


def create(name: str, level: str, expires_in_days: int | None) -> dict:
    uid = _uid()
    if not accounts.take_budget(f"tokens:{uid}", 60, seconds=3600):
        raise StudioError("Слишком много новых токенов. Попробуйте через час.", status=429)
    secret, tid = "rsu_" + secrets.token_urlsafe(32), uuid.uuid4().hex
    expires = int(time.time()) + expires_in_days * 86400 if expires_in_days is not None else None
    if not account_tokens.add(uid, tid, name, secret, level, expires, MAX_ACTIVE):
        raise Conflict(f"Можно иметь не больше {MAX_ACTIVE} действующих токенов. Отзовите ненужный.")
    token = next(t for t in account_tokens.list_for(uid) if t["id"] == tid)
    return {"token": token, "secret": secret}


def revoke(tid: str) -> dict:
    if not account_tokens.revoke(_uid(), tid):
        raise NotFound("Токен не найден")
    return {"ok": True}
