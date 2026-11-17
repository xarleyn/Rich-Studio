//! Telegram's measured entity and address rules. These deliberately differ from HTML5.
use std::sync::OnceLock;

use regex::Regex;

fn entity_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(||Regex::new(r"&(?:amp|apos|copy|gt|hellip|ldquo|lsquo|lt|mdash|nbsp|ndash|quot|rdquo|reg|rsquo|trade|#[0-9]+|#x[0-9a-fA-F]+);").unwrap_or_else(|error|panic!("entity regex: {error}")))
}

fn decoded(entity: &str) -> Option<char> {
    Some(match entity {
        "&amp;" => '&',
        "&apos;" => '\'',
        "&copy;" => '©',
        "&gt;" => '>',
        "&hellip;" => '…',
        "&ldquo;" => '“',
        "&lsquo;" => '‘',
        "&lt;" => '<',
        "&mdash;" => '—',
        "&nbsp;" => '\u{a0}',
        "&ndash;" => '–',
        "&quot;" => '"',
        "&rdquo;" => '”',
        "&reg;" => '®',
        "&rsquo;" => '’',
        "&trade;" => '™',
        _ => {
            let (digits, radix) = entity
                .strip_prefix("&#x")
                .map(|tail| (&tail[..tail.len() - 1], 16))
                .or_else(|| {
                    entity
                        .strip_prefix("&#")
                        .map(|tail| (&tail[..tail.len() - 1], 10))
                })?;
            let digits = digits.trim_start_matches('0');
            if digits.len() > 8 {
                return None;
            }
            let number = if digits.is_empty() {
                0
            } else {
                u32::from_str_radix(digits, radix).ok()?
            };
            if number < 32 {
                ' '
            } else {
                char::from_u32(number)?
            }
        }
    })
}

pub(crate) fn unescape(text: &str) -> String {
    entity_regex()
        .replace_all(text, |capture: &regex::Captures<'_>| {
            decoded(&capture[0]).map_or_else(|| capture[0].to_owned(), |ch| ch.to_string())
        })
        .into_owned()
}

fn invalid_entity(text: &str) -> Option<&str> {
    entity_regex()
        .find_iter(text)
        .map(|found| found.as_str())
        .find(|entity| decoded(entity).is_none())
}

/// A start/end tag is scanned once; quoted values may contain `>` but not a second `<`.
pub(crate) fn tags(source: &str) -> Vec<(usize, usize)> {
    let mut tags = Vec::new();
    let mut at = 0;
    while let Some(relative) = source[at..].find('<') {
        let start = at + relative;
        let mut name = start + 1;
        if source.as_bytes().get(name) == Some(&b'/') {
            name += 1;
        }
        if !source
            .as_bytes()
            .get(name)
            .is_some_and(u8::is_ascii_alphabetic)
        {
            at = start + 1;
            continue;
        }
        let mut quote = None;
        let mut end = None;
        for (offset, ch) in source[name..].char_indices() {
            if ch == '<' {
                at = name + offset;
                break;
            }
            if let Some(active) = quote {
                if ch == active {
                    quote = None;
                }
            } else if ch == '\'' || ch == '"' {
                quote = Some(ch);
            } else if ch == '>' {
                end = Some(name + offset + 1);
                break;
            }
        }
        if let Some(end) = end {
            tags.push((start, end));
            at = end;
        } else if at <= start {
            break;
        }
        if at >= source.len() {
            break;
        }
    }
    tags
}

/// First attribute wins, including the empty attribute. ASCII whitespace follows HTML's tokenizer.
pub(crate) fn attribute(tag: &str, wanted: &str, decode: bool) -> Option<String> {
    let bytes = tag.as_bytes();
    let mut at = 1;
    while at < bytes.len() && !bytes[at].is_ascii_whitespace() && !b"/>".contains(&bytes[at]) {
        at += 1;
    }
    while at < bytes.len() {
        while at < bytes.len() && (bytes[at].is_ascii_whitespace() || bytes[at] == b'/') {
            at += 1;
        }
        if at >= bytes.len() || bytes[at] == b'>' {
            break;
        }
        let start = at;
        while at < bytes.len()
            && !bytes[at].is_ascii_whitespace()
            && !b"=/>\"'".contains(&bytes[at])
        {
            at += 1;
        }
        if start == at {
            at += 1;
            continue;
        }
        let name = &tag[start..at];
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let mut value = "";
        if bytes.get(at) == Some(&b'=') {
            at += 1;
            while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            if let Some(quote @ (b'\'' | b'"')) = bytes.get(at).copied() {
                at += 1;
                let start = at;
                while at < bytes.len() && bytes[at] != quote {
                    at += 1;
                }
                value = &tag[start..at];
                at = at.saturating_add(1);
            } else {
                let start = at;
                while at < bytes.len() && !bytes[at].is_ascii_whitespace() && bytes[at] != b'>' {
                    at += 1;
                }
                value = &tag[start..at];
            }
        }
        if name.eq_ignore_ascii_case(wanted) {
            return Some(if decode {
                unescape(value)
            } else {
                value.to_owned()
            });
        }
    }
    None
}

pub(crate) fn address(
    kind: &str,
    url: &str,
    written: Option<&str>,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
    path: &str,
) {
    if url.starts_with("upload:") {
        errors.push(format!("{path}: upload: заменяется только в поле файла, в ссылке Telegram получит адрес как есть"));
        return;
    }
    let link = kind == "link";
    let typed = written.unwrap_or(url).contains('\u{2028}');
    let effective = written
        .map(|value| unescape(&value.replace('\u{2028}', "")))
        .unwrap_or_else(|| url.replace('\u{2028}', ""));
    let mut refused = Vec::new();
    let mut odd = Vec::new();
    let mut encoded = Vec::new();
    for ch in effective.chars() {
        let drops = ch <= ' ' || "\"<>\u{85}\u{a0}\u{2003}\u{3000}\u{200b}".contains(ch);
        let button_bad = "\"<\u{a0}\u{200b}".contains(ch);
        let target = if (link && drops) || (!link && button_bad) || (link && ch == '\u{2028}') {
            &mut refused
        } else if !link && " \t".contains(ch) {
            &mut encoded
        } else if (ch.is_whitespace() && !drops && ch != '\u{2028}') || (!link && drops) {
            &mut odd
        } else {
            continue;
        };
        if !target.contains(&ch) {
            target.push(ch);
        }
    }
    if !refused.is_empty() {
        errors.push(format!(
            "{path}: в адресе есть {:?}: Telegram {}. Запишите эти символы через %-кодирование",
            refused,
            if link {
                "уберёт ссылку и оставит только текст"
            } else {
                "отклонит весь пост (BUTTON_URL_INVALID)"
            }
        ));
    } else if typed {
        warnings.push(format!(
            "{path}: Telegram уберёт из адреса разделитель строк U+2028, ссылка останется"
        ));
    }
    if !odd.is_empty() {
        warnings.push(format!(
            "{path}: в адресе {:?}; поведение Telegram для этих символов не замерено",
            odd
        ));
    }
    if !encoded.is_empty() {
        warnings.push(format!(
            "{path}: Telegram запишет пробел и табуляцию в адресе как %20 и %09"
        ));
    }
}

fn erased_message(piece: &str, entity: &str, markdown: bool) -> String {
    let shown = piece
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(40)
        .collect::<String>();
    format!(
        "rich_message: {} «{shown}» содержит {entity}: это суррогат или значение больше U+10FFFF; Telegram стирает текст до ближайшего {}, а заголовок или цитату без текста отклоняет (RICH_MESSAGE_CONTENT_REQUIRED)",
        if markdown { "абзац" } else { "текст" },
        if markdown {
            "элемента разметки (точная граница не замерена)"
        } else {
            "тега"
        }
    )
}

fn markdown_without_erased(
    source: &str,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> String {
    let mut kept = String::new();
    let mut paragraph = String::new();
    let mut paragraph_quote = 0;
    let mut fence: Option<(char, usize)> = None;
    let mut indented = false;
    let target = Regex::new(r"\]\([^)]*\)").unwrap_or_else(|error| panic!("target regex: {error}"));
    let flush = |paragraph: &mut String,
                 kept: &mut String,
                 errors: &mut Vec<String>,
                 warnings: &mut Vec<String>| {
        let text = target.replace_all(paragraph, "]");
        let without_code = without_inline_code(&text);
        if let Some(entity) = invalid_entity(&without_code) {
            errors.push(erased_message(paragraph, entity, true));
        } else if let Some(entity) = invalid_entity(&text) {
            warnings.push(format!(
                "{}; внутри строкового кода поведение не замерено",
                erased_message(paragraph, entity, true)
            ));
        } else {
            kept.push_str(paragraph);
        }
        paragraph.clear();
    };
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start_matches(' ');
        let marker = if line.len() - trimmed.len() <= 3 {
            let first = trimmed.chars().next();
            first
                .filter(|ch| *ch == '`' || *ch == '~')
                .map(|ch| (ch, trimmed.chars().take_while(|next| *next == ch).count()))
        } else {
            None
        }
        .filter(|(_, n)| *n >= 3);
        if let Some((active, size)) = fence {
            kept.push_str(line);
            if marker
                .is_some_and(|(ch, n)| ch == active && n >= size && trimmed[n..].trim().is_empty())
            {
                fence = None;
            }
            continue;
        }
        if let Some(marker) = marker {
            flush(&mut paragraph, &mut kept, errors, warnings);
            fence = Some(marker);
            kept.push_str(line);
            continue;
        }
        let blank = line.trim().is_empty();
        let starts_indent = line.starts_with("    ") || line.starts_with('\t');
        if indented && (blank || starts_indent) {
            kept.push_str(line);
            continue;
        }
        indented = starts_indent && paragraph.is_empty();
        if indented {
            kept.push_str(line);
            continue;
        }
        if blank {
            flush(&mut paragraph, &mut kept, errors, warnings);
            kept.push_str(line);
            continue;
        }
        let (quoted, quotes) = quote_prefix(line);
        let nested = quoted.trim_start_matches(' ');
        let indentation = quoted.len() - nested.len();
        let hashes = nested.bytes().take_while(|byte| *byte == b'#').count();
        let heading = indentation <= 3
            && (1..=6).contains(&hashes)
            && nested
                .as_bytes()
                .get(hashes)
                .is_none_or(|byte| byte.is_ascii_whitespace());
        let underline = indentation <= 3 && {
            let text = nested.trim_end();
            !text.is_empty()
                && (text.bytes().all(|byte| byte == b'=') || text.bytes().all(|byte| byte == b'-'))
        };
        let thematic = indentation <= 3 && {
            let chars = nested
                .chars()
                .filter(|ch| *ch != ' ' && *ch != '\t' && *ch != '\r' && *ch != '\n')
                .collect::<Vec<_>>();
            chars.len() >= 3
                && chars.first().is_some_and(|first| {
                    "-*_".contains(*first) && chars.iter().all(|ch| ch == first)
                })
        };
        if underline && !paragraph.is_empty() && quotes == paragraph_quote {
            paragraph.push_str(line);
            flush(&mut paragraph, &mut kept, errors, warnings);
        } else if heading {
            flush(&mut paragraph, &mut kept, errors, warnings);
            paragraph.push_str(line);
            flush(&mut paragraph, &mut kept, errors, warnings);
        } else if thematic {
            flush(&mut paragraph, &mut kept, errors, warnings);
            kept.push_str(line);
        } else {
            if paragraph.is_empty() {
                paragraph_quote = quotes;
            }
            paragraph.push_str(line);
        }
    }
    flush(&mut paragraph, &mut kept, errors, warnings);
    kept
}

fn quote_prefix(mut line: &str) -> (&str, usize) {
    let mut quotes = 0;
    loop {
        let trimmed = line.trim_start_matches(' ');
        if line.len() - trimmed.len() > 3 || !trimmed.starts_with('>') {
            break;
        }
        line = trimmed[1..].strip_prefix(' ').unwrap_or(&trimmed[1..]);
        quotes += 1;
    }
    (line, quotes)
}

fn without_inline_code(source: &str) -> String {
    let mut runs = Vec::new();
    let mut by_size = std::collections::BTreeMap::<usize, Vec<usize>>::new();
    let mut at = 0;
    while let Some(relative) = source[at..].find('`') {
        let start = at + relative;
        let end = start
            + source[start..]
                .bytes()
                .take_while(|byte| *byte == b'`')
                .count();
        by_size.entry(end - start).or_default().push(runs.len());
        runs.push((start, end));
        at = end;
    }
    let mut kept = String::new();
    let mut index = 0;
    let mut previous = 0;
    while let Some(&(start, end)) = runs.get(index) {
        let Some(indices) = by_size.get(&(end - start)) else {
            index += 1;
            continue;
        };
        let next = indices.partition_point(|candidate| *candidate <= index);
        if let Some(&closing) = indices.get(next) {
            kept.push_str(&source[previous..start]);
            previous = runs[closing].1;
            index = closing + 1;
        } else {
            index += 1;
        }
    }
    kept.push_str(&source[previous..]);
    kept
}

pub(super) fn source_chars(
    source: &str,
    markdown: bool,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> (usize, usize) {
    let source = if markdown {
        markdown_without_erased(source, errors, warnings)
    } else {
        let mut kept = String::new();
        let mut at = 0;
        for (start, end) in tags(source) {
            let piece = &source[at..start];
            if let Some(entity) = invalid_entity(piece) {
                errors.push(erased_message(piece, entity, false));
            } else {
                kept.push_str(piece);
            }
            kept.push_str(&source[start..end]);
            at = end;
        }
        let piece = &source[at..];
        if let Some(entity) = invalid_entity(piece) {
            errors.push(erased_message(piece, entity, false));
        } else {
            kept.push_str(piece);
        }
        kept
    };
    let mut plain = String::new();
    let mut at = 0;
    for (start, end) in tags(&source) {
        plain.push_str(&source[at..start]);
        at = end;
    }
    plain.push_str(&source[at..]);
    if markdown {
        let target =
            Regex::new(r"\]\([^)]*\)").unwrap_or_else(|error| panic!("target regex: {error}"));
        plain = target.replace_all(&plain, "]").into_owned();
    }
    let plain = unescape(&plain);
    let count = plain.chars().count();
    let minimum = if !markdown {
        plain.chars().filter(|ch| !ch.is_whitespace()).count()
    } else {
        let markup=Regex::new(r"(?m)^.*\|.*$|\[\^[^\[\]]*\]|^[ \t]*\[[^\[\]]+\]:.*$|^[ \t]*(?:```|~~~).*$|^[ \t]*\d+[.)][ \t]|\[[ xX]\]").unwrap_or_else(|error|panic!("Markdown markup regex: {error}"));
        markup
            .replace_all(&plain, "")
            .chars()
            .filter(|ch| !ch.is_whitespace() && !ch.is_ascii_punctuation())
            .count()
    };
    (count, minimum)
}
