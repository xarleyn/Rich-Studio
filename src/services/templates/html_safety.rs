//! Follow HTML5 ASCII boundaries and reject ambiguous tags before inserting fields.
use super::{TemplateError, TemplateField, invalid};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};

struct Quoted<'a> {
    start: usize,
    end: usize,
    name: &'a str,
    tag: usize,
}
struct Tag<'a> {
    name: String,
    attributes: BTreeMap<String, &'a str>,
}
fn whitespace(byte: u8) -> bool {
    b" \t\n\r\x0c".contains(&byte)
}
fn unclear<T>(why: &str) -> Result<T, TemplateError> {
    invalid(format!(
        "HTML с полями разбирается неоднозначно ({why}): поправьте разметку, иначе значение поля может оказаться внутри тега"
    ))
}

pub(super) fn check(
    source: &str,
    variable: &Regex,
    fields: &BTreeMap<&str, &TemplateField>,
    answered: &BTreeSet<String>,
) -> Result<(), TemplateError> {
    let bytes = source.as_bytes();
    let mut at = 0;
    let mut tags = Vec::new();
    let mut quoted = Vec::new();
    let mut marks = Vec::new();
    while let Some(relative) = source[at..].find('<') {
        let start = at + relative;
        if source[start..].starts_with("<!--") {
            if source[start + 4..].starts_with('>') || source[start + 4..].starts_with("->") {
                return unclear("короткое окончание комментария");
            }
            let end = source[start + 4..].find("-->").map(|end| start + 4 + end);
            if source[start + 4..]
                .find("--!>")
                .is_some_and(|bang| end.is_none_or(|end| start + 4 + bang < end))
            {
                return unclear("комментарий закрыт --!>");
            }
            let Some(end) = end else {
                return unclear("комментарий не закрыт");
            };
            at = end + 3;
            marks.push((start, at));
            continue;
        }
        let mut cursor = start + 1;
        let closing = bytes.get(cursor) == Some(&b'/');
        if bytes.get(cursor) == Some(&b'/') {
            cursor += 1;
        }
        if !bytes.get(cursor).is_some_and(u8::is_ascii_alphabetic) {
            return unclear("знак < не начинает тег; в тексте пишите &lt;");
        }
        let name_start = cursor;
        while cursor < bytes.len() && !whitespace(bytes[cursor]) && !b"/>".contains(&bytes[cursor])
        {
            if b"\"'<=`".contains(&bytes[cursor]) {
                return unclear("недопустимый знак в имени тега");
            }
            cursor += 1;
        }
        let name = source[name_start..cursor].to_ascii_lowercase();
        if !closing
            && [
                "script",
                "style",
                "xmp",
                "iframe",
                "noembed",
                "noframes",
                "noscript",
                "plaintext",
                "textarea",
                "title",
            ]
            .contains(&name.as_str())
        {
            return unclear("raw-text элемент браузер и Telegram разбирают по-разному");
        }
        let index = tags.len();
        let mut tag = Tag {
            name,
            attributes: BTreeMap::new(),
        };
        loop {
            while cursor < bytes.len() && whitespace(bytes[cursor]) {
                cursor += 1;
            }
            let Some(&byte) = bytes.get(cursor) else {
                return unclear("тег не закрыт");
            };
            if byte == b'>' {
                cursor += 1;
                break;
            }
            if byte == b'/' {
                if bytes.get(cursor + 1) == Some(&b'>') {
                    cursor += 2;
                    break;
                }
                return unclear("/ внутри тега не перед >");
            }
            if byte == b'=' {
                return unclear("= без имени атрибута");
            }
            let attribute_start = cursor;
            while cursor < bytes.len()
                && !whitespace(bytes[cursor])
                && !b"/>=".contains(&bytes[cursor])
            {
                if b"\"'<".contains(&bytes[cursor]) {
                    return unclear("недопустимый знак в имени атрибута");
                }
                cursor += 1;
            }
            let attribute = &source[attribute_start..cursor];
            while cursor < bytes.len() && whitespace(bytes[cursor]) {
                cursor += 1;
            }
            if bytes.get(cursor) != Some(&b'=') {
                tag.attributes
                    .entry(attribute.to_ascii_lowercase())
                    .or_insert("");
                continue;
            }
            cursor += 1;
            while cursor < bytes.len() && whitespace(bytes[cursor]) {
                cursor += 1;
            }
            let Some(&byte) = bytes.get(cursor) else {
                return unclear("после = нет значения");
            };
            if byte == b'>' {
                return unclear("после = нет значения");
            }
            let value = if byte == b'\'' || byte == b'"' {
                cursor += 1;
                let value_start = cursor;
                while cursor < bytes.len() && bytes[cursor] != byte {
                    cursor += 1;
                }
                if cursor == bytes.len() {
                    return unclear("кавычка значения не закрыта");
                }
                quoted.push(Quoted {
                    start: value_start,
                    end: cursor,
                    name: attribute,
                    tag: index,
                });
                let value = &source[value_start..cursor];
                cursor += 1;
                if bytes
                    .get(cursor)
                    .is_some_and(|byte| !whitespace(*byte) && !b"/>".contains(byte))
                {
                    return unclear("после значения атрибута нет пробела");
                }
                value
            } else {
                let value_start = cursor;
                while cursor < bytes.len() && !whitespace(bytes[cursor]) && bytes[cursor] != b'>' {
                    if b"\"'<=`".contains(&bytes[cursor]) {
                        return unclear("недопустимый знак в значении без кавычек");
                    }
                    cursor += 1;
                }
                &source[value_start..cursor]
            };
            tag.attributes
                .entry(attribute.to_ascii_lowercase())
                .or_insert(value);
        }
        at = cursor;
        marks.push((start, at));
        tags.push(tag);
    }
    for capture in variable.captures_iter(source) {
        let Some(found) = capture.get(0) else {
            continue;
        };
        let index = quoted.partition_point(|item| item.start <= found.start());
        let quoted = index
            .checked_sub(1)
            .and_then(|index| quoted.get(index))
            .filter(|item| found.end() <= item.end);
        let Some(item) = quoted else {
            let index = marks.partition_point(|(start, _)| *start <= found.start());
            if index
                .checked_sub(1)
                .and_then(|index| marks.get(index))
                .is_some_and(|(_, end)| found.start() < *end)
            {
                return invalid(
                    "Переменная внутри HTML-тега или комментария: разрешены только текст и значения в кавычках",
                );
            }
            continue;
        };
        let name = item.name.to_ascii_lowercase();
        let tag = &tags[item.tag];
        let value = &source[item.start..item.end];
        if name.starts_with("on") {
            return invalid("Переменные нельзя использовать в обработчиках HTML");
        }
        let link = [
            "href",
            "src",
            "url",
            "poster",
            "srcset",
            "xlink:href",
            "cite",
            "data",
            "action",
            "formaction",
            "background",
            "ping",
        ]
        .contains(&name.as_str())
            && !(name == "data" && tag.name == "tg-button");
        if link {
            if value != found.as_str()
                || fields
                    .get(&capture[1])
                    .is_none_or(|field| field.kind != "url")
            {
                return invalid(format!(
                    "Атрибут {name}: ссылку подставляет только поле типа url целиком"
                ));
            }
            if fields
                .get(&capture[1])
                .is_some_and(|field| field.default.starts_with("upload:"))
            {
                return invalid(
                    "upload: не подставляется в атрибут HTML; используйте rich_message.media и tg://photo?id=…",
                );
            }
        }
        if tag.name == "tg-button" && ["data", "callback_data"].contains(&name.as_str()) {
            let defaults = |value: &str| {
                let filled = variable.replace_all(value, |capture: &regex::Captures<'_>| {
                    fields
                        .get(&capture[1])
                        .map(|field| {
                            super::substitute::escape(&field.default, super::substitute::Mode::Html)
                        })
                        .unwrap_or_default()
                });
                crate::spec::validator::text::unescape(&filled)
            };
            let action = tag
                .attributes
                .get("type")
                .map(|value| defaults(value))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| {
                    if tag.attributes.contains_key("url") {
                        "url"
                    } else {
                        "callback_data"
                    }
                    .to_owned()
                });
            if action == "callback_data" && answered.contains(&defaults(value)) {
                return invalid(
                    "Переменная в callback_data кнопки с ответом: ключ callbacks не меняется, после заполнения кнопка останется без ответа",
                );
            }
        }
    }
    Ok(())
}
