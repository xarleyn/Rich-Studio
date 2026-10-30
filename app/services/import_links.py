"""Strict Telegram message references, never arbitrary fetch destinations."""

import re
from dataclasses import asdict, dataclass
from urllib.parse import parse_qsl, urlsplit

from errors import Invalid

MAX_ID = 2**31 - 1
RESERVED = {"c", "s", "share", "joinchat", "addstickers", "proxy", "socks", "login", "addlist", "boost"}


@dataclass(frozen=True)
class MessageLink:
    username: str | None
    channel_id: int | None
    message_id: int
    topic_id: int | None = None
    single: bool = False

    @property
    def url(self) -> str:
        peer = self.username or f"c/{self.channel_id}"
        topic = f"{self.topic_id}/" if self.topic_id else ""
        return f"https://t.me/{peer}/{topic}{self.message_id}" + ("?single" if self.single else "")

    def describe(self) -> dict:
        return {**asdict(self), "url": self.url}


def parse_link(raw: str) -> MessageLink:
    """Accept only message links; reject ambiguous paths and unsupported comment targets."""
    message = "Нужна ссылка https://t.me/канал/сообщение или https://t.me/канал/тема/сообщение."
    if not isinstance(raw, str) or len(raw) > 2048 or re.search(r"[\x00-\x20\x7f\\%]", raw.strip()):
        raise Invalid(message)
    raw = raw.strip()
    if raw.startswith("t.me/"):
        raw = "https://" + raw
    try:
        u = urlsplit(raw)
        if u.scheme != "https" or u.netloc.lower() != "t.me" or u.fragment:
            raise ValueError
        parts = u.path.removeprefix("/").split("/")
        if parts[-1] == "":
            parts.pop()
        username, channel_id = None, None
        if parts[0] == "c":
            if len(parts) not in (3, 4) or not re.fullmatch(r"[1-9][0-9]{0,12}", parts[1]):
                raise ValueError
            channel_id = int(parts[1])
            ids = parts[2:]
        else:
            if len(parts) not in (2, 3) or not re.fullmatch(r"[A-Za-z][A-Za-z0-9_]{3,31}", parts[0]):
                raise ValueError
            username = parts[0].lower()
            if username in RESERVED:
                raise ValueError
            ids = parts[1:]
        if any(not re.fullmatch(r"[1-9][0-9]{0,9}", v) or int(v) > MAX_ID for v in ids):
            raise ValueError
        query = parse_qsl(u.query, keep_blank_values=True, strict_parsing=False)
        if len({k for k, _ in query}) != len(query):
            raise ValueError
        topic = int(ids[0]) if len(ids) == 2 else None
        single = False
        for key, value in query:
            if key == "single" and value in ("", "1"):
                single = True
            elif key == "thread" and re.fullmatch(r"[1-9][0-9]{0,9}", value) and int(value) <= MAX_ID:
                if topic is not None and topic != int(value):
                    raise ValueError
                topic = int(value)
            elif key == "t" and re.fullmatch(r"[0-9hms:]{1,20}", value):
                pass  # playback position does not change the imported message
            else:
                raise ValueError
        return MessageLink(username, channel_id, int(ids[-1]), topic, single)
    except (ValueError, IndexError):
        raise Invalid(message + " Параметры комментариев и других действий не поддерживаются.") from None
