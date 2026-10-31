"""Custom emoji packs: the editor panel inserts entities, the preview shows real pictures.

The Bot API covers everything without MTProto: getStickerSet(short_name) lists a set,
getCustomEmojiStickers + getFile download the pictures. Packs live in the account's meta,
pictures are cached under the account directory — outside media/, so storage limits and
housekeeping never touch them (an emoji pack is configuration, not user uploads).
"""

import json
import re

import settings
import telegram as tg
from errors import Invalid, NotFound, Upstream
from store.meta import get_meta, set_meta

PACKS_KEY = "emoji-packs"
PACK_KEY = "emoji-pack:{}"
MAX_PACKS = 20
_SHORT_RE = re.compile(r"[A-Za-z0-9_]{1,64}")
_LINK_RE = re.compile(r"(?:https?://)?t\.me/addemoji/([A-Za-z0-9_]+)")
_ID_RE = re.compile(r"\d{1,32}")
# (extension, media type, pick this file when …); animated keep their thumbnail for the static view
KINDS = {"webp": "image/webp", "webm": "video/webm", "tgs": "application/gzip"}


def normalize(name: str) -> str:
    """Accept t.me/addemoji/<name> (with or without scheme) or a bare short name."""
    name = (name or "").strip().lstrip("@")
    m = _LINK_RE.fullmatch(name)
    return m.group(1) if m else name


def pack_names() -> list[str]:
    raw = get_meta(PACKS_KEY)
    return json.loads(raw) if raw else []


def list_packs() -> list[dict]:
    packs = []
    for name in pack_names():
        raw = get_meta(PACK_KEY.format(name))
        if raw:
            packs.append(json.loads(raw))
    return packs


def find_sticker(custom_emoji_id: str) -> dict | None:
    """The pack entry (emoji char) for an id, or None: the preview uses it as the fallback text."""
    for pack in list_packs():
        for st in pack.get("stickers", []):
            if st.get("custom_emoji_id") == custom_emoji_id:
                return st
    return None


async def add_pack(name: str) -> dict:
    short = normalize(name)
    if not _SHORT_RE.fullmatch(short):
        raise Invalid("Нужен короткий код набора или ссылка вида t.me/addemoji/<код>.")
    try:
        s = await tg.call("getStickerSet", {"name": short})
    except tg.TelegramError as e:
        raise NotFound(f"Набор «{short}» не найден: {e.description}") from e
    if not isinstance(s, dict) or s.get("sticker_type") != "custom_emoji":
        raise Invalid(f"«{short}» — обычные стикеры, а не набор кастомных эмодзи.")
    stickers = [
        {
            "custom_emoji_id": st["custom_emoji_id"],
            "emoji": st.get("emoji", ""),
            # the panel and the preview pick the renderer by kind: webm-video, tgs-lottie or a static webp
            "kind": "video" if st.get("is_video") else "tgs" if st.get("is_animated") else "static",
        }
        for st in s.get("stickers", [])
        if isinstance(st, dict) and st.get("custom_emoji_id")
    ]
    if not stickers:
        raise Invalid(f"В наборе «{short}» нет кастомных эмодзи.")
    pack = {"short_name": s["name"], "title": s.get("title", short), "stickers": stickers}
    names = pack_names()
    if s["name"] not in names:
        if len(names) >= MAX_PACKS:
            raise Invalid(f"Паков уже {MAX_PACKS}: уберите один, чтобы добавить новый.")
        set_meta(PACKS_KEY, json.dumps([*names, s["name"]]))
    set_meta(PACK_KEY.format(s["name"]), json.dumps(pack, ensure_ascii=False))
    return pack


def remove_pack(name: str) -> None:
    short = normalize(name)
    names = pack_names()
    if short not in names:
        raise NotFound(f"Пака «{short}» нет.")
    set_meta(PACKS_KEY, json.dumps([n for n in names if n != short]))
    # cached pictures stay on disk: a re-added pack shows them again without downloading


def _cache_dir() -> "re.Pattern[str] | None":
    return None  # placeholder replaced below


async def _sticker_of(custom_emoji_id: str) -> dict:
    try:
        result = await tg.call("getCustomEmojiStickers", {"custom_emoji_ids": [custom_emoji_id]})
    except tg.TelegramError as e:
        raise Upstream(f"Telegram не отдал эмодзи: {e.description}") from e
    if not isinstance(result, list) or not result or not isinstance(result[0], dict):
        raise NotFound("Telegram не знает такой кастомный эмодзи.")
    return result[0]


def _file_id_for(st: dict, full: bool) -> tuple[str, str]:
    """(file_id, extension) — the animated video or Lottie for full, a webp thumbnail for the static view."""
    if full:
        if st.get("is_video"):
            return st["file_id"], "webm"
        if st.get("is_animated"):
            return st["file_id"], "tgs"
    thumb = st.get("thumbnail") or {}
    if thumb.get("file_id"):
        return thumb["file_id"], "webp"
    return st["file_id"], "webp"


async def _cached(custom_emoji_id: str, full: bool) -> tuple[str, str]:
    """A cached file for the id, downloading it once on a miss; returns (path, media_type)."""
    if not _ID_RE.fullmatch(custom_emoji_id):
        raise NotFound("Нет такого кастомного эмодзи.")
    prefix = "full" if full else "thumb"
    for ext, media_type in KINDS.items():
        p = settings.get().emoji_cache_dir / f"{custom_emoji_id}.{prefix}.{ext}"
        if p.is_file():
            return str(p), media_type
    st = await _sticker_of(custom_emoji_id)
    file_id, ext = _file_id_for(st, full)
    data = await tg.download_file(file_id)
    path = settings.get().emoji_cache_dir / f"{custom_emoji_id}.{prefix}.{ext}"
    tmp = settings.get().emoji_cache_dir / f"{custom_emoji_id}.{prefix}.{ext}.part"
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp.write_bytes(data)
    tmp.replace(path)
    return str(path), KINDS[ext]


async def static_file(custom_emoji_id: str) -> tuple[str, str]:
    return await _cached(custom_emoji_id, full=False)


async def full_file(custom_emoji_id: str) -> tuple[str, str]:
    return await _cached(custom_emoji_id, full=True)
