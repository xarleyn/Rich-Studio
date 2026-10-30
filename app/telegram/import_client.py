"""Read-only MTProto transport. Session selection is supplied by the account-scoped service."""

from contextlib import asynccontextmanager
from urllib.parse import unquote, urlsplit

from telethon import TelegramClient
from telethon.sessions import StringSession

import settings
from errors import StudioError


def proxy_config(url: str) -> dict | None:
    if not url:
        return None
    p = urlsplit(url)
    return {
        "proxy_type": p.scheme,
        "addr": p.hostname,
        "port": p.port or (1080 if p.scheme == "socks5" else 8080),
        "username": unquote(p.username) if p.username else None,
        "password": unquote(p.password) if p.password else None,
        "rdns": True,
    }


def make_client(session: str = "") -> TelegramClient:
    cfg = settings.get()
    return TelegramClient(
        StringSession(session),
        cfg.import_api_id,
        cfg.import_api_hash,
        proxy=proxy_config(cfg.import_proxy),
        receive_updates=False,
        request_retries=0,
        connection_retries=1,
        retry_delay=0,
        flood_sleep_threshold=0,
        timeout=15,
        auto_reconnect=False,
    )


@asynccontextmanager
async def connection(session: str, expected_uid: int | None):
    """Never prompt for a login in a web request. Bot mode cannot borrow an operator's user session."""
    client = make_client(session)
    try:
        await client.connect()
        if not await client.is_user_authorized():
            if session or not settings.get().bot_token:
                raise StudioError("Подключение Telegram для импорта требует повторного входа.", status=409)
            await client.sign_in(bot_token=settings.get().bot_token)
        me = await client.get_me()
        if not me or (session and expected_uid is not None and (me.bot or me.id != expected_uid)):
            raise StudioError("Сессия импорта принадлежит другой учётке.", status=403)
        if not session and not me.bot:
            raise StudioError("Для общего подключения импорта требуется бот.", status=403)
        yield client, bool(me.bot)
    finally:
        await client.disconnect()
