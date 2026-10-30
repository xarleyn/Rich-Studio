"""Validate reusable post templates and fill them into independent, unpublished drafts."""

import copy
import json
import re
from html import escape
from html.parser import HTMLParser
from urllib.parse import urlsplit

import store
from errors import Conflict, Invalid, StudioError
from store import templates as records

VARIABLE = re.compile(r"\{\{([A-Za-z][A-Za-z0-9_]{0,31})\}\}")
KEY = re.compile(r"[A-Za-z][A-Za-z0-9_]{0,31}")
MARKDOWN_PUNCTUATION = re.compile(r"""[&<>"'\\`*_{}\[\]()#+.!|~=$-]""")
MAX_VALUE = 10000


class _SourceFields(HTMLParser):
    """Placeholders may be text or quoted attribute values, never markup names."""

    def __init__(self, fields: dict[str, dict]):
        super().__init__(convert_charrefs=False)
        self.fields = fields

    def handle_starttag(self, tag, attrs):
        raw = self.get_starttag_text() or ""
        # Remove complete quoted values; any remaining variable is a tag/name or unquoted attribute.
        outside = re.sub(r""""[^"]*"|'[^']*' """, "", raw, flags=re.X)
        if VARIABLE.search(outside):
            raise Invalid("Переменные в HTML разрешены только в тексте и в значениях атрибутов внутри кавычек.")
        for name, value in attrs:
            if name.startswith("on") and value and VARIABLE.search(value):
                raise Invalid("Переменные нельзя использовать в обработчиках HTML.")
            if name in {"href", "src"} and value:
                # A whole destination must be explicitly a URL field. Fixed prefixes cannot become scripts.
                match = VARIABLE.fullmatch(value)
                if match and self.fields[match[1]]["type"] != "url":
                    raise Invalid(f"Для HTML-атрибута {name} поле {match[1]} должно иметь тип «Ссылка».")
                if VARIABLE.search(value) and not match and not value.startswith(("https://", "http://", "tg://", "#")):
                    raise Invalid("Переменная ссылки должна иметь постоянный http(s)/tg-префикс или тип «Ссылка».")

    handle_startendtag = handle_starttag


def _markdown_code_fields(text: str) -> None:
    """Code and math ignore character entities. Reject their variables before changing the content."""
    fence: str | None = None
    fence_quotes = 0
    outside = []
    for line in text.splitlines(keepends=True):
        nested = line
        quotes = 0
        # Strip quote/list containers from fence openings, including nested combinations.
        # Inside a fence list markers are literal code, and only the opening quote depth
        # is structural: a code line '- ~~~' or '> ~~~' must not end a top-level fence.
        while True:
            quote = re.match(r"^[ \t]*>[ \t]?", nested)
            item = re.match(r"^[ \t]*(?:[-+*]|\d{1,9}[.)])[ \t]", nested) if not fence else None
            if quote and (not fence or quotes < fence_quotes):
                nested = nested[quote.end() :]
                quotes += 1
            elif item:
                nested = nested[item.end() :]
            else:
                break
        marker = re.match(r"^[ \t]*(`{3,}|~{3,})", nested)
        if fence:
            if VARIABLE.search(line):
                raise Invalid(
                    "Переменные внутри блоков кода Markdown не поддерживаются. Используйте визуальный блок кода."
                )
            if (
                marker
                and marker[1][0] == fence[0]
                and len(marker[1]) >= len(fence)
                and not nested[marker.end() :].strip()
            ):
                fence = None
            outside.append("\n")
        elif marker:
            if VARIABLE.search(line):
                raise Invalid("Переменные в ограждении блока кода Markdown не поддерживаются.")
            fence = marker[1]
            fence_quotes = quotes
            outside.append("\n")
        else:
            if re.match(r"^( {4}|\t)", nested) and VARIABLE.search(line):
                raise Invalid(
                    "Переменные в коде Markdown с отступом не поддерживаются. Используйте визуальный блок кода."
                )
            outside.append(line)
    source = "".join(outside)
    position = 0
    backticks = re.compile(r"`+")
    while opening := backticks.search(source, position):
        end = opening.end()
        slash = opening.start() - 1
        while slash >= 0 and source[slash] == "\\":
            slash -= 1
        if (opening.start() - slash - 1) % 2:
            position = end
            continue
        delimiter = opening[0]
        closing = re.compile(r"(?<!`)" + re.escape(delimiter) + r"(?!`)").search(source, end)
        if not closing:
            position = end
            continue
        stop = closing.start()
        if VARIABLE.search(source[end:stop]):
            raise Invalid(
                "Переменные внутри строкового кода Markdown не поддерживаются. Используйте визуальное форматирование кода."
            )
        position = closing.end()
    # Conservative paired-dollar detection also covers multiline/block math and nested
    # containers. Escaped dollar signs remain ordinary text. Use visual math or an HTML
    # tg-math element when a formula needs configurable values.
    math_start: int | None = None
    block_math_start: int | None = None
    for dollar in re.finditer(r"\$+", source):
        slash = dollar.start() - 1
        while slash >= 0 and source[slash] == "\\":
            slash -= 1
        if (dollar.start() - slash - 1) % 2:
            continue
        ranges = []
        if len(dollar[0]) >= 2:
            if block_math_start is None:
                block_math_start = dollar.start() + 2
            else:
                ranges.append(block_math_start)
                block_math_start = None
        if math_start is None:
            math_start = dollar.start() + 1
        else:
            ranges.append(math_start)
            math_start = None
        for start in ranges:
            if VARIABLE.search(source[start : dollar.start()]):
                raise Invalid(
                    "Переменные внутри формул Markdown не поддерживаются. Используйте визуальную формулу или HTML-тег tg-math."
                )


def _walk(value, transform, path=(), depth=0):
    if depth > 60:
        raise Invalid("Слишком большая вложенность шаблона.")
    if isinstance(value, str):
        return transform(value, path)
    if isinstance(value, list):
        return [_walk(child, transform, (*path, str(i)), depth + 1) for i, child in enumerate(value)]
    if isinstance(value, dict):
        for key in value:
            if VARIABLE.search(key):
                raise Invalid("Переменные допускаются в значениях, но не в именах полей JSON.")
        return {key: _walk(child, transform, (*path, key), depth + 1) for key, child in value.items()}
    return value


def list_templates() -> list[dict]:
    return records.list_templates()


def get_template(tid: str) -> dict:
    return records.get_template(tid)


def _url(value: str, label: str) -> None:
    try:
        parsed = urlsplit(value)
        valid = parsed.scheme in {"https", "http"} and bool(parsed.hostname)
    except ValueError:
        valid = False
    if not valid or re.search(r"[\s<>\"'\\\x00-\x1f\x7f]", value):
        raise Invalid(f"Поле «{label}»: укажите полную ссылку http:// или https:// без пробелов.")


def _field_value(field: dict, value: str, *, required: bool = True) -> str:
    if not isinstance(value, str) or len(value) > MAX_VALUE:
        raise Invalid(f"Поле «{field['label']}»: допустимо не больше {MAX_VALUE} символов.")
    if required and field["required"] and not value.strip():
        raise Invalid(f"Заполните поле «{field['label']}».")
    if value and field["type"] == "url":
        _url(value, field["label"])
    return value


def save_template(
    title: str, post: dict, fields: list[dict], tid: str | None = None, version: int | None = None
) -> dict:
    title = title.strip()
    if not title or len(title) > 128:
        raise Invalid("Название шаблона должно содержать от 1 до 128 символов.")
    if len(fields) > 50:
        raise Invalid("В шаблоне допускается не больше 50 полей.")
    clean = copy.deepcopy(post)
    clean.pop("_studio", None)  # stale alternative buffers and import metadata are not the template
    rm = clean.get("rich_message")
    if not isinstance(rm, dict) or sum(k in rm for k in ("blocks", "html", "markdown")) != 1:
        raise Invalid("Шаблон должен содержать rich_message и один формат: blocks, html или markdown.")
    used: set[str] = set()

    def collect(text, _path):
        if text.startswith("upload:") and VARIABLE.search(text):
            raise Invalid(
                "Для файла замените всю ссылку upload:имя одним полем {{file}}; задайте полную ссылку в значении по умолчанию."
            )
        used.update(VARIABLE.findall(text))
        return text

    _walk(clean, collect)
    normalized = []
    keys = set()
    for field in fields:
        key = field.get("key", "")
        label = field.get("label", "").strip()
        if not KEY.fullmatch(key) or key in keys:
            raise Invalid(
                "Имена полей должны быть уникальными: латинская буква, затем буквы, цифры или _; до 32 знаков."
            )
        if not label or len(label) > 100 or field.get("type", "text") not in {"text", "multiline", "url"}:
            raise Invalid(f"Проверьте подпись и тип поля {key}.")
        item = {
            "key": key,
            "label": label,
            "type": field.get("type", "text"),
            "required": bool(field.get("required", True)),
            "default": field.get("default", ""),
        }
        _field_value(item, item["default"], required=False)
        normalized.append(item)
        keys.add(key)
    if used - keys:
        raise Invalid("Опишите поля для переменных: " + ", ".join(sorted(used - keys)))
    if keys - used:
        raise Invalid("Эти поля не используются в посте: " + ", ".join(sorted(keys - used)))
    by_key = {field["key"]: field for field in normalized}

    def validate_source(text, path):
        if VARIABLE.search(text) and not VARIABLE.fullmatch(text):
            # Fragmented defaults can synthesize upload: without any literal file reference
            # for media cleanup to see. Inspect only the seven-character prefix: even many
            # repeated defaults must never be expanded just to decide whether this is a file.
            prefix = ""
            position = 0
            for match in VARIABLE.finditer(text):
                prefix += text[position : min(match.start(), position + 7 - len(prefix))]
                if len(prefix) == 7:
                    break
                prefix += by_key[match[1]]["default"][: 7 - len(prefix)]
                position = match.end()
                if len(prefix) == 7:
                    break
            if len(prefix) < 7:
                prefix += text[position : position + 7 - len(prefix)]
            if prefix == "upload:":
                raise Invalid(
                    "Для файла замените всю ссылку upload:имя одним полем {{file}}; задайте полную ссылку в значении по умолчанию."
                )
        if path and path[-1] in {"html", "markdown"}:
            parser = _SourceFields(by_key)
            parser.feed(text)
            parser.close()
            if re.search(r"<\s*/?\s*\{\{", text):
                raise Invalid("Переменные не могут задавать имена HTML-тегов.")
            if path[-1] == "markdown":
                _markdown_code_fields(text)
                for key in re.findall(r"\]\(\s*\{\{([A-Za-z][A-Za-z0-9_]{0,31})\}\}", text):
                    if by_key[key]["type"] != "url":
                        raise Invalid(f"Для адреса Markdown-ссылки поле {key} должно иметь тип «Ссылка».")
        return text

    _walk(clean, validate_source)
    if tid and version is None:
        raise Invalid("Для обновления передайте version прочитанного шаблона.")
    return records.save_template(title, clean, normalized, tid, version)


def delete_template(tid: str, version: int) -> None:
    records.delete_template(tid, version)


def create_draft(tid: str, values: dict[str, str], version: int, title: str = "") -> dict:
    template = records.get_template(tid)
    if template["version"] != version:
        raise Conflict("Шаблон изменился. Откройте форму заново, чтобы заполнить актуальные поля.")
    fields = {field["key"]: field for field in template["fields"]}
    if extra := values.keys() - fields.keys():
        raise Invalid("Неизвестные поля: " + ", ".join(sorted(extra)))
    filled = {key: _field_value(field, values.get(key, field["default"])) for key, field in fields.items()}
    replacements: dict[tuple[str, str], tuple[str, int]] = {}

    def replacement(key, path):
        mode = path[-1] if path and path[-1] in {"html", "markdown"} else "plain"
        cache_key = (mode, key)
        if cache_key not in replacements:
            value = filled[key]
            if mode == "html":
                value = escape(value, quote=True)
            elif mode == "markdown":
                # Entities render literally in Markdown, link destinations and raw HTML alike;
                # backslash escapes would become visible inside an HTML block. A single pass
                # also leaves the ampersands in generated entities intact. URL delimiters decode
                # back to their original characters instead of changing query '=' into '%3D'.
                value = MARKDOWN_PUNCTUATION.sub(lambda match: f"&#{ord(match[0])};", value)
            # JSON string content size, including escaped quotes, backslashes and controls.
            size = len(json.dumps(value, ensure_ascii=False).encode("utf-8")) - 2
            replacements[cache_key] = (value, size)
        return replacements[cache_key]

    expanded_size = len(json.dumps(template["payload"], ensure_ascii=False).encode("utf-8"))

    def measure(text, path):
        nonlocal expanded_size
        for match in VARIABLE.finditer(text):
            expanded_size += replacement(match[1], path)[1] - len(match[0])
        return text

    # Count the entire expanded JSON before constructing it: a 1 MB template may repeat a
    # 10 kB field over 100,000 times. This pass only visits the bounded source and small values.
    _walk(template["payload"], measure)
    if expanded_size > records.MAX_BYTES:
        raise StudioError("Заполненный пост больше 1 МБ.", status=413)
    post = _walk(
        template["payload"], lambda text, path: VARIABLE.sub(lambda match: replacement(match[1], path)[0], text)
    )
    title = title.strip() or template["title"]
    if len(title) > 128:
        raise Invalid("Название черновика длиннее 128 символов.")
    draft = store.save_draft(None, title, post)
    assert draft is not None  # noqa: S101 - insert without a version cannot conflict
    return draft
