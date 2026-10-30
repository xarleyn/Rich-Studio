"""Service status: health, configuration, incoming updates."""

import secrets

from fastapi import APIRouter, Request
from fastapi.responses import JSONResponse

import settings
import spec
from services import kinds, updates

router = APIRouter(tags=["service"])


@router.get("/healthz", include_in_schema=False)
def healthz():
    return {"ok": True}


@router.get(
    "/api/config",
    summary="Что нужно редактору: канал по умолчанию, чат для «Мне в личку», задан ли токен, версия спецификации",
)
def config():
    cfg = settings.get()
    return {
        "default_chat": cfg.target_chat,
        "preview_chat": cfg.preview_target,
        "token_set": bool(cfg.bot_token),
        "api_base": None if cfg.public else cfg.api_base,
        "spec_version": spec.current.version,
        "auth": cfg.public or bool(cfg.api_key),
        "public": cfg.public,
        "bot_username": cfg.bot_username if cfg.public else None,
        "updates": updates.public_status(),
        "kinds": kinds.describe(),
    }


@router.get("/api/updates", summary="Приём событий от Telegram: нажатия, комментарии, реакции, опросы")
def updates_status():
    return updates.public_status()


@router.post("/api/telegram/webhook", include_in_schema=False)
async def telegram_webhook(request: Request):
    """Telegram authenticates with its secret header, independently of studio sessions."""
    cfg = settings.get()
    if not cfg.updates_on or cfg.updates_mode != "webhook" or not cfg.webhook_secret:
        return JSONResponse({"detail": "not found"}, status_code=404)
    supplied = request.headers.get("x-telegram-bot-api-secret-token", "")
    if not secrets.compare_digest(supplied.encode(), cfg.webhook_secret.encode()):
        return JSONResponse({"detail": "forbidden"}, status_code=403)
    try:
        update = await request.json()
    except ValueError:
        return JSONResponse({"detail": "invalid update"}, status_code=400)
    if not isinstance(update, dict) or type(update.get("update_id")) is not int or update["update_id"] < 0:
        return JSONResponse({"detail": "invalid update"}, status_code=400)
    await updates.receive_webhook(update)
    return {"ok": True}
