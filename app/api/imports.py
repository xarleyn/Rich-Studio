"""Telegram message import: new drafts only."""

from fastapi import APIRouter

from models import ImportReq
from services import import_forward, imports

router = APIRouter(prefix="/api/imports", tags=["imports"])


@router.get("/telegram", summary="Доступные способы импорта из Telegram")
def import_status():
    """Только состояние подключения текущей учётки, без секретов и сессий."""
    return imports.status()


@router.post("/telegram", summary="Импортировать сообщение Telegram в новый черновик")
async def import_telegram(req: ImportReq):
    """Проверяет ссылку, читает сообщение и сохраняет визуальные блоки, HTML и Rich Markdown.

    Ничего не публикует. MTProto использует сессию текущей учётки либо бота; без подключения пробует
    публичный веб-виджет. Заглушка, приватная ссылка без сессии и неполный rich-пост дают ошибку.
    Потери преобразования перечислены в warnings. Существующие черновики не меняются.
    """
    return await imports.import_post(req.url)


@router.post("/forward", summary="Подготовить импорт выбранной пересылки в личном чате бота")
def begin_forward(req: ImportReq):
    """Откройте bot_url, нажмите Start и перешлите сообщение. Запрос действует 10 минут, принимает
    только следующую пересылку от текущего пользователя. Результат: GET /api/imports/forward/{id}."""
    return import_forward.begin(req.url)


@router.get("/forward/{token}", summary="Результат импорта пересланного сообщения текущей учётки")
def forward_status(token: str):
    return import_forward.get(token)
