"""Private conversation with the bot. Telegram's verified sender chooses the account, never command text."""

import html
import logging
import re

import account_context
import logs
import settings
import store
import telegram as tg
from errors import StudioError
from store import bot_dialog as db

from . import accounts

log = logging.getLogger("rich-studio.bot")
PREFIX = "rsbot:"
COMMANDS = {
    "start": ("Знакомство со студией", "Welcome to the studio"),
    "help": ("Как создавать и публиковать посты", "How to create and publish posts"),
    "studio": ("Открыть редактор", "Open the editor"),
    "status": ("Мои публикации и очередь", "My posts and publishing queue"),
    "notifications": ("Настроить уведомления", "Notification settings"),
    "stop": ("Выключить уведомления", "Turn off notifications"),
}


def phrase(language: str, ru: str, en: str) -> str:
    return ru if language == "ru" else en


def recipient() -> int | None:
    uid = account_context.user_id()
    if uid is not None:
        return uid
    target = settings.get().preview_chat
    return int(target) if target.isdecimal() and 0 < int(target) < 2**52 else None


def keyboard(language: str, *, preferences: bool = False, link: str | None = None) -> dict:
    rows = []
    if link:
        rows.append([{"text": phrase(language, "Посмотреть пост ↗", "View post ↗"), "url": link}])
    url = settings.get().public_url
    if url.startswith("https://"):
        rows.append([{"text": phrase(language, "Открыть студию ↗", "Open studio ↗"), "url": url}])
    if preferences:
        rows += [
            [{"text": text, "callback_data": PREFIX + "notifications " + mode}]
            for mode, text in (
                ("all", phrase(language, "🔔 Все публикации и ошибки", "🔔 All posts and errors")),
                ("errors", phrase(language, "⚠️ Только ошибки", "⚠️ Errors only")),
                ("off", phrase(language, "🔕 Выключить", "🔕 Turn off")),
            )
        ]
    else:
        rows.append(
            [
                {"text": phrase(language, "Помощь", "Help"), "callback_data": PREFIX + "help"},
                {"text": phrase(language, "Уведомления", "Notifications"), "callback_data": PREFIX + "notifications"},
            ]
        )
    return {"inline_keyboard": rows}


async def send_private(uid: int, text: str, markup: dict) -> None:
    """Narrow exception for one studio-owned message to self, with the normal policy restored afterward."""
    if uid != recipient():
        raise StudioError("Получатель сообщения не совпадает с текущей учёткой.", status=403)
    params = {
        "chat_id": uid,
        "text": logs.scrub(text),
        "parse_mode": "HTML",
        "reply_markup": markup,
        "link_preview_options": {"is_disabled": True},
    }
    current = account_context.user_id()

    async def policy(method: str, values: dict) -> dict:
        if method == "sendMessage" and values == params:
            return values
        return await accounts.authorize(method, values)

    with account_context.bind(current, policy):
        await tg.call("sendMessage", params, timeout=15)


async def register_commands() -> None:
    for language in ("", "ru", "en"):
        await tg.call(
            "setMyCommands",
            {
                "scope": {"type": "all_private_chats"},
                "language_code": language,
                "commands": [
                    {"command": key, "description": labels[1 if language == "en" else 0]}
                    for key, labels in COMMANDS.items()
                ],
            },
        )


def _known_account(uid: int) -> bool:
    return bool(store.accounts.session_by_id(uid)) if settings.get().public else str(uid) == settings.get().preview_chat


def _status(uid: int, language: str) -> str:
    if not _known_account(uid):
        return phrase(
            language,
            "Сначала откройте студию и войдите через Telegram. Здесь появится состояние ваших публикаций.",
            "Open the studio and sign in with Telegram first. Your publishing status will appear here.",
        )
    with accounts.scope(uid if settings.get().public else None):
        counts = store.stats.post_counts()
    title = phrase(language, "📊 Ваши публикации", "📊 Your posts")
    labels = {
        "sent": ("Опубликовано", "Published"),
        "scheduled": ("В очереди", "Scheduled"),
        "sending": ("Отправляется", "Sending"),
        "failed": ("Требуют проверки", "Need attention"),
        "missed": ("Пропущено", "Missed"),
    }
    return (
        f"<b>{title}</b>\n\n"
        + "\n".join(f"{phrase(language, *label)}: {counts.get(key, 0)}" for key, label in labels.items())
        + phrase(language, "\n\nПодробности — во вкладке «Посты» в студии.", "\n\nSee the Posts tab for details.")
    )


def _help(language: str) -> str:
    return phrase(
        language,
        "<b>Как работать с Rich Studio</b>\n\n"
        "1. Откройте студию и войдите через Telegram.\n"
        "2. Добавьте бота администратором канала с правом публикации и подключите канал в окне «Каналы».\n"
        "3. Создайте пост. «Мне в личку» покажет предпросмотр, «Опубликовать» отправит в канал.\n"
        "4. Можно назначить время публикации — студия должна работать в этот момент.\n\n"
        "/studio — открыть редактор\n/status — ваши публикации и очередь\n"
        "/notifications — все уведомления, только ошибки или тишина\n/stop — выключить уведомления\n\n"
        "Если бот пишет «мог уйти», проверьте канал перед повтором. Пересылку для импорта начните в студии.",
        "<b>Getting started with Rich Studio</b>\n\n"
        "1. Open the studio and sign in with Telegram.\n"
        "2. Add the bot as a channel administrator with posting rights, then connect the channel in Channels.\n"
        "3. Create a post. Send a private preview to check it, then Publish to your channel.\n"
        "4. You can schedule a post — the studio must be running at that time.\n\n"
        "/studio — open the editor\n/status — your posts and queue\n"
        "/notifications — all notifications, errors only, or off\n/stop — turn notifications off\n\n"
        "If a post may have been sent, check the channel before retrying. Start forward imports in the studio.",
    )


async def handle(update: dict) -> bool:
    callback = update.get("callback_query") or {}
    message = callback.get("message") or update.get("message") or {}
    sender = callback.get("from") or message.get("from") or {}
    uid, chat = sender.get("id"), message.get("chat") or {}
    if (
        type(uid) is not int
        or not 0 < uid < 2**52
        or sender.get("is_bot")
        or chat.get("type") != "private"
        or chat.get("id") != uid
    ):
        return False
    if callback and not str(callback.get("data", "")).startswith(PREFIX):
        return False
    if message.get("forward_origin") and not callback:
        return False  # import_forward owns forwarded messages
    raw = callback.get("data", "")[len(PREFIX) :] if callback else message.get("text", "")
    if not isinstance(raw, str) or not raw:
        return False
    parts = raw.split(maxsplit=1)
    if not parts:
        return False
    command, arg = parts[0], parts[1].strip() if len(parts) > 1 else ""
    if not callback:
        match = re.fullmatch(r"/([a-zA-Z_]+)(?:@([\w]+))?", command)
        if match and match[2] and match[2].lower() != settings.get().bot_username.lower():
            return False
        command = match[1].lower() if match else "unknown"
    language = "en" if str(sender.get("language_code", "ru")).startswith("en") else "ru"
    # A private deployment never accepts another person's subscription or account commands.
    if not settings.get().public and str(uid) != settings.get().preview_chat:
        return False
    mode = arg if command == "notifications" and arg in {"all", "errors", "off"} else None
    if command == "stop":
        mode = "off"
    if mode == "off":
        db.disable(uid)  # opting out still works when a person has exhausted the reply budget
    if not store.accounts.take_budget(f"bot-dialog:{uid}", 12):
        if callback:
            await tg.call("answerCallbackQuery", {"callback_query_id": callback["id"]})
        return True
    start = command == "start" and not arg.startswith("import_")
    existing = db.contact(uid)
    prefs = existing or {"mode": "off"}
    if start or mode is not None or existing:
        prefs = db.touch(uid, language, mode, start=start)
    if callback:
        await tg.call("answerCallbackQuery", {"callback_query_id": callback["id"]})
    if command == "start" and arg.startswith("import_"):
        text = phrase(
            language,
            "Запрос импорта устарел или не найден. Начните импорт заново в студии и откройте новую ссылку.",
            "This import request expired or was not found. Start a new import in the studio and open its link.",
        )
    elif command == "start":
        text = phrase(
            language,
            "<b>Привет! Я бот Rich Studio 👋</b>\n\n"
            "Помогаю публиковать красивые посты в Telegram: таблицы, фото, кнопки и отложенные публикации.\n\n"
            "Откройте студию, войдите через Telegram и подключите свой канал. Здесь будут предпросмотры "
            "и отчёты о публикациях.\n\n/help — с чего начать\n/notifications — настроить уведомления",
            "<b>Hi! I'm the Rich Studio bot 👋</b>\n\n"
            "Create Telegram posts with tables, photos, buttons and scheduled publishing.\n\n"
            "Open the studio, sign in with Telegram and connect your channel. Your previews and publishing "
            "reports will arrive here.\n\n/help — getting started\n/notifications — notification settings",
        )
    elif command == "help":
        text = _help(language)
    elif command == "status":
        text = _status(uid, language)
    elif command == "studio":
        text = phrase(
            language,
            "🎨 Откройте студию, чтобы создать или отредактировать пост.",
            "🎨 Open the studio to create or edit a post.",
        )
    elif command in {"notifications", "stop"}:
        labels = {
            "all": ("все публикации и ошибки", "all posts and errors"),
            "errors": ("только ошибки", "errors only"),
            "off": ("выключены", "off"),
        }
        text = phrase(language, "🔔 Уведомления: ", "🔔 Notifications: ") + phrase(language, *labels[prefs["mode"]])
        text += phrase(
            language,
            "\n\nВыберите режим ниже. /stop в любой момент выключит уведомления.",
            "\n\nChoose a mode below. Use /stop at any time to turn notifications off.",
        )
    else:
        text = phrase(
            language,
            "Посты создаются в студии. Я помогу с предпросмотром и сообщу о публикациях. "
            "Нажмите «Открыть студию» или отправьте /help.",
            "Create posts in the studio. I handle previews and publishing reports. Tap Open studio or send /help.",
        )
    if not settings.get().public_url.startswith("https://"):
        text += "\n\n" + html.escape(settings.get().public_url)
    with accounts.scope(uid if settings.get().public else None):
        await send_private(uid, text, keyboard(language, preferences=command in {"notifications", "stop"}))
    return True


async def import_reply(update: dict) -> None:
    """An armed import keeps its existing handler; acknowledge its next step without changing its outcome."""
    message = update.get("message") or {}
    uid = (message.get("from") or {})["id"]
    language = "en" if str((message.get("from") or {}).get("language_code", "ru")).startswith("en") else "ru"
    text = (
        phrase(
            language,
            "📥 Теперь перешлите сюда исходный пост из Telegram.",
            "📥 Now forward the original Telegram post here.",
        )
        if not message.get("forward_origin")
        else phrase(
            language,
            "📥 Пересылка обработана. Результат импорта или объяснение ошибки — в студии.",
            "📥 Forward processed. Open the studio for the imported draft or error details.",
        )
    )
    with accounts.scope(uid if settings.get().public else None):
        await send_private(uid, text, keyboard(language))
