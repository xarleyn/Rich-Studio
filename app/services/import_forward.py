"""Receive only an explicitly armed forward in the requesting person's private bot chat."""

import account_context
import settings
import store
from errors import NotFound, StudioError
from services import accounts, imports
from services.import_links import parse_link
from store import import_inbox


def begin(url: str) -> dict:
    link = parse_link(url)
    if not imports.status()["forward_available"]:
        raise StudioError(
            "Для пересылки включите приём событий и задайте личный TG_PREVIEW_CHAT или войдите через Telegram.",
            status=409,
        )
    sender = account_context.user_id() or int(settings.get().preview_target)
    ticket = import_inbox.create(link.url, sender)
    return {
        "id": ticket["id"],
        "expires_at": ticket["expires_at"],
        "bot_url": f"https://t.me/{settings.get().bot_username}?start=import_{ticket['id']}",
    }


def get(token: str) -> dict:
    ticket = import_inbox.get(token)
    if not ticket:
        raise NotFound("Запрос импорта не найден.")
    return {key: ticket[key] for key in ("id", "state", "result", "error", "expires_at")}


def receive(update: dict) -> bool:
    m = update.get("message") or {}
    sender = (m.get("from") or {}).get("id")
    chat = m.get("chat") or {}
    if (
        not isinstance(sender, int)
        or chat.get("type") != "private"
        or chat.get("id") != sender
        or (m.get("from") or {}).get("is_bot")
    ):
        return False
    if settings.get().public and store.accounts.session_by_id(sender):
        uid = sender
    elif str(sender) == settings.get().preview_chat:
        uid = None
    else:
        return False
    with accounts.scope(uid):
        if str(m.get("text", "")).startswith("/start import_"):
            token = m["text"].removeprefix("/start import_")
            return import_inbox.arm(token, sender)
        if not m.get("forward_origin"):
            return False
        ticket = import_inbox.claim(sender)
        if not ticket:
            return False
        try:
            link = parse_link(ticket["source_url"])
            origin = m["forward_origin"]
            if origin.get("type") == "channel":
                source = origin.get("chat") or {}
                matches_peer = (
                    (source.get("username") or "").lower() == link.username
                    if link.username
                    else str(source.get("id")) == f"-100{link.channel_id}"
                )
                if not matches_peer or origin.get("message_id") != link.message_id:
                    raise StudioError(
                        "Переслано другое сообщение. Начните импорт заново и выберите пост по указанной ссылке."
                    )
            warnings = []
            if origin.get("type") != "channel":
                # Telegram group/hidden forwards do not carry the original message id.
                warnings.append("Telegram не передал исходный ID: соответствие пересылки ссылке не подтверждено.")
            result = imports.from_forward(m, link, warnings)
            import_inbox.finish(ticket["id"], result)
        except StudioError as error:
            import_inbox.finish(ticket["id"], error=str(error))
        return True
