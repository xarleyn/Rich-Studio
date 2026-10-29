"""Telegram Login Widget redirect flow with a browser-bound, one-use challenge."""

import secrets

from fastapi import APIRouter, Request, Response
from fastapi.responses import HTMLResponse, RedirectResponse

import security
import settings
import store
from errors import StudioError
from services import accounts

from .session import _https

router = APIRouter(tags=["service"])
STATE_COOKIE = "rs_telegram_login"


@router.get("/api/auth/config", summary="Публичные настройки входа через Telegram")
def config():
    cfg = settings.get()
    return {"telegram": cfg.public, "bot_username": cfg.bot_username if cfg.public else None}


@router.post("/api/auth/telegram/start", summary="Начать вход через Telegram с одноразовой проверкой браузера")
def start(request: Request, response: Response):
    """Challenge действует пять минут. Cookie SameSite=Lax нужна для возврата с telegram.org."""
    cfg = settings.get()
    if not cfg.public:
        raise StudioError("Вход через Telegram выключен", status=404)
    secret = secrets.token_urlsafe(32)
    store.accounts.challenge(secret, accounts.LOGIN_TTL)
    response.set_cookie(
        STATE_COOKIE,
        secret,
        max_age=accounts.LOGIN_TTL,
        httponly=True,
        secure=_https(request),
        samesite="lax",
        path="/auth/telegram/callback",
    )
    response.headers["Cache-Control"] = "no-store"
    return {"bot_username": cfg.bot_username, "auth_url": f"{cfg.public_url}/auth/telegram/callback?state={secret}"}


@router.get("/auth/telegram/callback", summary="Проверить подпись Telegram и открыть сессию пользователя")
def callback(request: Request):
    """Telegram id берётся только из подписанных данных; повторы и вход из другого браузера отклоняются."""
    try:
        return _callback(request)
    except StudioError as e:
        return HTMLResponse(
            '<!doctype html><html lang="ru"><meta charset="utf-8"><meta name="viewport" content="width=device-width">'
            '<title>Вход в RichPost</title><link rel="stylesheet" href="/static/app.css">'
            '<body><main class="login-screen"><div class="login-card"><h1>Не удалось войти</h1>'
            "<p>Данные входа не прошли проверку или устарели. Начните вход в этом браузере заново.</p>"
            '<a href="/">Вернуться ко входу</a></div></main></body></html>',
            status_code=e.status,
            headers={"Cache-Control": "no-store", "Content-Security-Policy": security.CSP},
        )


def _callback(request: Request):
    if not settings.get().public:
        raise StudioError("Вход через Telegram выключен", status=404)
    pairs = list(request.query_params.multi_items())
    data = dict(pairs)
    if len(pairs) != len(data):
        raise StudioError("Повторяющиеся параметры входа", status=401)
    state, cookie = data.get("state", ""), request.cookies.get(STATE_COOKIE, "")
    if not state or not cookie or not secrets.compare_digest(state.encode(), cookie.encode()):
        raise StudioError("Начните вход в этом браузере заново.", status=401)
    user = accounts.verify_login(data)
    if not store.accounts.consume(state):
        raise StudioError("Этот вход уже завершён или устарел. Войдите ещё раз.", status=401)
    accounts.initialize(user["id"])
    store.accounts.save(user)
    secret = secrets.token_urlsafe(32)
    ttl = settings.get().session_days * 86400
    store.accounts.add_session(secret, user["id"], ttl)
    response = RedirectResponse("/", status_code=303)
    response.set_cookie(
        security.SESSION_COOKIE,
        secret,
        max_age=int(ttl),
        httponly=True,
        secure=_https(request),
        samesite="strict",
        path="/",
    )
    response.delete_cookie(STATE_COOKIE, path="/auth/telegram/callback")
    response.headers["Cache-Control"] = "no-store"
    return response
