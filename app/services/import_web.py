"""Bounded public-widget fallback; a Telegram placeholder is an error, not imported content."""

from html.parser import HTMLParser

import httpx

from errors import StudioError
from services import media
from services.import_links import MessageLink
from spec.import_content import safe_link


class Widget(HTMLParser):
    def __init__(self, expected: str):
        super().__init__(convert_charrefs=True)
        self.expected = expected
        self.matched = False
        self.unsupported = False
        self.depth = 0
        self.root: list = []
        self.stack: list[list] = [self.root]
        self.open_tags: list[str] = []
        self.media_urls: list[str] = []
        self.warnings: list[str] = []

    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        classes = (a.get("class") or "").split()
        if "tgme_widget_message" in classes:
            self.matched = a.get("data-post") == self.expected
        if not self.matched:
            return
        if "message_media_not_supported" in classes or "tgme_widget_message_error" in classes:
            self.unsupported = True
        if "tgme_widget_message_photo_wrap" in classes:
            # CSS isn't interpreted: only a single HTTPS background URL is considered for download.
            import re

            match = re.search(r"background-image:\s*url\(['\"]?(https://[^)'\"\s]+)", a.get("style") or "")
            if match:
                self.media_urls.append(match[1])
        if tag in {"video", "audio"}:
            self.warnings.append("Видео/аудио виджета не импортировано. Для полного импорта подключите Telegram.")
        if "tgme_widget_message_text" in classes and not self.depth:
            self.depth = 1
            self.open_tags = [tag]
            return
        if not self.depth:
            return
        if tag == "br":
            self.stack[-1].append("\n")
            return
        if tag in {"img", "input", "hr", "source", "wbr"}:
            return
        self.depth += 1
        self.open_tags.append(tag)
        kinds = {
            "b": "bold",
            "strong": "bold",
            "i": "italic",
            "em": "italic",
            "u": "underline",
            "s": "strikethrough",
            "del": "strikethrough",
            "code": "code",
            "pre": "code",
            "tg-spoiler": "spoiler",
        }
        children: list = []
        if tag in kinds:
            self.stack[-1].append({"type": kinds[tag], "text": children})
        elif tag == "a" and safe_link(a.get("href") or ""):
            self.stack[-1].append({"type": "url", "url": a["href"], "text": children})
        else:
            self.stack[-1].append(children)
        self.stack.append(children)

    def handle_endtag(self, tag):
        if self.depth and self.open_tags and tag == self.open_tags[-1]:
            self.open_tags.pop()
            self.depth -= 1
            if self.depth:
                self.stack.pop()

    def handle_data(self, data):
        if self.depth and not any(t in {"script", "style"} for t in self.open_tags):
            self.stack[-1].append(data)


async def fetch_widget(link: MessageLink) -> Widget:
    if not link.username:
        raise StudioError("Для приватной ссылки требуется собственное подключение Telegram.", status=409)
    # Topic ids are not part of the widget route. Never fetch the caller's original URL.
    url = f"https://t.me/{link.username}/{link.message_id}?embed=1" + ("&single=1" if link.single else "")
    proxy = media._proxy_for("https", "t.me")
    ip = await media._public_only("t.me", 443, proxied=proxy is not None, force=True)
    target, headers, extensions = media._pinned(url, None if proxy else ip)
    async with (
        httpx.AsyncClient(
            timeout=15,
            follow_redirects=False,
            proxy=proxy,
            trust_env=False,
            verify=httpx.create_ssl_context(trust_env=True),
        ) as client,
        client.stream("GET", target, headers=headers, extensions=extensions) as response,
    ):
        if response.status_code != 200:
            raise StudioError("Telegram не отдал публичный предпросмотр сообщения.", status=502)
        body = bytearray()
        async for chunk in response.aiter_bytes():
            body.extend(chunk)
            if len(body) > 2 * 1024 * 1024:
                raise StudioError("Предпросмотр Telegram превышает лимит 2 МБ.", status=413)
    widget = Widget(f"{link.username}/{link.message_id}")
    widget.feed(body.decode("utf-8", errors="replace"))
    if not widget.matched or widget.unsupported or not (widget.root or widget.media_urls):
        raise StudioError(
            {
                "code": "telegram_connection_required",
                "message": "Telegram не отдаёт содержимое этого поста через веб-виджет. Подключите Telegram для импорта или перешлите сообщение боту студии.",
            },
            status=409,
        )
    return widget
