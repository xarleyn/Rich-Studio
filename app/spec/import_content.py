"""Telegram MTProto content → editable Bot API blocks. Unknown content is visible, never silently dropped.

This is an explicit protocol adapter, not an editor schema. The editor still uses api.json.
"""

import re
from html import escape
from itertools import pairwise
from typing import Any
from urllib.parse import urlsplit

from errors import Invalid

INLINE = {
    "Bold": "bold",
    "Italic": "italic",
    "Underline": "underline",
    "Strike": "strikethrough",
    "Spoiler": "spoiler",
    "Fixed": "code",
    "Subscript": "subscript",
    "Superscript": "superscript",
    "Marked": "marked",
}
TAGS = {
    "bold": "b",
    "italic": "i",
    "underline": "u",
    "strikethrough": "s",
    "spoiler": "tg-spoiler",
    "code": "code",
    "subscript": "sub",
    "superscript": "sup",
    "marked": "mark",
}


def safe_link(url: str) -> str:
    if not isinstance(url, str) or re.search(r"[\x00-\x20\x7f\\]", url):
        return ""
    try:
        p = urlsplit(url)
        if p.scheme in {"http", "https"} and p.hostname and not p.username and not p.password:
            return url
        if p.scheme in {"mailto", "tel"} and p.path:
            return url
        if url.startswith("#"):
            return url
    except ValueError:
        pass
    return ""


def plain(text) -> str:
    if isinstance(text, str):
        return text
    if isinstance(text, list):
        return "".join(plain(x) for x in text)
    if isinstance(text, dict):
        return plain(text.get("text", text.get("alternative_text", text.get("expression", ""))))
    return ""


class Converter:
    def __init__(self, media: dict[str, dict] | None = None):
        self.media = media or {}
        self.warnings: list[str] = []
        self.nodes = 0

    def warn(self, message: str):
        if message not in self.warnings:
            self.warnings.append(message)

    def budget(self, depth: int):
        self.nodes += 1
        if depth > 40 or self.nodes > 10000:
            raise Invalid("Сообщение слишком сложное для импорта (глубина или число элементов).")

    def text(self, value, depth=0):
        self.budget(depth)
        if isinstance(value, str):
            return value
        if not isinstance(value, dict):
            return ""
        name = value.get("_", "").removeprefix("Text")
        if name == "Empty":
            return ""
        if name == "Concat":
            return [self.text(x, depth + 1) for x in value.get("texts", [])]
        if name == "Plain":
            return str(value.get("text", ""))
        if name in INLINE:
            return {"type": INLINE[name], "text": self.text(value.get("text"), depth + 1)}
        if name == "CustomEmoji":
            return {
                "type": "custom_emoji",
                "custom_emoji_id": str(value["document_id"]),
                "alternative_text": value["alt"],
            }
        if name == "Math":
            return {"type": "mathematical_expression", "expression": value["source"]}
        text = self.text(value.get("text"), depth + 1)
        if name in {"Url", "Email", "Phone", "AutoUrl", "AutoEmail", "AutoPhone"}:
            url = value.get("url") or ("mailto:" + value["email"] if name == "Email" else "")
            url = url or ("tel:" + value["phone"] if name == "Phone" else "")
            url = url or ({"AutoEmail": "mailto:", "AutoPhone": "tel:"}.get(name, "") + plain(text))
            if safe_link(url):
                return {"type": "url", "text": text, "url": url}
            self.warn("Небезопасная ссылка заменена обычным текстом.")
            return text
        if name == "Anchor":
            return [{"type": "anchor", "name": value["name"]}, text]
        if name in {"Mention", "Hashtag", "Cashtag", "BotCommand", "BankCard"}:
            return text
        self.warn(f"Форматирование {value.get('_', 'неизвестное')} перенесено как текст.")
        return text or f"[Неподдерживаемый элемент: {value.get('_', '?')}]"

    def caption(self, value, depth):
        value = value or {}
        return {"text": self.text(value.get("text"), depth + 1), "credit": self.text(value.get("credit"), depth + 1)}

    def blocks(self, values: list, depth=0) -> list:
        return [self.block(v, depth + 1) for v in values]

    def block(self, v: dict, depth: int) -> dict:
        self.budget(depth)
        name = v.get("_", "").removeprefix("PageBlock")

        def text():
            return self.text(v.get("text"), depth + 1)

        if name in {"Paragraph", "Footer", "Thinking"}:
            return {"type": name.lower(), "text": text()}
        if name.startswith("Heading") or name in {"Title", "Header", "Subtitle", "Subheader", "Kicker"}:
            size = int(name[-1]) if re.fullmatch(r"Heading[1-6]", name) else (1 if name in {"Title", "Header"} else 2)
            return {"type": "heading", "size": size, "text": text()}
        if name == "Preformatted":
            return {"type": "pre", "text": text(), "language": v.get("language", "")}
        if name == "Divider":
            return {"type": "divider"}
        if name == "Anchor":
            return {"type": "anchor", "name": v["name"]}
        if name == "Math":
            return {"type": "mathematical_expression", "expression": v["source"]}
        if name in {"Blockquote", "BlockquoteBlocks", "Pullquote"}:
            credit = self.text(v.get("caption"), depth + 1)
            if name == "Pullquote" or v.get("collapsed"):
                return {
                    "type": "pullquote" if name == "Pullquote" else "expandable_blockquote",
                    "text": text(),
                    "credit": credit,
                }
            blocks = self.blocks(v["blocks"], depth) if "blocks" in v else [{"type": "paragraph", "text": text()}]
            return {"type": "blockquote", "blocks": blocks, "credit": credit}
        if name in {"List", "OrderedList"}:
            items = []
            counter = v.get("start") or (len(v["items"]) if v.get("reversed") else 1)
            for item in v["items"]:
                blocks = (
                    self.blocks(item["blocks"], depth)
                    if "blocks" in item
                    else [{"type": "paragraph", "text": self.text(item.get("text"), depth + 1)}]
                )
                out: dict = {
                    "blocks": blocks,
                    "has_checkbox": bool(item.get("checkbox")),
                    "is_checked": bool(item.get("checked")),
                }
                if name == "OrderedList":
                    number = item.get("value")
                    if number is None and str(item.get("num", "")).isdigit():
                        number = int(item["num"])
                    counter = counter if number is None else number
                    out.update(value=counter, type=item.get("type") or v.get("type") or "1")
                    counter += -1 if v.get("reversed") else 1
                items.append(out)
            return {"type": "list", "items": items}
        if name in {"Collage", "Slideshow"}:
            return {
                "type": name.lower(),
                "blocks": self.blocks(v["items"], depth),
                "caption": self.caption(v.get("caption"), depth),
            }
        if name == "Cover":
            return self.block(v["cover"], depth + 1)
        if name == "Details":
            return {
                "type": "details",
                "summary": self.text(v["title"], depth + 1),
                "blocks": self.blocks(v["blocks"], depth),
                "is_open": bool(v.get("open")),
            }
        if name == "Table":
            rows = []
            for row in v["rows"]:
                cells = []
                for c in row["cells"]:
                    cell = {
                        "align": "right" if c.get("align_right") else "center" if c.get("align_center") else "left",
                        "valign": "bottom" if c.get("valign_bottom") else "middle" if c.get("valign_middle") else "top",
                        "is_header": bool(c.get("header")),
                    }
                    if c.get("text") is not None:
                        cell["text"] = self.text(c["text"], depth + 1)
                    for key in ("colspan", "rowspan"):
                        if c.get(key):
                            cell[key] = c[key]
                    cells.append(cell)
                rows.append(cells)
            return {
                "type": "table",
                "cells": rows,
                "caption": self.text(v["title"], depth + 1),
                "is_bordered": bool(v.get("bordered")),
                "is_striped": bool(v.get("striped")),
                "is_compact": bool(v.get("compact")),
            }
        if name == "Map":
            return {
                "type": "map",
                "location": {"latitude": v["geo"]["lat"], "longitude": v["geo"]["long"]},
                "zoom": v["zoom"],
                "width": v["w"],
                "height": v["h"],
                "caption": self.caption(v.get("caption"), depth),
            }
        if name in {"Photo", "Video", "Audio", "Document"}:
            key = name.lower() + "_id"
            item = self.media.get(str(v[key]))
            if item:
                kind = item["type"]
                return {
                    "type": kind,
                    kind: {**item, "has_spoiler": bool(v.get("spoiler"))},
                    "caption": self.caption(v.get("caption"), depth),
                }
        self.warn(f"Блок {v.get('_', '?')} не перенесён: в редакторе оставлена отметка.")
        return {"type": "paragraph", "text": f"[Не импортировано: {v.get('_', 'неизвестный блок')}]"}


def entities_text(text: str, entities: list, warnings: list[str]):
    """UTF-16 entity offsets, including astral emoji; split crossing ranges without losing text."""
    if len(text) > 100000 or len(entities) > 2000:
        raise Invalid("Слишком много текста или диапазонов форматирования для импорта.")
    raw = text.encode("utf-16-le")
    boundaries = {0, len(raw) // 2}
    valid = []
    names = {
        "bold": "bold",
        "italic": "italic",
        "underline": "underline",
        "strikethrough": "strikethrough",
        "spoiler": "spoiler",
        "code": "code",
        "pre": "code",
    }
    for e in entities:
        a, b = e.get("offset", -1), e.get("offset", -1) + e.get("length", 0)
        try:
            if not 0 <= a < b <= len(raw) // 2:
                raise ValueError
            raw[: a * 2].decode("utf-16-le")
            raw[: b * 2].decode("utf-16-le")
        except (ValueError, UnicodeDecodeError):
            warnings.append("Некорректный диапазон форматирования пропущен.")
            continue
        valid.append((a, b, e))
        boundaries.update((a, b))
    out = []
    points = sorted(boundaries)
    for a, b in pairwise(points):
        chunk = raw[a * 2 : b * 2].decode("utf-16-le")
        node: Any = chunk
        for start, end, e in sorted(valid, key=lambda v: v[1] - v[0]):
            if start <= a and end >= b:
                kind = e.get("type")
                if kind in names:
                    node = {"type": names[kind], "text": node}
                elif kind in {"text_link", "url"}:
                    url = e.get("url") or raw[start * 2 : end * 2].decode("utf-16-le")
                    if safe_link(url):
                        node = {"type": "url", "url": url, "text": node}
                elif kind == "custom_emoji":
                    node = {
                        "type": "custom_emoji",
                        "custom_emoji_id": str(e["custom_emoji_id"]),
                        "alternative_text": chunk,
                    }
                elif kind not in {"mention", "hashtag", "cashtag", "bot_command", "email", "phone_number"}:
                    warnings.append(f"Форматирование {kind} перенесено как текст.")
        out.append(node)
    return out or text


def text_html(value) -> str:
    if isinstance(value, str):
        return escape(value).replace("\n", "<br>")
    if isinstance(value, list):
        return "".join(text_html(x) for x in value)
    if not isinstance(value, dict):
        return ""
    kind = value.get("type")
    content = text_html(value.get("text", ""))
    if kind in TAGS:
        return f"<{TAGS[kind]}>{content}</{TAGS[kind]}>"
    if kind == "url" and safe_link(value.get("url", "")):
        return f'<a href="{escape(value["url"], quote=True)}">{content}</a>'
    if kind == "custom_emoji":
        return f'<tg-emoji emoji-id="{escape(value["custom_emoji_id"], quote=True)}">{escape(value["alternative_text"])}</tg-emoji>'
    if kind == "mathematical_expression":
        return f"<tg-math>{escape(value['expression'])}</tg-math>"
    if kind == "anchor":
        return f'<a name="{escape(value["name"], quote=True)}"></a>'
    return content or escape(plain(value))


def representations(blocks: list, warnings: list[str]) -> tuple[str, str, list]:
    """Rich Markdown uses native syntax for simple blocks, supported HTML for complex ones."""
    media: list[dict] = []

    def audit(value, depth=0):
        if depth > 40:
            raise Invalid("Слишком глубокая структура содержимого.")
        if isinstance(value, list):
            for child in value:
                audit(child, depth + 1)
        elif isinstance(value, dict):
            known = {
                *TAGS,
                "url",
                "custom_emoji",
                "mathematical_expression",
                "anchor",
                "paragraph",
                "heading",
                "footer",
                "thinking",
                "pre",
                "divider",
                "map",
                "blockquote",
                "expandable_blockquote",
                "pullquote",
                "list",
                "details",
                "collage",
                "slideshow",
                "table",
                "photo",
                "video",
                "audio",
                "animation",
                "voice_note",
                "document",
                "1",
                "a",
                "A",
                "i",
                "I",
            }
            if value.get("type") and value["type"] not in known:
                warnings.append(f"HTML/Markdown: оформление {value['type']} упрощено; проверьте визуальный оригинал.")
            for child in value.values():
                if isinstance(child, (dict, list)):
                    audit(child, depth + 1)

    audit(blocks)

    def caption(c):
        if not c:
            return ""
        return (
            "<figcaption>"
            + text_html(c.get("text"))
            + ("<cite>" + text_html(c["credit"]) + "</cite>" if c.get("credit") else "")
            + "</figcaption>"
        )

    def render(b):
        kind = b["type"]
        text = text_html(b.get("text"))
        if kind in {"paragraph", "heading", "footer", "thinking"}:
            tag = f"h{b['size']}" if kind == "heading" else "footer" if kind == "footer" else "p"
            return f"<{tag}>{text}</{tag}>"
        if kind == "pre":
            return (
                '<pre><code class="language-'
                + escape(b.get("language", ""), quote=True)
                + '">'
                + escape(plain(b["text"]))
                + "</code></pre>"
            )
        if kind == "map":
            loc = b["location"]
            return (
                f'<figure><tg-map lat="{float(loc["latitude"])}" long="{float(loc["longitude"])}"'
                f' zoom="{int(b.get("zoom", 12))}"></tg-map>' + caption(b.get("caption")) + "</figure>"
            )
        if kind == "divider":
            return "<hr>"
        if kind == "anchor":
            return '<a name="' + escape(b["name"], quote=True) + '"></a>'
        if kind == "mathematical_expression":
            return "<tg-math-block>" + escape(b["expression"]) + "</tg-math-block>"
        if kind in {"blockquote", "expandable_blockquote", "pullquote"}:
            tag = "aside" if kind == "pullquote" else "blockquote"
            attr = " expandable" if kind == "expandable_blockquote" else ""
            body = "".join(render(x) for x in b.get("blocks", [])) or text
            credit = "<cite>" + text_html(b["credit"]) + "</cite>" if b.get("credit") else ""
            return f"<{tag}{attr}>{body}{credit}</{tag}>"
        if kind == "list":
            ordered = any("value" in i for i in b["items"])
            tag = "ol" if ordered else "ul"
            rows = []
            for i in b["items"]:
                attr = (
                    f' value="{int(i["value"])}" type="{escape(i.get("type", "1"), quote=True)}"'
                    if "value" in i
                    else ""
                )
                check = (
                    '<input type="checkbox"' + (" checked" if i.get("is_checked") else "") + ">"
                    if i.get("has_checkbox")
                    else ""
                )
                rows.append(f"<li{attr}>" + check + "".join(render(x) for x in i["blocks"]) + "</li>")
            return f"<{tag}>" + "".join(rows) + f"</{tag}>"
        if kind == "details":
            return (
                "<details"
                + (" open" if b.get("is_open") else "")
                + "><summary>"
                + text_html(b["summary"])
                + "</summary>"
                + "".join(render(x) for x in b["blocks"])
                + "</details>"
            )
        if kind in {"collage", "slideshow"}:
            return (
                f"<tg-{kind}>" + "".join(render(x) for x in b["blocks"]) + caption(b.get("caption")) + f"</tg-{kind}>"
            )
        if kind == "table":
            rows = []
            for row in b["cells"]:
                cells = []
                for c in row:
                    if "text" not in c:
                        continue
                    tag = "th" if c.get("is_header") else "td"
                    attrs = "".join(
                        f' {k}="{escape(str(c[k]), quote=True)}"'
                        for k in ("align", "valign", "colspan", "rowspan")
                        if k in c
                    )
                    cells.append(f"<{tag}{attrs}>" + text_html(c["text"]) + f"</{tag}>")
                rows.append("<tr>" + "".join(cells) + "</tr>")
            attrs = "".join(" " + k for k in ("bordered", "striped", "compact") if b.get("is_" + k))
            return f"<table{attrs}><caption>" + text_html(b.get("caption")) + "</caption>" + "".join(rows) + "</table>"
        if kind in {"photo", "video", "audio", "animation", "voice_note", "document"}:
            item = b[kind]
            mid = "import" + str(len(media) + 1)
            media.append({"id": mid, "media": item})
            scheme = "voice" if kind == "voice_note" else kind
            tag = {"photo": "img", "animation": "video", "voice_note": "audio", "document": "tg-document"}.get(
                kind, kind
            )
            attrs = " tg-spoiler" if item.get("has_spoiler") else ""
            elem = f'<{tag} src="tg://{scheme}?id={mid}"{attrs}>' + (f"</{tag}>" if tag != "img" else "")
            return "<figure>" + elem + caption(b.get("caption")) + "</figure>"
        warnings.append(f"HTML/Markdown: блок {kind} заменён отметкой; исходник сохранён в визуальном режиме.")
        return "<p>" + escape(f"[Блок {kind}: доступен в визуальном режиме]") + "</p>"

    html_parts = [render(b) for b in blocks]
    md = []
    for b, html in zip(blocks, html_parts, strict=True):
        # Only plain strings can safely become Markdown; rich inline HTML remains explicit HTML.
        if b["type"] in {"paragraph", "heading"} and isinstance(b.get("text"), str):
            escaped = re.sub(r"([\\`*_{}\[\]()#+.!|>~=$-])", r"\\\1", b["text"])
            escaped = escape(escaped).replace("\n", "  \n")
            md.append(("#" * b["size"] + " " if b["type"] == "heading" else "") + escaped)
        else:
            md.append(html)
    return "\n".join(html_parts), "\n\n".join(md), media
