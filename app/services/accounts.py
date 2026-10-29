"""Telegram login and account scopes. Every account has its own database and uploads."""

import contextlib
import hashlib
import hmac
import time
from collections.abc import Iterator

import account_context
import settings
import store
import telegram as tg
from errors import StudioError

LOGIN_TTL = 300
LOGIN_FIELDS = {"id", "first_name", "last_name", "username", "photo_url", "auth_date"}
READ_METHODS = {"getMe", "getChat", "getChatMember", "answerCallbackQuery"}


def verify_login(data: dict[str, str]) -> dict:
    """Verify Telegram Login Widget's HMAC and freshness before trusting any identity field."""
    fields = {k: v for k, v in data.items() if k in LOGIN_FIELDS}
    check = "\n".join(f"{k}={v}" for k, v in sorted(fields.items()))
    key = hashlib.sha256(settings.get().bot_token.encode()).digest()
    expected = hmac.new(key, check.encode(), hashlib.sha256).hexdigest()
    if not settings.get().bot_token or not hmac.compare_digest(expected.encode(), data.get("hash", "").encode()):
        raise StudioError("Telegram не подтвердил вход. Попробуйте войти ещё раз.", status=401)
    try:
        uid, at = int(fields["id"]), int(fields["auth_date"])
        if not 0 < uid < 2**52 or not -30 <= time.time() - at <= LOGIN_TTL or not fields["first_name"]:
            raise ValueError
    except (KeyError, ValueError):
        raise StudioError("Данные входа устарели или повреждены. Войдите ещё раз.", status=401) from None
    return {**fields, "id": uid}


@contextlib.contextmanager
def scope(uid: int | None, *, trusted: bool = False) -> Iterator[None]:
    """Bind storage and transport together; context propagates to async tasks and worker threads."""
    with account_context.bind(uid, (_trusted if trusted else authorize) if uid is not None else None):
        yield


async def _trusted(method: str, params: dict) -> dict:
    return params


def initialize(uid: int) -> None:
    with scope(uid):
        store.init()
        settings.get().media_dir.mkdir(parents=True, exist_ok=True)


def all_ids() -> list[int | None]:
    return [None, *store.accounts.ids()] if settings.get().public else [None]


def linked_target(chat_id) -> int | str:
    """An account cannot select an arbitrary destination or inherit the operator's default channel."""
    uid = account_context.user_id()
    if uid is None:
        return chat_id
    if str(chat_id) == str(uid):
        return uid
    ch = store.get_channel(chat_id)
    if not ch or ch.get("chat_num") is None:
        raise StudioError("Сначала привяжите этот канал к своей учётке в окне «Каналы».", status=403)
    return ch["chat_num"]


async def check_member(chat_id, chat_type: str | None = None, *, method: str | None = None) -> None:
    uid = account_context.user_id()
    if uid is None:
        return
    member = await tg.call("getChatMember", {"chat_id": chat_id, "user_id": uid})
    status = member.get("status")
    allowed = status == "creator" or (
        status == "administrator" and (chat_type != "channel" or bool(member.get("can_post_messages")))
    )
    if not allowed:
        raise StudioError("Нужны ваши права администратора с публикацией сообщений в этом канале.", status=403)
    if status == "creator" or method is None:
        return
    required = None
    if "InviteLink" in method:
        required = "can_invite_users"
    elif method in {"deleteMessage", "deleteMessages"}:
        required = "can_delete_messages"
    elif method in {"pinChatMessage", "unpinChatMessage", "unpinAllChatMessages"}:
        required = "can_edit_messages" if chat_type == "channel" else "can_pin_messages"
    elif chat_type == "channel" and (method.startswith("editMessage") or method == "stopPoll"):
        required = "can_edit_messages"
    if required and not member.get(required):
        raise StudioError(f"У вас нет права Telegram {required} для этой операции.", status=403)


def _paid_or_business(value) -> bool:
    if isinstance(value, dict):
        return bool(value.get("allow_paid_broadcast") or value.get("business_connection_id")) or any(
            _paid_or_business(v) for v in value.values()
        )
    return isinstance(value, list) and any(_paid_or_business(v) for v in value)


async def authorize(method: str, params: dict) -> dict:
    """Final transport boundary for public users, including edits, invites and scheduled sends."""
    if method in READ_METHODS:
        return params
    if _paid_or_business(params):
        raise StudioError("Платная массовая рассылка и бизнес-подключения здесь недоступны.", status=403)
    if "chat_id" not in params:
        raise StudioError("Этот метод недоступен для пользовательской учётки.", status=403)
    target = linked_target(params["chat_id"])
    uid = account_context.user_id()
    if target == uid:
        if method not in {"sendRichMessage", "sendPoll", "sendLivePhoto", "sendPaidMedia"}:
            raise StudioError("В личном чате доступен только предпросмотр.", status=403)
    else:
        ch = store.get_channel(target) or {}
        await check_member(target, ch.get("type"), method=method)
    reply = params.get("reply_parameters")
    if reply is not None:
        if not isinstance(reply, dict):
            raise StudioError("reply_parameters задаётся объектом JSON.", status=403)
        if reply.get("chat_id") is not None:
            source = linked_target(reply["chat_id"])
            if source not in {target, uid}:
                ch = store.get_channel(source) or {}
                await check_member(source, ch.get("type"))
            params = {**params, "reply_parameters": {**reply, "chat_id": source}}
    return {**params, "chat_id": target}
