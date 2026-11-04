//! Literal template substitution. Templates create independent unpublished drafts.

use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::account_context::AccountContext;
use crate::errors::StoreError;
pub use crate::store::templates::TemplateError;
use crate::store::{drafts, templates as records};

pub const MAX_VALUE: usize = 10_000;
const FILE_VARIABLE: &str = "Для файла замените всю ссылку upload:имя одним полем {{file}}; задайте полную ссылку в значении по умолчанию.";

fn default_true() -> bool {
    true
}
fn default_text() -> String {
    "text".to_owned()
}

#[derive(Clone, Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct TemplateField {
    pub key: String,
    pub label: String,
    #[serde(rename = "type", default = "default_text")]
    pub kind: String,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub default: String,
}

#[derive(Clone, Debug, Deserialize, utoipa::ToSchema)]
pub struct TemplateInput {
    pub title: String,
    pub post: Value,
    #[serde(default)]
    pub fields: Vec<TemplateField>,
    pub version: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, utoipa::ToSchema)]
pub struct FillInput {
    pub version: i64,
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    #[serde(default)]
    pub title: String,
}

pub struct TemplateService {
    context: AccountContext,
}

/// Every HTTP/MCP operation owns and joins its blocking SQLite worker.
pub async fn run<T, F>(context: AccountContext, action: F) -> Result<T, TemplateError>
where
    T: Send + 'static,
    F: FnOnce(&TemplateService) -> Result<T, TemplateError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || action(&TemplateService::new(context)))
        .await
        .map_err(|error| StoreError::Migration(format!("template worker failed: {error}")))?
}

impl TemplateService {
    pub fn new(context: AccountContext) -> Self {
        Self { context }
    }

    pub fn list(&self) -> Result<Vec<Value>, TemplateError> {
        records::list(&self.context)
    }

    pub fn get(&self, id: &str) -> Result<Value, TemplateError> {
        records::get(&self.context, id)
    }

    pub fn delete(&self, id: &str, version: i64) -> Result<(), TemplateError> {
        version_valid(version)?;
        records::delete(&self.context, id, version)
    }

    pub fn save(&self, id: Option<&str>, mut input: TemplateInput) -> Result<Value, TemplateError> {
        let rules = Rules::new()?;
        input.title = input.title.trim().to_owned();
        if input.title.is_empty() || input.title.chars().count() > 128 {
            return invalid("Название шаблона должно содержать от 1 до 128 символов.");
        }
        if input.fields.len() > 50 {
            return invalid("В шаблоне допускается не больше 50 полей.");
        }
        if let Some(version) = input.version {
            version_valid(version)?;
        }
        let post = input
            .post
            .as_object_mut()
            .ok_or_else(|| TemplateError::Invalid("Пост должен быть объектом JSON.".to_owned()))?;
        post.remove("_studio");
        let formats = post
            .get("rich_message")
            .and_then(Value::as_object)
            .map(|rich| {
                ["blocks", "html", "markdown"]
                    .iter()
                    .filter(|key| rich.contains_key(**key))
                    .count()
            });
        if formats != Some(1) {
            return invalid(
                "Шаблон должен содержать rich_message и один формат: blocks, html или markdown.",
            );
        }
        let mut used = BTreeSet::new();
        visit(
            &input.post,
            &mut Vec::new(),
            0,
            &rules.variable,
            &mut |text, _path| {
                if text.starts_with("upload:") && rules.variable.is_match(text) {
                    return invalid(FILE_VARIABLE);
                }
                used.extend(
                    rules
                        .variable
                        .captures_iter(text)
                        .map(|capture| capture[1].to_owned()),
                );
                Ok(())
            },
        )?;
        let mut keys = BTreeSet::new();
        for field in &mut input.fields {
            field.label = field.label.trim().to_owned();
            if !rules.key.is_match(&field.key) || !keys.insert(field.key.clone()) {
                return invalid(
                    "Имена полей должны быть уникальными: латинская буква, затем буквы, цифры или _; до 32 знаков.",
                );
            }
            if field.label.is_empty()
                || field.label.chars().count() > 100
                || !["text", "multiline", "url"].contains(&field.kind.as_str())
            {
                return invalid(format!("Проверьте подпись и тип поля {}.", field.key));
            }
            field_value(field, &field.default, false)?;
        }
        if used != keys {
            let missing = used.difference(&keys).cloned().collect::<Vec<_>>();
            if !missing.is_empty() {
                return invalid(format!(
                    "Опишите поля для переменных: {}",
                    missing.join(", ")
                ));
            }
            let unused = keys.difference(&used).cloned().collect::<Vec<_>>();
            return invalid(format!(
                "Эти поля не используются в посте: {}",
                unused.join(", ")
            ));
        }
        let by_key = input
            .fields
            .iter()
            .map(|field| (field.key.as_str(), field))
            .collect::<BTreeMap<_, _>>();
        visit(
            &input.post,
            &mut Vec::new(),
            0,
            &rules.variable,
            &mut |text, path| rules.validate_source(text, path, &by_key),
        )?;
        if id.is_some() && input.version.is_none() {
            return invalid("Для обновления передайте version прочитанного шаблона.");
        }
        let fields = serde_json::to_value(&input.fields)
            .map_err(|error| StoreError::Migration(error.to_string()))?;
        records::save(
            &self.context,
            &input.title,
            &input.post,
            &fields,
            id,
            input.version,
        )
    }

    pub fn create_draft(&self, id: &str, input: FillInput) -> Result<Value, TemplateError> {
        version_valid(input.version)?;
        let template = self.get(id)?;
        if template["version"].as_i64() != Some(input.version) {
            return Err(TemplateError::Conflict(
                "Шаблон изменился. Откройте форму заново, чтобы заполнить актуальные поля."
                    .to_owned(),
            ));
        }
        let fields: Vec<TemplateField> = serde_json::from_value(template["fields"].clone())
            .map_err(|error| {
                StoreError::Migration(format!("invalid stored template fields: {error}"))
            })?;
        let known = fields
            .iter()
            .map(|field| field.key.as_str())
            .collect::<BTreeSet<_>>();
        let extra = input
            .values
            .keys()
            .filter(|key| !known.contains(key.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if !extra.is_empty() {
            return invalid(format!("Неизвестные поля: {}", extra.join(", ")));
        }
        let mut replacements = BTreeMap::new();
        for field in fields {
            let value = input.values.get(&field.key).unwrap_or(&field.default);
            field_value(&field, value, true)?;
            for mode in [Mode::Plain, Mode::Html, Mode::Markdown] {
                let text = escape(value, mode);
                let size = records::encode(&Value::String(text.clone()))?.len() - 2;
                replacements.insert((mode, field.key.clone()), (text, size));
            }
        }
        let rules = Rules::new()?;
        let payload = &template["payload"];
        let mut size = records::encode(payload)?.len() as i64;
        visit(
            payload,
            &mut Vec::new(),
            0,
            &rules.variable,
            &mut |text, path| {
                let mode = Mode::for_path(path);
                for capture in rules.variable.captures_iter(text) {
                    let replacement = replacements
                        .get(&(mode, capture[1].to_owned()))
                        .ok_or_else(|| {
                            TemplateError::Invalid(format!(
                                "Опишите поля для переменных: {}",
                                &capture[1]
                            ))
                        })?;
                    size += replacement.1 as i64 - capture[0].len() as i64;
                }
                Ok(())
            },
        )?;
        if size > records::MAX_BYTES as i64 {
            return Err(TemplateError::TooLarge(
                "Заполненный пост больше 1 МБ.".to_owned(),
            ));
        }
        let mut post = payload.clone();
        substitute(&mut post, &mut Vec::new(), &rules.variable, &replacements)?;
        let title = if input.title.trim().is_empty() {
            template["title"]
                .as_str()
                .ok_or_else(|| StoreError::Migration("invalid stored template title".to_owned()))?
        } else {
            input.title.trim()
        };
        if title.chars().count() > 128 {
            return invalid("Название черновика длиннее 128 символов.");
        }
        drafts::save(&self.context, None, title, &post, None, None, None)?
            .ok_or_else(|| StoreError::Migration("created draft is missing".to_owned()).into())
    }
}

pub fn version_valid(version: i64) -> Result<(), TemplateError> {
    if version < 1 {
        invalid("Версия шаблона должна быть не меньше 1.")
    } else {
        Ok(())
    }
}

fn invalid<T>(message: impl Into<String>) -> Result<T, TemplateError> {
    Err(TemplateError::Invalid(message.into()))
}

fn field_value(field: &TemplateField, value: &str, required: bool) -> Result<(), TemplateError> {
    if value.chars().count() > MAX_VALUE {
        return invalid(format!(
            "Поле «{}»: допустимо не больше {MAX_VALUE} символов.",
            field.label
        ));
    }
    if required && field.required && value.trim().is_empty() {
        return invalid(format!("Заполните поле «{}».", field.label));
    }
    if !value.is_empty() && field.kind == "url" {
        let valid = value.split_once("://").is_some_and(|(scheme, rest)| {
            ["http", "https"].contains(&scheme.to_ascii_lowercase().as_str())
                && !rest.starts_with('/')
                && !rest.is_empty()
        }) && url::Url::parse(value)
            .is_ok_and(|parsed| parsed.host_str().is_some_and(|host| !host.is_empty()));
        if !valid
            || value
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control() || "<>\"'\\".contains(ch))
        {
            return invalid(format!(
                "Поле «{}»: укажите полную ссылку http:// или https:// без пробелов.",
                field.label
            ));
        }
    }
    Ok(())
}

fn regex(pattern: &str) -> Result<Regex, TemplateError> {
    Regex::new(pattern)
        .map_err(|error| StoreError::Migration(format!("template pattern: {error}")).into())
}

fn visit<F>(
    value: &Value,
    path: &mut Vec<String>,
    depth: usize,
    variable: &Regex,
    action: &mut F,
) -> Result<(), TemplateError>
where
    F: FnMut(&str, &[String]) -> Result<(), TemplateError>,
{
    if depth > 60 {
        return invalid("Слишком большая вложенность шаблона.");
    }
    match value {
        Value::String(text) => action(text, path),
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                path.push(index.to_string());
                visit(child, path, depth + 1, variable, action)?;
                path.pop();
            }
            Ok(())
        }
        Value::Object(items) => {
            for (key, child) in items {
                if variable.is_match(key) {
                    return invalid(
                        "Переменные допускаются в значениях, но не в именах полей JSON.",
                    );
                }
                path.push(key.clone());
                visit(child, path, depth + 1, variable, action)?;
                path.pop();
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
enum Mode {
    Plain,
    Html,
    Markdown,
}

impl Mode {
    fn for_path(path: &[String]) -> Self {
        match path.last().map(String::as_str) {
            Some("html") => Self::Html,
            Some("markdown") => Self::Markdown,
            _ => Self::Plain,
        }
    }
}

fn escape(text: &str, mode: Mode) -> String {
    let mut output = String::with_capacity(text.len());
    for ch in text.chars() {
        match (mode, ch) {
            (Mode::Html, '&') => output.push_str("&amp;"),
            (Mode::Html, '<') => output.push_str("&lt;"),
            (Mode::Html, '>') => output.push_str("&gt;"),
            (Mode::Html, '"') => output.push_str("&quot;"),
            (Mode::Html, '\'') => output.push_str("&#x27;"),
            (Mode::Markdown, ch) if "&<>\"'\\`*_{}[]()#+.!|~=$-".contains(ch) => {
                output.push_str(&format!("&#{};", u32::from(ch)))
            }
            _ => output.push(ch),
        }
    }
    output
}

type Replacements = BTreeMap<(Mode, String), (String, usize)>;

fn substitute(
    value: &mut Value,
    path: &mut Vec<String>,
    variable: &Regex,
    replacements: &Replacements,
) -> Result<(), TemplateError> {
    match value {
        Value::String(text) => {
            let mut replaced = String::new();
            let mut position = 0;
            for capture in variable.captures_iter(text) {
                let matched = capture
                    .get(0)
                    .ok_or_else(|| StoreError::Migration("missing variable match".to_owned()))?;
                replaced.push_str(&text[position..matched.start()]);
                let item = replacements
                    .get(&(Mode::for_path(path), capture[1].to_owned()))
                    .ok_or_else(|| {
                        TemplateError::Invalid(format!("Неизвестное поле: {}", &capture[1]))
                    })?;
                replaced.push_str(&item.0);
                position = matched.end();
            }
            replaced.push_str(&text[position..]);
            *text = replaced;
        }
        Value::Array(items) => {
            for (index, child) in items.iter_mut().enumerate() {
                path.push(index.to_string());
                substitute(child, path, variable, replacements)?;
                path.pop();
            }
        }
        Value::Object(items) => {
            for (key, child) in items {
                path.push(key.clone());
                substitute(child, path, variable, replacements)?;
                path.pop();
            }
        }
        _ => {}
    }
    Ok(())
}

struct Rules {
    variable: Regex,
    key: Regex,
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
    fn new() -> Result<Self, TemplateError> {
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

    fn validate_source(
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
