use regex::Regex;
use serde_json::{Map, Value, json};
use url::Url;

use super::source::Spec;
use super::{enum_values, type_const};

mod limits;
pub(crate) mod text;

pub type Validation = Value;

pub struct Validator<'a> {
    spec: &'a Spec,
    errors: Vec<String>,
    warnings: Vec<String>,
}

impl<'a> Validator<'a> {
    pub fn new(spec: &'a Spec) -> Self {
        Self {
            spec,
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn check(&self, post: &Value) -> Validation {
        self.check_post(post)
    }

    pub fn check_post(&self, post: &Value) -> Validation {
        let mut validator = Self::new(self.spec);
        let rich_message = post.get("rich_message").unwrap_or(&Value::Null);
        let mut report = validator.validate_rich_message(rich_message);
        let button_errors = keyboard_errors(post.get("reply_markup").unwrap_or(&Value::Null));
        if !button_errors.is_empty() {
            report["ok"] = json!(false);
            report["errors"] = json!(
                report["errors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .chain(button_errors)
                    .collect::<Vec<_>>()
            );
        }
        let mut extra = Self::new(self.spec);
        extra.post_fields(post);
        extra.callbacks(post);
        extra.poll(post);
        append_report(&mut report, extra.errors, extra.warnings);
        report
    }

    pub fn check_reply(&self, reply: &Value) -> Validation {
        let mut validator = Self::new(self.spec);
        validator.callbacks(&json!({"callbacks":{"answer":{"reply":reply}}}));
        json!({"ok":validator.errors.is_empty(),"errors":validator.errors,"warnings":validator.warnings,"spec_version":self.spec.version()})
    }

    pub fn check_answers(&self, post: &Value) -> Validation {
        let mut validator = Self::new(self.spec);
        validator.callbacks(post);
        validator
            .errors
            .extend(keyboard_errors(&post["reply_markup"]));
        json!({"ok":validator.errors.is_empty(),"errors":validator.errors,"warnings":validator.warnings,"spec_version":self.spec.version()})
    }

    fn post_fields(&mut self, post: &Value) {
        if let Some(object) = post.as_object() {
            for key in object.keys() {
                if ["chat_id", "chat_ids"].contains(&key.as_str()) {
                    self.errors.push(format!(
                        "{key}: канал задаётся рядом с post, не внутри поста"
                    ));
                } else if ![
                    "rich_message",
                    "reply_markup",
                    "options",
                    "pin",
                    "comment",
                    "callbacks",
                    "poll_after",
                    "_studio",
                ]
                .contains(&key.as_str())
                {
                    self.warnings.push(format!("поле «{key}» на верхнем уровне поста не используется и в Telegram не уходит; параметры Bot API кладите в options"));
                }
            }
        }
        if let Some(options) = post.get("options").filter(|value| !value.is_null()) {
            if let Some(options) = options.as_object() {
                let fields = self.spec.methods()["sendRichMessage"]["fields"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                for (key, value) in options {
                    if [
                        "chat_id",
                        "chat_ids",
                        "rich_message",
                        "reply_markup",
                        "message_id",
                        "ephemeral_message_parameters",
                    ]
                    .contains(&key.as_str())
                    {
                        self.errors.push(format!("options.{key}: поле задаёт студия; передайте канал рядом с post, rich_message и reply_markup — на уровне поста"));
                        continue;
                    }
                    if let Some(field) = fields.iter().find(|field| field["name"] == *key) {
                        if field["types"] == json!(["Boolean"]) && !value.is_boolean() {
                            self.warnings.push(format!("options.{key}: ожидалось true/false; значение передаётся в Telegram как задано"));
                            continue;
                        }
                        self.check_value(
                            value,
                            &candidates(field["types"].as_array().cloned().unwrap_or_default()),
                            &format!("options.{key}"),
                        );
                    } else {
                        self.warnings.push(format!("options.{key}: поля нет в sendRichMessage (спецификация {}), передаётся как есть",self.spec.version()));
                    }
                }
            } else {
                self.errors
                    .push("options: нужен объект параметров sendRichMessage".to_owned());
            }
        }
        if let Some(comment) = post.get("comment").filter(|value| !value.is_null()) {
            let mut validator = Self::new(self.spec);
            let report = validator.validate_rich_message(comment);
            self.errors.extend(
                report["errors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|message| format!("comment: {message}")),
            );
            self.warnings.extend(
                report["warnings"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|message| format!("comment: {message}")),
            );
        }
    }

    pub fn check_post_in(&self, post: &Value, places: &[(String, String)]) -> Validation {
        let mut report = self.check_post(post);
        append_report(
            &mut report,
            Vec::new(),
            super::chats::warnings(self.spec, post, places),
        );
        for (data, answer) in post["callbacks"].as_object().into_iter().flatten() {
            if let Some(reply) = answer.get("reply").filter(|value| value.is_object()) {
                let warnings = super::chats::warnings(self.spec, reply, places)
                    .into_iter()
                    .map(|warning| format!("callbacks.{data}.reply: {warning}"))
                    .collect();
                append_report(&mut report, Vec::new(), warnings);
            }
        }
        report
    }

    fn poll(&mut self, post: &Value) {
        if let Some(poll) = post.get("poll_after").filter(|poll| !poll.is_null()) {
            if !poll.is_object() {
                self.errors
                    .push("poll_after: нужен объект параметров sendPoll".to_owned());
                return;
            }
            let mut poll = poll.clone();
            if let Some(options) = poll["options"].as_array_mut() {
                for option in options {
                    if option.is_string() {
                        *option = json!({"text":option.clone()});
                    }
                }
            }
            if poll.get("chat_id").is_some() {
                self.errors
                    .push("poll_after.chat_id: канал задаётся рядом с post".to_owned());
            }
            let question = poll["question"].as_str().unwrap_or("");
            if question.trim().is_empty() || question.chars().count() > 300 {
                self.errors.push(
                    "poll_after.question: вопрос должен содержать от 1 до 300 символов".to_owned(),
                );
            }
            let options = poll["options"].as_array();
            if options.is_none_or(|options| {
                !(2..=12).contains(&options.len())
                    || options
                        .iter()
                        .filter(|option| {
                            option["text"]
                                .as_str()
                                .is_some_and(|text| !text.trim().is_empty())
                        })
                        .count()
                        < 2
            }) {
                self.errors.push(
                    "poll_after.options: нужно минимум два варианта, всего от 2 до 12 непустых вариантов"
                        .to_owned(),
                );
            }
            if poll["type"] == "quiz"
                && poll["correct_option_id"].is_null()
                && poll["correct_option_ids"]
                    .as_array()
                    .is_none_or(Vec::is_empty)
            {
                self.errors.push(
                    "poll_after: у викторины не отмечен правильный ответ (correct_option_ids)"
                        .to_owned(),
                );
            }
            let fields = self.spec.methods()["sendPoll"]["fields"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for field in &fields {
                let key = field["name"].as_str().unwrap_or("");
                if key == "chat_id" {
                    continue;
                }
                if let Some(value) = poll.get(key) {
                    let types = candidates(field["types"].as_array().cloned().unwrap_or_default());
                    self.check_value(value, &types, &format!("poll_after.{key}"));
                } else if field["required"] == true {
                    self.errors
                        .push(format!("poll_after: нет обязательного поля {key}"));
                }
            }
        }
    }

    fn callbacks(&mut self, post: &Value) {
        let Some(callbacks) = post.get("callbacks").filter(|value| !value.is_null()) else {
            return;
        };
        let Some(callbacks) = callbacks.as_object() else {
            self.errors
                .push("callbacks: нужен объект {callback_data: ответ}".to_owned());
            return;
        };
        let buttons = callback_buttons(post);
        for (data, answer) in callbacks {
            let path = format!("callbacks.{data}.reply");
            if !buttons.contains_key(data) {
                self.warnings.push(format!("callbacks.{data}: ответ без кнопки — кнопки с таким callback_data нет ни под постом, ни в его тексте; ответ хранится как есть"));
            }
            if !answer.is_string() && !answer.is_object() {
                self.warnings.push(format!(
                    "callbacks.{data}: ни текст, ни объект, на нажатие придёт пустой ответ"
                ));
            }
            if let Some(answer) = answer.as_object() {
                let fields = self.spec.methods()["answerCallbackQuery"]["fields"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                for (key, value) in answer {
                    if key == "reply" {
                        continue;
                    }
                    if key == "callback_query_id" {
                        self.warnings.push(format!(
                            "callbacks.{data}.callback_query_id задаёт студия при нажатии"
                        ));
                        continue;
                    }
                    if let Some(field) = fields.iter().find(|field| field["name"] == *key) {
                        let mut check = Self::new(self.spec);
                        check.check_value(
                            value,
                            &candidates(field["types"].as_array().cloned().unwrap_or_default()),
                            &format!("callbacks.{data}.{key}"),
                        );
                        self.warnings.extend(check.errors);
                        self.warnings.extend(check.warnings);
                    } else {
                        self.warnings.push(format!("callbacks.{data}.{key}: поля нет в answerCallbackQuery (спецификация {})",self.spec.version()));
                    }
                }
            }
            let Some(reply) = answer.get("reply").filter(|value| !value.is_null()) else {
                continue;
            };
            let Some(reply) = reply.as_object() else {
                self.errors
                    .push(format!("{path}: нужен объект параметров sendRichMessage"));
                continue;
            };
            for key in ["chat_id", "business_connection_id", "allow_paid_broadcast"] {
                if reply.contains_key(key) {
                    self.errors.push(format!(
                        "{path}.{key}: ответ уходит только нажавшему; поле задаёт студия"
                    ));
                }
            }
            if let Some(ephemeral) = reply.get("ephemeral_message_parameters") {
                for key in ["callback_query_id", "receiver_user_id"] {
                    if ephemeral.get(key).is_some() {
                        self.errors.push(format!("{path}.ephemeral_message_parameters.{key}: значение задаёт студия при нажатии"));
                    }
                }
            }
            let mut validator = Self::new(self.spec);
            let report =
                validator.validate_rich_message(reply.get("rich_message").unwrap_or(&Value::Null));
            self.errors.extend(
                report["errors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|message| format!("{path}: {message}")),
            );
            if !callback_buttons(&Value::Object(reply.clone())).is_empty() {
                self.warnings.push(format!(
                    "{path}: на callback_data кнопки внутри rich-ответа студия не отвечает"
                ));
            }
            self.warnings.extend(
                report["warnings"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|message| format!("{path}: {message}")),
            );
            for (key, value) in reply {
                if key == "rich_message"
                    || ["chat_id", "business_connection_id", "allow_paid_broadcast"]
                        .contains(&key.as_str())
                {
                    continue;
                }
                let fields = self.spec.methods()["sendRichMessage"]["fields"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if let Some(field) = fields.iter().find(|field| field["name"] == *key) {
                    let mut child = Self::new(self.spec);
                    child.check_value(
                        value,
                        &candidates(field["types"].as_array().cloned().unwrap_or_default()),
                        &format!("{path}.{key}"),
                    );
                    self.errors.extend(child.errors);
                    self.warnings.extend(child.warnings);
                } else {
                    self.warnings.push(format!(
                        "{path}.{key}: поля нет в sendRichMessage (спецификация {})",
                        self.spec.version()
                    ));
                }
            }
            self.errors.extend(
                keyboard_errors(reply.get("reply_markup").unwrap_or(&Value::Null))
                    .into_iter()
                    .map(|message| format!("{path}: {message}")),
            );
            self.warnings.extend(super::chats::warnings(
                self.spec,
                &Value::Object(reply.clone()),
                &[(
                    "ephemeral".to_owned(),
                    "в rich-ответе на нажатие".to_owned(),
                )],
            ));
            if answer["cache_time"].as_i64().is_some_and(|time| time > 0) {
                self.warnings.push(format!("callbacks.{data}.cache_time: повторное нажатие в это время не дойдёт до студии, rich-ответа не будет"));
            }
        }
    }

    fn check_value(&mut self, value: &Value, types: &[String], path: &str) {
        if types.is_empty() {
            return;
        }
        if types.len() == 1 {
            self.check_one(value, &types[0], path);
            return;
        }
        let mut best: Option<(Self, usize)> = None;
        for type_name in types {
            let mut candidate = Self::new(self.spec);
            candidate.check_one(value, type_name, path);
            if candidate.errors.is_empty() {
                self.warnings.extend(candidate.warnings);
                return;
            }
            let gap = shape_gap(value, type_name, self.spec);
            if best.as_ref().is_none_or(|(current, current_gap)| {
                (candidate.errors.len(), gap) < (current.errors.len(), *current_gap)
            }) {
                best = Some((candidate, gap));
            }
        }
        if let Some((best, _)) = best {
            self.errors.push(format!(
                "{path}: не подходит ни под один из типов {} (ближайший: {})",
                types.join(", "),
                best.errors.first().cloned().unwrap_or_default()
            ));
        }
    }

    fn check_one(&mut self, value: &Value, type_name: &str, path: &str) {
        if let Some(inner) = type_name.strip_prefix("Array of ") {
            let Some(items) = value.as_array() else {
                self.errors
                    .push(format!("{path}: ожидался массив ({type_name})"));
                return;
            };
            for (index, item) in items.iter().enumerate() {
                self.check_one(item, inner, &format!("{path}[{index}]"));
            }
            return;
        }
        match type_name {
            "String" | "InputFile" => {
                if !value.is_string() {
                    self.errors.push(format!("{path}: ожидалась строка"));
                }
                return;
            }
            "Integer" => {
                if !value.is_i64() && !value.is_u64() {
                    self.errors.push(format!("{path}: ожидалось целое число"));
                }
                return;
            }
            "Float" => {
                if !value.is_number() {
                    self.errors.push(format!("{path}: ожидалось число"));
                }
                return;
            }
            "Boolean" | "True" => {
                if !value.is_boolean() {
                    self.errors.push(format!("{path}: ожидалось true/false"));
                }
                return;
            }
            _ => {}
        }
        let Some(definition) = self.spec.types().get(type_name) else {
            self.warnings.push(format!(
                "{path}: тип {type_name} не найден в спецификации, не проверяю"
            ));
            return;
        };
        let subtypes = definition
            .get("subtypes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !subtypes.is_empty() {
            if value.is_object() {
                let candidates: Vec<String> = subtypes
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|name| self.spec.types().get(name).is_some())
                    .map(str::to_owned)
                    .collect();
                let tagged: Vec<String> = candidates
                    .iter()
                    .filter(|name| {
                        type_const(self.spec.types(), name).is_some_and(|(field, constant)| {
                            value.get(&field).and_then(Value::as_str) == Some(constant.as_str())
                        })
                    })
                    .cloned()
                    .collect();
                if !tagged.is_empty() {
                    self.check_value(value, &tagged, path);
                    return;
                }
                let untagged: Vec<String> = candidates
                    .iter()
                    .filter(|name| type_const(self.spec.types(), name).is_none())
                    .cloned()
                    .collect();
                if !untagged.is_empty() {
                    self.check_value(value, &untagged, path);
                    return;
                }
                let allowed = candidates
                    .iter()
                    .filter_map(|name| type_const(self.spec.types(), name).map(|(_, value)| value))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.errors.push(format!(
                    "{path}: неизвестный type={} для {type_name}; допустимо: {allowed}",
                    python_repr(value.get("type").unwrap_or(&Value::Null))
                ));
                return;
            }
            self.check_value(value, &candidates(subtypes), path);
            return;
        }
        let Some(object) = value.as_object() else {
            self.errors
                .push(format!("{path}: ожидался объект {type_name}"));
            return;
        };
        let fields = definition
            .get("fields")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for field in &fields {
            let name = field
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if field
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && !object.contains_key(name)
                && !(type_name == "RichBlockTableCell" && ["align", "valign"].contains(&name))
            {
                self.errors.push(format!(
                    "{path}: нет обязательного поля {name} ({type_name})"
                ));
            }
        }
        for (name, item) in object {
            let Some(field) = fields
                .iter()
                .find(|field| field.get("name").and_then(Value::as_str) == Some(name.as_str()))
            else {
                self.warnings.push(format!(
                    "{path}.{name}: поля нет в {type_name} (спецификация {})",
                    self.spec.version()
                ));
                continue;
            };
            if let Some((constant_name, constant)) = type_const(self.spec.types(), type_name)
                && constant_name == *name
                && item.as_str() != Some(constant.as_str())
            {
                self.errors.push(format!(
                    "{path}.{name}: должно быть '{}', а не {}",
                    constant,
                    python_repr(item)
                ));
                continue;
            }
            if field
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && field
                    .get("types")
                    .and_then(Value::as_array)
                    .is_some_and(|types| types.len() == 1 && types[0] == "RichText")
                && (item == "" || item == &json!([]))
            {
                self.warnings.push(format!(
                    "{path}.{name}: пустой текст, Telegram может отклонить блок"
                ));
            }
            if name == "url"
                && let Some(problem) = url_error(item)
            {
                self.warnings.push(format!("{path}.{name}: {problem}"));
            }
            if item == ""
                && !["url", "anchor_link", "reference", "reference_link"]
                    .contains(&object.get("type").and_then(Value::as_str).unwrap_or(""))
            {
                let required = fields
                    .iter()
                    .filter(|field| field["required"] == true && field["name"] != "type")
                    .collect::<Vec<_>>();
                if required.len() == 2
                    && required.iter().any(|field| field["name"] == "text")
                    && required
                        .iter()
                        .any(|field| field["name"] == *name && field["types"] == json!(["String"]))
                {
                    self.warnings.push(format!("{path}.{name}: пустое значение, а текст есть: Telegram может отклонить или не распознать {type_name}"));
                }
            }
            if let Some(allowed) = field
                .get("description")
                .and_then(Value::as_str)
                .and_then(enum_values)
                && allowed.len() > 1
                && item
                    .as_str()
                    .is_some_and(|actual| !allowed.iter().any(|allowed| allowed == actual))
            {
                self.warnings.push(format!(
                    "{path}.{name}: значение {item}, ожидалось одно из {allowed:?}"
                ));
            }
            let types = field
                .get("types")
                .and_then(Value::as_array)
                .map(|types| {
                    types
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if let Some((minimum, maximum)) = number_range(field)
                && let Some(number) = item.as_f64()
                && (number < minimum as f64 || number > maximum as f64)
            {
                self.warnings.push(format!("{path}.{name}: значение {number} вне диапазона {minimum}–{maximum} из спецификации"));
            }
            self.check_value(item, &types, &format!("{path}.{name}"));
        }
    }

    fn validate_rich_message(&mut self, rich_message: &Value) -> Validation {
        if !rich_message.is_object() {
            return json!({"ok": false, "errors": ["rich_message должен быть объектом"], "warnings": [], "stats": {}});
        }
        if json_depth(rich_message) > 200 {
            return json!({"ok": false, "errors": ["вложенность больше 200 уровней"], "warnings": [], "stats": {}});
        }
        let present: Vec<&str> = ["html", "markdown", "blocks"]
            .into_iter()
            .filter(|field| {
                rich_message
                    .get(field)
                    .is_some_and(|value| !value.is_null() && value != "" && value != &json!([]))
            })
            .collect();
        if present.len() != 1 {
            self.errors.push(format!(
                "нужно ровно одно из html / markdown / blocks, сейчас: {}",
                if present.is_empty() {
                    "ничего".to_owned()
                } else {
                    present.join(", ")
                }
            ));
        }
        self.check_value(
            rich_message,
            &["InputRichMessage".to_owned()],
            "rich_message",
        );
        let mut stats = Map::new();
        if let Some(blocks) = rich_message.get("blocks").and_then(Value::as_array) {
            stats = limits::blocks(self.spec, blocks, &mut self.errors, &mut self.warnings);
        } else {
            let source = rich_message
                .get("html")
                .or_else(|| rich_message.get("markdown"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (chars, at_least) = text::source_chars(
                source,
                rich_message.get("markdown").is_some(),
                &mut self.errors,
                &mut self.warnings,
            );
            stats.insert("chars".to_owned(), json!(chars));
            stats.insert("at_least_chars".to_owned(), json!(at_least));
            stats.insert("bytes".to_owned(), json!(source.len()));
            if source.len() > 34990 {
                self.errors.push(format!("rich_message: исходник {} байт в UTF-8 вместе с разметкой: Telegram может молча обрезать хвост после ~35000 байт",source.len()));
            }
            let depth = limits::source(
                source,
                rich_message.get("html").is_some(),
                &mut self.errors,
                &mut self.warnings,
            );
            if rich_message.get("html").is_some() {
                stats.insert("depth".to_owned(), json!(depth));
            }
            let references = Regex::new(
                r"tg://(photo|video|audio|document|animation|voice(?:_note)?)\?id=([\w\-.]+)",
            )
            .unwrap_or_else(|error| panic!("media regex: {error}"));
            let media: Vec<&str> = rich_message
                .get("media")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|item| item.get("id"))
                .filter_map(Value::as_str)
                .collect();
            let mut referenced = std::collections::BTreeSet::new();
            let mut reference_count = 0usize;
            for found in references.captures_iter(source) {
                reference_count += 1;
                referenced.insert(found.get(2).map_or("", |id| id.as_str()));
                if !media.contains(&found.get(2).map_or("", |id| id.as_str())) {
                    self.errors.push(format!(
                        "в тексте есть {}, но в media нет элемента с id='{}'",
                        found.get(0).map_or("", |found| found.as_str()),
                        found.get(2).map_or("", |id| id.as_str())
                    ));
                }
            }
            for id in media
                .iter()
                .filter(|id| !id.is_empty() && !referenced.contains(**id))
            {
                self.warnings.push(format!(
                    "rich_message.media id={id:?} нигде не используется в тексте"
                ));
            }
            let mut http_media = 0usize;
            for (start, end) in text::tags(source) {
                let tag = &source[start..end];
                let name = tag
                    .trim_start_matches('<')
                    .split(|ch: char| ch.is_ascii_whitespace() || ch == '>' || ch == '/')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if ["img", "video", "audio", "tg-document"].contains(&name.as_str())
                    && text::attribute(tag, "src", true).is_some_and(|src| {
                        src.starts_with("http://") || src.starts_with("https://")
                    })
                {
                    http_media += 1;
                }
            }
            if rich_message.get("markdown").is_some() {
                let images = Regex::new(r"!\[[^\[\]]*\]\(\s*<?https?://")
                    .unwrap_or_else(|error| panic!("Markdown image regex: {error}"));
                http_media += images.find_iter(source).count();
            }
            stats.insert(
                "media".to_owned(),
                json!(media.len().max(reference_count + http_media)),
            );
        }
        let media_count = rich_message
            .get("media")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let media_count =
            media_count.max(stats.get("media").and_then(Value::as_u64).unwrap_or(0) as usize);
        stats.insert("media".to_owned(), json!(media_count));
        let chars = stats.get("chars").and_then(Value::as_i64).unwrap_or(0);
        let minimum = stats
            .get("at_least_chars")
            .and_then(Value::as_i64)
            .unwrap_or(chars);
        if minimum > 32768 {
            self.errors
                .push(format!("текст не меньше {minimum} символов, лимит 32768"));
        } else if chars > 32768 {
            self.warnings.push(format!("текст около {chars} символов, лимит 32768: точно посчитает Telegram после разбора разметки"));
        } else if chars > 8000 {
            self.warnings
                .push("больше ~8000 символов: клиент свернёт пост под «Показать ещё»".to_owned());
        }
        if media_count > 50 {
            if rich_message.get("blocks").is_some() {
                self.errors.push(format!("медиа: {media_count}, лимит 50"));
            } else {
                self.warnings
                    .push(format!("медиа около {media_count}, лимит 50"));
            }
        }
        json!({"ok": self.errors.is_empty(), "errors": self.errors, "warnings": self.warnings, "stats": stats, "spec_version": self.spec.version()})
    }
}

fn append_report(report: &mut Value, errors: Vec<String>, warnings: Vec<String>) {
    if !errors.is_empty() {
        report["ok"] = json!(false);
    }
    for (key, items) in [("errors", errors), ("warnings", warnings)] {
        if let Some(target) = report[key].as_array_mut() {
            target.extend(items.into_iter().map(Value::String));
        }
    }
}

pub fn callback_buttons(post: &Value) -> std::collections::BTreeMap<String, String> {
    let mut buttons = std::collections::BTreeMap::new();
    let mut pending = vec![&post["reply_markup"], &post["rich_message"]["blocks"]];
    while let Some(value) = pending.pop() {
        match value {
            Value::Array(items) => pending.extend(items.iter().rev()),
            Value::Object(object) => {
                if let Some(data) = object.get("callback_data").and_then(Value::as_str) {
                    buttons.entry(data.to_owned()).or_insert_with(|| {
                        object
                            .get("text")
                            .and_then(Value::as_str)
                            .filter(|text| !text.trim().is_empty())
                            .unwrap_or(data)
                            .to_owned()
                    });
                }
                pending.extend(object.values());
            }
            _ => {}
        }
    }
    for source in [
        post["rich_message"]["html"].as_str(),
        post["rich_message"]["markdown"].as_str(),
    ]
    .into_iter()
    .flatten()
    {
        for (start, end) in text::tags(source) {
            let tag = &source[start..end];
            if tag
                .get(..10)
                .is_none_or(|prefix| !prefix.eq_ignore_ascii_case("<tg-button"))
                || tag
                    .as_bytes()
                    .get(10)
                    .is_none_or(|byte| !byte.is_ascii_whitespace() && *byte != b'>')
            {
                continue;
            }
            let action = text::attribute(tag, "type", true)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| {
                    if text::attribute(tag, "url", false).is_some() {
                        "url"
                    } else {
                        "callback_data"
                    }
                    .to_owned()
                });
            if action == "callback_data"
                && let Some(data) = text::attribute(tag, "data", true)
                    .or_else(|| text::attribute(tag, "callback_data", true))
            {
                buttons.entry(data.clone()).or_insert(data);
            }
        }
    }
    buttons
}

pub fn number_range(field: &Value) -> Option<(u64, u64)> {
    let types = field["types"].as_array()?;
    if types.len() != 1 || !["Integer", "Float"].iter().any(|kind| types[0] == *kind) {
        return None;
    }
    static RANGE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let pattern = RANGE.get_or_init(|| {
        Regex::new(r";\s*(\d+)-(\d+)(?:[,.;]|$)")
            .unwrap_or_else(|error| panic!("number range regex: {error}"))
    });
    let captures = pattern.captures(field["description"].as_str()?)?;
    Some((captures[1].parse().ok()?, captures[2].parse().ok()?))
}

fn candidates(subtypes: Vec<Value>) -> Vec<String> {
    subtypes
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn url_error(value: &Value) -> Option<String> {
    let raw = value.as_str()?;
    if raw.trim().is_empty() {
        return Some("пустая ссылка".to_owned());
    }
    let trimmed = raw.trim();
    let parsed = Url::parse(trimmed).ok();
    if trimmed.to_lowercase().starts_with("http") {
        let authority = trimmed
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or("");
        if authority.is_empty()
            || authority.starts_with('/')
            || parsed.as_ref().and_then(Url::host_str).is_none()
        {
            return Some(format!("в ссылке {raw:?} нет адреса сайта"));
        }
    }
    None
}

fn python_repr(value: &Value) -> String {
    match value {
        Value::String(text) => format!("'{text}'"),
        other => other.to_string(),
    }
}

pub fn keyboard_errors(markup: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    for (row_index, row) in markup
        .get("inline_keyboard")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        for (button_index, button) in row.as_array().into_iter().flatten().enumerate() {
            let Some(object) = button.as_object() else {
                continue;
            };
            let where_ = format!(
                "кнопка «{}» (ряд {}, №{})",
                button
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                row_index + 1,
                button_index + 1
            );
            let action = object.keys().any(|key| {
                !["text", "icon_custom_emoji_id", "style", "disabled"].contains(&key.as_str())
            });
            if !action {
                errors.push(format!(
                    "{where_}: не задано, что она делает (ссылка, ответ на нажатие, копирование…)"
                ));
            } else if let Some(problem) = button.get("url").and_then(url_error) {
                let problem = if problem == "пустая ссылка" {
                    format!(
                        "{problem}: впишите адрес или выберите кнопке другой тип (уберите поле url)"
                    )
                } else {
                    problem
                };
                errors.push(format!("{where_}: {problem}"));
            }
            for url in std::iter::once(button.get("url"))
                .chain(
                    object
                        .values()
                        .filter(|value| value.is_object())
                        .map(|value| value.get("url")),
                )
                .flatten()
                .filter_map(Value::as_str)
            {
                if url.starts_with("upload:") {
                    errors.push(format!("{where_}: upload: заменяется только в поле файла, ссылка уйдёт в Telegram как есть"));
                }
            }
        }
    }
    errors
}

fn shape_gap(value: &Value, name: &str, spec: &Spec) -> usize {
    let primitive = ["String", "Integer", "Float", "Boolean", "True", "InputFile"].contains(&name);
    if value.is_array() {
        usize::from(!name.starts_with("Array of "))
    } else if value.is_object() {
        if primitive || name.starts_with("Array of ") {
            2
        } else if type_const(spec.types(), name)
            .is_some_and(|(key, constant)| value[&key] == constant)
        {
            0
        } else {
            1
        }
    } else {
        usize::from(!primitive)
    }
}

pub(super) fn json_depth(value: &Value) -> usize {
    let mut depth: usize = 0;
    let mut level = vec![value];
    while !level.is_empty() && depth <= 201 {
        depth += 1;
        level = level
            .into_iter()
            .flat_map(|item| match item {
                Value::Array(items) => items.iter().collect::<Vec<_>>(),
                Value::Object(items) => items.values().collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
    }
    depth.saturating_sub(1)
}
