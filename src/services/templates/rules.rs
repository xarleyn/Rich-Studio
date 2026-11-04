use std::collections::BTreeMap;

use regex::Regex;

use crate::errors::StoreError;

use super::fields::{FILE_VARIABLE, TemplateField};
use super::substitute::Mode;
use super::{TemplateError, invalid};

fn regex(pattern: &str) -> Result<Regex, TemplateError> {
    Regex::new(pattern)
        .map_err(|error| StoreError::Migration(format!("template pattern: {error}")).into())
}

pub(super) struct Rules {
    pub(super) variable: Regex,
    pub(super) key: Regex,
    dynamic_tag: Regex,
    attributes: Regex,
    md_link: Regex,
    quote: Regex,
    list_item: Regex,
    fence: Regex,
    indented: Regex,
    backticks: Regex,
    dollars: Regex,
}

impl Rules {
    pub(super) fn new() -> Result<Self, TemplateError> {
        Ok(Self {
            variable: regex(r"\{\{([A-Za-z][A-Za-z0-9_]{0,31})\}\}")?,
            key: regex(r"^[A-Za-z][A-Za-z0-9_]{0,31}$")?,
            dynamic_tag: regex(r"<\s*/?\s*\{\{")?,
            attributes: regex(r#"(?s)([^\s=/>]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]*)))?"#)?,
            md_link: regex(r"\]\(\s*\{\{([A-Za-z][A-Za-z0-9_]{0,31})\}\}")?,
            quote: regex(r"^[ \t]*>[ \t]?")?,
            list_item: regex(r"^[ \t]*(?:[-+*]|\d{1,9}[.)])[ \t]")?,
            fence: regex(r"^[ \t]*(`{3,}|~{3,})")?,
            indented: regex(r"^( {4}|\t)")?,
            backticks: regex(r"`+")?,
            dollars: regex(r"\$+")?,
        })
    }

    fn whole_variable(&self, text: &str) -> Option<String> {
        self.variable
            .captures(text)
            .filter(|capture| {
                capture
                    .get(0)
                    .is_some_and(|matched| matched.as_str() == text)
            })
            .map(|capture| capture[1].to_owned())
    }

    pub(super) fn validate_source(
        &self,
        text: &str,
        path: &[String],
        fields: &BTreeMap<&str, &TemplateField>,
    ) -> Result<(), TemplateError> {
        if self.variable.is_match(text) && self.whole_variable(text).is_none() {
            let mut prefix = String::new();
            let mut position = 0;
            for capture in self.variable.captures_iter(text) {
                let matched = capture
                    .get(0)
                    .ok_or_else(|| StoreError::Migration("missing variable match".to_owned()))?;
                prefix.extend(
                    text[position..matched.start()]
                        .chars()
                        .take(7 - prefix.chars().count()),
                );
                if prefix.chars().count() == 7 {
                    break;
                }
                let field = fields.get(&capture[1]).ok_or_else(|| {
                    TemplateError::Invalid(format!("Неизвестное поле: {}", &capture[1]))
                })?;
                prefix.extend(field.default.chars().take(7 - prefix.chars().count()));
                position = matched.end();
                if prefix.chars().count() == 7 {
                    break;
                }
            }
            if prefix.chars().count() < 7 {
                prefix.extend(text[position..].chars().take(7 - prefix.chars().count()));
            }
            if prefix == "upload:" {
                return invalid(FILE_VARIABLE);
            }
        }
        let mode = Mode::for_path(path);
        if mode != Mode::Plain {
            self.html_fields(text, fields)?;
            if self.dynamic_tag.is_match(text) {
                return invalid("Переменные не могут задавать имена HTML-тегов.");
            }
            if mode == Mode::Markdown {
                self.markdown_code_fields(text)?;
                for capture in self.md_link.captures_iter(text) {
                    if fields
                        .get(&capture[1])
                        .is_some_and(|field| field.kind != "url")
                    {
                        return invalid(format!(
                            "Для адреса Markdown-ссылки поле {} должно иметь тип «Ссылка».",
                            &capture[1]
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn html_fields(
        &self,
        text: &str,
        fields: &BTreeMap<&str, &TemplateField>,
    ) -> Result<(), TemplateError> {
        let mut position = 0;
        while let Some(relative) = text[position..].find('<') {
            let start = position + relative;
            if text[start..].starts_with("<!--") {
                position = text[start + 4..]
                    .find("-->")
                    .map_or(text.len(), |end| start + 4 + end + 3);
                continue;
            }
            if !text
                .as_bytes()
                .get(start + 1)
                .is_some_and(u8::is_ascii_alphabetic)
            {
                position = start + 1;
                continue;
            }
            let mut quote = None;
            let mut end = None;
            let mut outside = String::new();
            for (offset, ch) in text[start..].char_indices() {
                if let Some(delimiter) = quote {
                    if ch == delimiter {
                        quote = None;
                    }
                } else if ch == '\'' || ch == '"' {
                    quote = Some(ch);
                } else {
                    outside.push(ch);
                    if ch == '>' {
                        end = Some(start + offset + 1);
                        break;
                    }
                }
            }
            let Some(end) = end else {
                break;
            };
            if self.variable.is_match(&outside) {
                return invalid(
                    "Переменные в HTML разрешены только в тексте и в значениях атрибутов внутри кавычек.",
                );
            }
            let raw = &text[start + 1..end - 1];
            let name_end = raw
                .find(|ch: char| ch.is_whitespace() || ch == '/')
                .unwrap_or(raw.len());
            for attribute in self.attributes.captures_iter(&raw[name_end..]) {
                let name = attribute[1].to_ascii_lowercase();
                let Some(value) = attribute
                    .get(2)
                    .or_else(|| attribute.get(3))
                    .or_else(|| attribute.get(4))
                else {
                    continue;
                };
                let value = value.as_str();
                if name.starts_with("on") && self.variable.is_match(value) {
                    return invalid("Переменные нельзя использовать в обработчиках HTML.");
                }
                if ["href", "src"].contains(&name.as_str()) && self.variable.is_match(value) {
                    if let Some(key) = self.whole_variable(value) {
                        if fields
                            .get(key.as_str())
                            .is_some_and(|field| field.kind != "url")
                        {
                            return invalid(format!(
                                "Для HTML-атрибута {name} поле {key} должно иметь тип «Ссылка»."
                            ));
                        }
                    } else if !["https://", "http://", "tg://", "#"]
                        .iter()
                        .any(|prefix| value.starts_with(prefix))
                    {
                        return invalid(
                            "Переменная ссылки должна иметь постоянный http(s)/tg-префикс или тип «Ссылка».",
                        );
                    }
                }
            }
            position = end;
        }
        Ok(())
    }

    fn markdown_code_fields(&self, text: &str) -> Result<(), TemplateError> {
        let mut fence: Option<String> = None;
        let mut fence_quotes = 0;
        let mut outside = String::new();
        for line in text.split_inclusive('\n') {
            let mut nested = line;
            let mut quotes = 0;
            loop {
                if let Some(quote) = self
                    .quote
                    .find(nested)
                    .filter(|_| fence.is_none() || quotes < fence_quotes)
                {
                    nested = &nested[quote.end()..];
                    quotes += 1;
                } else if let Some(item) = self.list_item.find(nested).filter(|_| fence.is_none()) {
                    nested = &nested[item.end()..];
                } else {
                    break;
                }
            }
            let marker = self.fence.captures(nested);
            if let Some(active) = &fence {
                if self.variable.is_match(line) {
                    return invalid(
                        "Переменные внутри блоков кода Markdown не поддерживаются. Используйте визуальный блок кода.",
                    );
                }
                if let Some(marker) = marker
                    && marker[1].starts_with(&active[..1])
                    && marker[1].len() >= active.len()
                    && marker
                        .get(0)
                        .is_some_and(|matched| nested[matched.end()..].trim().is_empty())
                {
                    fence = None;
                }
                outside.push('\n');
            } else if let Some(marker) = marker {
                if self.variable.is_match(line) {
                    return invalid(
                        "Переменные в ограждении блока кода Markdown не поддерживаются.",
                    );
                }
                fence = Some(marker[1].to_owned());
                fence_quotes = quotes;
                outside.push('\n');
            } else {
                if self.indented.is_match(nested) && self.variable.is_match(line) {
                    return invalid(
                        "Переменные в коде Markdown с отступом не поддерживаются. Используйте визуальный блок кода.",
                    );
                }
                outside.push_str(line);
            }
        }
        let mut position = 0;
        while let Some(opening) = self.backticks.find_at(&outside, position) {
            position = opening.end();
            if escaped_at(&outside, opening.start()) {
                continue;
            }
            if let Some(closing) = self
                .backticks
                .find_iter(&outside[opening.end()..])
                .find(|found| found.len() == opening.len())
            {
                let end = opening.end() + closing.start();
                if self.variable.is_match(&outside[opening.end()..end]) {
                    return invalid(
                        "Переменные внутри строкового кода Markdown не поддерживаются. Используйте визуальное форматирование кода.",
                    );
                }
                position = opening.end() + closing.end();
            }
        }
        let mut math = None;
        let mut block_math = None;
        for dollar in self.dollars.find_iter(&outside) {
            if escaped_at(&outside, dollar.start()) {
                continue;
            }
            let mut starts = Vec::new();
            if dollar.len() >= 2 {
                if let Some(start) = block_math.take() {
                    starts.push(start);
                } else {
                    block_math = Some(dollar.start() + 2);
                }
            }
            if let Some(start) = math.take() {
                starts.push(start);
            } else {
                math = Some(dollar.start() + 1);
            }
            for start in starts {
                if start <= dollar.start()
                    && self.variable.is_match(&outside[start..dollar.start()])
                {
                    return invalid(
                        "Переменные внутри формул Markdown не поддерживаются. Используйте визуальную формулу или HTML-тег tg-math.",
                    );
                }
            }
        }
        Ok(())
    }
}

fn escaped_at(text: &str, position: usize) -> bool {
    text.as_bytes()[..position]
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count()
        % 2
        == 1
}
