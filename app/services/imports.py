"""Import a Telegram message into a new account-owned draft without publishing anything."""

import asyncio
import json
import re
import secrets
from pathlib import Path

import httpx
from telethon import errors as mt_errors
from telethon.tl import functions, types

import account_context
import settings
import spec
import store
from errors import Invalid, StudioError
from services import import_web, media
from services.import_links import MessageLink, parse_link
from spec.import_content import Converter, entities_text, representations
from telegram import import_client

DEADLINE = 120
MAX_FILES = 30
MAX_TOTAL_BYTES = 100 * 1024 * 1024
_active: set[int | None] = set()


def session_path() -> Path:
    return settings.get().account_dir / "telegram-import.session.txt"


def status() -> dict:
    cfg = settings.get()
    return {
        "mtproto_configured": bool(cfg.import_api_id and cfg.import_api_hash),
        "own_session": session_path().is_file(),
        "bot_username": cfg.bot_username,
        "forward_available": bool(
            cfg.updates_on and cfg.bot_token and (account_context.user_id() or cfg.preview_target.isdigit())
        ),
    }


def save_result(blocks: list, warnings: list[str], link: MessageLink, provider: str, title="") -> dict:
    if not blocks:
        raise Invalid("Сообщение не содержит поддерживаемого содержимого.")
    if len(json.dumps(blocks, ensure_ascii=False).encode("utf-8")) > 2 * 1024 * 1024:
        raise StudioError("Содержимое превышает лимит импорта 2 МБ.", status=413)
    html, markdown, files = representations(blocks, warnings)
    source = {**link.describe(), "provider": provider}
    payload: dict = {
        "rich_message": {"blocks": blocks, "media": files},
        "options": {},
        "_studio": {
            "format": "blocks",
            "buffers": {"blocks": blocks, "html": html, "markdown": markdown},
            "import_source": source,
            "import_warnings": list(dict.fromkeys(warnings)),
        },
    }
    # The user's publish target, callbacks, pinning and scheduling are never copied from the source.
    draft = store.save_draft(None, title or f"Импорт {link.username or 'Telegram'}/{link.message_id}", payload)
    return {
        "draft": draft,
        "source": source,
        "warnings": payload["_studio"]["import_warnings"],
        "block_count": len(blocks),
        "media_count": len(files),
    }


def _document_kind(document) -> str:
    attributes = document.attributes or []
    if any(isinstance(a, types.DocumentAttributeAnimated) for a in attributes):
        return "animation"
    audio = next((a for a in attributes if isinstance(a, types.DocumentAttributeAudio)), None)
    if audio:
        return "voice_note" if audio.voice else "audio"
    if any(isinstance(a, types.DocumentAttributeVideo) for a in attributes):
        return "video"
    return "document"


async def _download(client, item, saved: list[Path]) -> dict:
    if len(saved) >= MAX_FILES:
        raise StudioError("В одном импорте допускается не больше 30 файлов.", status=413)
    kind = "photo" if isinstance(item, types.Photo) else _document_kind(item)
    ext = {
        "photo": ".jpg",
        "video": ".mp4",
        "animation": ".mp4",
        "audio": ".mp3",
        "voice_note": ".ogg",
        "document": ".bin",
    }[kind]
    attr = next((a for a in getattr(item, "attributes", []) if isinstance(a, types.DocumentAttributeFilename)), None)
    if attr:
        # Preserve display filename/extension, but never the upstream directory or an existing upload.
        ext = "-" + media.free_name(attr.file_name)
    path = media.media_dir() / ("import-" + secrets.token_hex(12) + ext)
    part = path.with_name("." + path.name + ".part")
    total = sum(p.stat().st_size for p in saved)
    limit = min(int(settings.get().max_upload_mb * 1024 * 1024), MAX_TOTAL_BYTES - total)
    if getattr(item, "size", 0) > limit:
        raise StudioError("Медиа превышает лимит размера импорта.", status=413)
    size = 0
    try:
        with part.open("xb") as file:
            async for chunk in client.iter_download(item):
                size += len(chunk)
                if size > limit:
                    raise StudioError("Медиа превышает лимит размера импорта.", status=413)
                media.check_capacity(len(chunk))
                file.write(chunk)
        if not size:
            raise Invalid("Telegram вернул пустой файл.")
        part.rename(path)
        saved.append(path)
        return {"type": kind, "media": "upload:" + path.name}
    finally:
        part.unlink(missing_ok=True)


def _mt_entities(values: list) -> list:
    names = {"Strike": "strikethrough", "TextUrl": "text_link", "MentionName": "text_mention"}
    result = []
    for value in values:
        e = value.to_dict()
        name = e.pop("_").removeprefix("MessageEntity")
        e["type"] = names.get(name, re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower())
        if "document_id" in e:
            e["custom_emoji_id"] = str(e.pop("document_id"))
        result.append(e)
    return result


async def _message_blocks(client, message, saved: list[Path], warnings: list[str]) -> list:
    if getattr(message, "noforwards", False) or getattr(message, "ttl_period", None):
        raise StudioError("Импорт защищённых или исчезающих сообщений недоступен.", status=403)
    if message.rich_message:
        rich = message.rich_message
        data = rich.to_dict()
        needed: set[str] = set()

        def collect(value):
            if isinstance(value, list):
                for child in value:
                    collect(child)
            elif isinstance(value, dict):
                for key, child in value.items():
                    if value.get("_") in {
                        "PageBlockPhoto",
                        "PageBlockVideo",
                        "PageBlockAudio",
                        "PageBlockDocument",
                    } and key in {"photo_id", "video_id", "audio_id", "document_id"}:
                        needed.add(str(child))
                    elif isinstance(child, (dict, list)):
                        collect(child)

        collect(data["blocks"])
        files = {}
        for item in [*(rich.photos or []), *(rich.documents or [])]:
            if isinstance(item, (types.Photo, types.Document)) and str(item.id) in needed:
                files[str(item.id)] = await _download(client, item, saved)
        converter = Converter(files)
        blocks = converter.blocks(data["blocks"])
        warnings.extend(converter.warnings)
        return blocks
    blocks = []
    if message.message:
        blocks.append(
            {
                "type": "paragraph",
                "text": entities_text(message.message, _mt_entities(message.entities or []), warnings),
            }
        )
    attachment = message.media
    item = getattr(attachment, "photo", None) or getattr(attachment, "document", None)
    if isinstance(item, (types.Photo, types.Document)):
        if getattr(attachment, "ttl_seconds", None):
            raise StudioError("Импорт исчезающих медиа недоступен.", status=403)
        result = await _download(client, item, saved)
        block = {
            "type": result["type"],
            result["type"]: {**result, "has_spoiler": bool(getattr(attachment, "spoiler", False))},
        }
        blocks.insert(0, block)
    elif attachment and not isinstance(attachment, (types.MessageMediaWebPage, types.MessageMediaEmpty)):
        warnings.append(f"Вложение {type(attachment).__name__} не импортировано.")
        blocks.append({"type": "paragraph", "text": "[Вложение не импортировано]"})
    return blocks


async def _mtproto(link: MessageLink, saved: list[Path]) -> tuple[list, list[str]]:
    path = session_path()
    session = path.read_text("utf-8").strip() if path.is_file() else ""
    if not session and link.channel_id:
        raise StudioError("Приватные ссылки доступны только через собственное подключение Telegram.", status=403)
    async with import_client.connection(session, account_context.user_id()) as (client, is_bot):
        peer = await client.get_entity(link.username or types.PeerChannel(link.channel_id))
        if not isinstance(peer, types.Channel):
            raise Invalid("Ссылка должна указывать на сообщение канала или супергруппы.")
        if getattr(peer, "noforwards", False):
            raise StudioError("В исходном чате включена защита содержимого.", status=403)
        if is_bot and (
            not link.username
            or link.username.lower()
            not in [str(peer.username or "").lower(), *[x.username.lower() for x in peer.usernames or []]]
        ):
            raise StudioError("Общее подключение читает только публичные сообщения.", status=403)
        message = await client.get_messages(peer, ids=link.message_id)
        if not isinstance(message, types.Message):
            raise StudioError("Сообщение не найдено или недоступно этому подключению.", status=404)
        if link.topic_id:
            reply = message.reply_to
            topic = (
                (getattr(reply, "reply_to_top_id", None) or getattr(reply, "reply_to_msg_id", None)) if reply else None
            )
            if topic != link.topic_id:
                raise Invalid("Сообщение не принадлежит теме, указанной в ссылке.")
        messages = [message]
        warnings: list[str] = []
        if message.grouped_id and not link.single:
            # Albums contain <= 10 messages; bounded neighbourhood, only the same grouped_id.
            nearby = await client.get_messages(peer, ids=list(range(max(1, message.id - 10), message.id + 11)))
            messages = sorted(
                [m for m in nearby if isinstance(m, types.Message) and m.grouped_id == message.grouped_id],
                key=lambda m: m.id,
            )
            warnings.append("Импортированы доступные элементы альбома рядом с сообщением; проверьте комплектность.")
        blocks = []
        for original in messages:
            msg = original
            if msg.rich_message and msg.rich_message.part:
                response = await client(functions.messages.GetRichMessageRequest(peer=peer, id=msg.id))
                full = next((m for m in response.messages if isinstance(m, types.Message) and m.id == msg.id), None)
                if not full or not full.rich_message or full.rich_message.part:
                    raise StudioError("Telegram вернул только часть rich-поста. Импорт отменён.", status=502)
                msg = full
            blocks.extend(await _message_blocks(client, msg, saved, warnings))
            if msg.reply_markup:
                warnings.append("Кнопки исходного сообщения не перенесены: их действия принадлежат другому боту.")
        return blocks, warnings


async def import_post(url: str) -> dict:
    link = parse_link(url)
    uid = account_context.user_id()
    if uid in _active or len(_active) >= 4:
        raise StudioError("Импорт уже выполняется. Дождитесь завершения.", status=429)
    saved: list[Path] = []
    _active.add(uid)
    try:
        async with asyncio.timeout(DEADLINE):
            cfg = settings.get()
            if cfg.import_api_id and cfg.import_api_hash:
                blocks, warnings = await _mtproto(link, saved)
                return save_result(blocks, warnings, link, "mtproto")
            widget = await import_web.fetch_widget(link)
            blocks = [{"type": "paragraph", "text": widget.root}] if widget.root else []
            if len(widget.media_urls) > MAX_FILES:
                raise StudioError("В одном импорте допускается не больше 30 файлов.", status=413)
            for file_url in widget.media_urls:
                item = await media.fetch(file_url, "import-" + secrets.token_hex(12) + ".jpg", public_only=True)
                saved.append(media.path_of(item["name"]))
                if sum(p.stat().st_size for p in saved) > MAX_TOTAL_BYTES:
                    raise StudioError("Медиа превышает лимит 100 МБ на импорт.", status=413)
                blocks.append({"type": "photo", "photo": {"type": "photo", "media": item["ref"]}})
            return save_result(
                blocks, ["Импорт из веб-виджета: проверьте полноту содержимого и медиа.", *widget.warnings], link, "web"
            )
    except BaseException as error:
        for path in saved:
            path.unlink(missing_ok=True)
        if isinstance(error, mt_errors.FloodWaitError):
            raise StudioError(f"Telegram ограничил запросы. Повторите через {error.seconds} с.", status=429) from None
        if isinstance(error, (mt_errors.RPCError, OSError, ValueError, TimeoutError, httpx.HTTPError)):
            raise StudioError(
                "Не удалось прочитать сообщение через Telegram. Проверьте подключение и доступ или перешлите пост боту.",
                status=502,
            ) from None
        raise
    finally:
        _active.discard(uid)


def from_forward(message: dict, link: MessageLink, warnings: list[str] | None = None) -> dict:
    """Bot API file_ids are valid for this bot; importing a received forward sends nothing."""
    if message.get("has_protected_content"):
        raise Invalid("Импорт защищённого содержимого недоступен.")
    warnings = list(warnings or [])
    if rich := message.get("rich_message"):
        blocks = spec.to_input_blocks(spec.current, rich.get("blocks", []))

        def without_actions(value):
            if isinstance(value, list):
                return [without_actions(x) for x in value]
            if isinstance(value, dict):
                if value.get("type") in {"buttons", "button"}:
                    warnings.append("Кнопки исходного бота заменены отметками; их действия не перенесены.")
                    return (
                        {"type": "paragraph", "text": "[Кнопки исходного бота]"}
                        if value["type"] == "buttons"
                        else "[Кнопка исходного бота]"
                    )
                return {key: without_actions(child) for key, child in value.items()}
            return value

        blocks = without_actions(blocks)
    else:
        blocks = []
        text = message.get("text") or message.get("caption")
        if text:
            blocks.append(
                {
                    "type": "paragraph",
                    "text": entities_text(
                        text, message.get("entities") or message.get("caption_entities") or [], warnings
                    ),
                }
            )
        for kind in ("animation", "photo", "video", "audio", "voice", "document"):
            if item := message.get(kind):
                if isinstance(item, list):
                    item = max(item, key=lambda p: p.get("width", 0) * p.get("height", 0))
                target = "voice_note" if kind == "voice" else kind
                blocks.insert(0, {"type": target, target: {"type": target, "media": item["file_id"]}})
                break
    if message.get("media_group_id"):
        warnings.append("Пересылка импортирует одно сообщение альбома. Для всего альбома используйте импорт по ссылке.")
    if message.get("reply_markup"):
        warnings.append("Кнопки исходного бота не перенесены.")
    warnings.append("Содержимое получено из выбранной вами пересылки.")
    return save_result(blocks, warnings, link, "forward")
