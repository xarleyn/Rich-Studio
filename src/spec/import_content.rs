//! MTProto content to editable Bot API blocks. Unsupported content remains visible.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::errors::ImportError;

fn invalid(message: impl Into<String>) -> ImportError {
    ImportError::new(422, message)
}
fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}
fn string(value: &Value) -> String {
    value.as_str().map(str::to_owned).unwrap_or_else(|| {
        if value.is_null() {
            String::new()
        } else {
            value.to_string()
        }
    })
}
fn flag(value: &Value) -> bool {
    value.as_bool().unwrap_or(false)
}

pub fn safe_link(value: &str) -> String {
    if value
        .chars()
        .any(|ch| ch <= '\u{20}' || ch == '\u{7f}' || ch == '\\')
    {
        return String::new();
    }
    if value.starts_with('#') {
        return value.to_owned();
    }
    if let Ok(url) = url::Url::parse(value) {
        if ["http", "https"].contains(&url.scheme())
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && value.contains("://")
        {
            return value.to_owned();
        }
        if ["mailto", "tel"].contains(&url.scheme()) && !url.path().is_empty() {
            return value.to_owned();
        }
    }
    String::new()
}

pub fn plain(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Array(items) => items.iter().map(plain).collect(),
        Value::Object(object) => object
            .get("text")
            .or_else(|| object.get("alternative_text"))
            .or_else(|| object.get("expression"))
            .map(plain)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn inline_kind(name: &str) -> Option<&'static str> {
    match name {
        "Bold" => Some("bold"),
        "Italic" => Some("italic"),
        "Underline" => Some("underline"),
        "Strike" => Some("strikethrough"),
        "Spoiler" => Some("spoiler"),
        "Fixed" => Some("code"),
        "Subscript" => Some("subscript"),
        "Superscript" => Some("superscript"),
        "Marked" => Some("marked"),
        _ => None,
    }
}

fn tag(kind: &str) -> Option<&'static str> {
    match kind {
        "bold" => Some("b"),
        "italic" => Some("i"),
        "underline" => Some("u"),
        "strikethrough" => Some("s"),
        "spoiler" => Some("tg-spoiler"),
        "code" => Some("code"),
        "subscript" => Some("sub"),
        "superscript" => Some("sup"),
        "marked" => Some("mark"),
        _ => None,
    }
}

#[derive(Default)]
pub struct Converter {
    pub media: BTreeMap<String, Value>,
    pub warnings: Vec<String>,
    nodes: usize,
}

impl Converter {
    pub fn new(media: BTreeMap<String, Value>) -> Self {
        Self {
            media,
            ..Self::default()
        }
    }
    fn warn(&mut self, message: String) {
        if !self.warnings.contains(&message) {
            self.warnings.push(message);
        }
    }
    fn budget(&mut self, depth: usize) -> Result<(), ImportError> {
        self.nodes += 1;
        if depth > 40 || self.nodes > 10_000 {
            return Err(invalid(
                "Сообщение слишком сложное для импорта (глубина или число элементов).",
            ));
        }
        Ok(())
    }
    pub fn text(&mut self, value: &Value, depth: usize) -> Result<Value, ImportError> {
        self.budget(depth)?;
        if value.is_string() {
            return Ok(value.clone());
        }
        if !value.is_object() {
            return Ok(json!(""));
        }
        let raw_name = text(&value["_"]);
        let name = raw_name.strip_prefix("Text").unwrap_or(raw_name);
        match name {
            "Empty" => return Ok(json!("")),
            "Concat" => {
                return Ok(Value::Array(
                    array(&value["texts"])
                        .iter()
                        .map(|item| self.text(item, depth + 1))
                        .collect::<Result<_, _>>()?,
                ));
            }
            "Plain" => return Ok(json!(string(&value["text"]))),
            "CustomEmoji" => {
                return Ok(
                    json!({"type":"custom_emoji","custom_emoji_id":string(&value["document_id"]),"alternative_text":value["alt"]}),
                );
            }
            "Math" => {
                return Ok(json!({"type":"mathematical_expression","expression":value["source"]}));
            }
            _ => {}
        }
        if let Some(kind) = inline_kind(name) {
            return Ok(json!({"type":kind,"text":self.text(&value["text"],depth+1)?}));
        }
        let content = self.text(&value["text"], depth + 1)?;
        if ["Url", "Email", "Phone", "AutoUrl", "AutoEmail", "AutoPhone"].contains(&name) {
            let mut url = string(&value["url"]);
            if url.is_empty() {
                url = match name {
                    "Email" => format!("mailto:{}", text(&value["email"])),
                    "Phone" => format!("tel:{}", text(&value["phone"])),
                    _ => String::new(),
                };
            }
            if url.is_empty() {
                url = format!(
                    "{}{}",
                    match name {
                        "AutoEmail" => "mailto:",
                        "AutoPhone" => "tel:",
                        _ => "",
                    },
                    plain(&content)
                );
            }
            if !safe_link(&url).is_empty() {
                return Ok(json!({"type":"url","text":content,"url":url}));
            }
            self.warn("Небезопасная ссылка заменена обычным текстом.".to_owned());
            return Ok(content);
        }
        if name == "Anchor" {
            return Ok(json!([{"type":"anchor","name":value["name"]},content]));
        }
        if ["Mention", "Hashtag", "Cashtag", "BotCommand", "BankCard"].contains(&name) {
            return Ok(content);
        }
        self.warn(format!(
            "Форматирование {} перенесено как текст.",
            if raw_name.is_empty() {
                "неизвестное"
            } else {
                raw_name
            }
        ));
        Ok(if content == json!("") || content == json!([]) {
            json!(format!(
                "[Неподдерживаемый элемент: {}]",
                if raw_name.is_empty() { "?" } else { raw_name }
            ))
        } else {
            content
        })
    }
    fn caption(&mut self, value: &Value, depth: usize) -> Result<Value, ImportError> {
        Ok(
            json!({"text":self.text(&value["text"],depth+1)?,"credit":self.text(&value["credit"],depth+1)?}),
        )
    }
    pub fn blocks(&mut self, values: &[Value]) -> Result<Vec<Value>, ImportError> {
        self.blocks_at(values, 0)
    }
    fn blocks_at(&mut self, values: &[Value], depth: usize) -> Result<Vec<Value>, ImportError> {
        values.iter().map(|v| self.block(v, depth + 1)).collect()
    }
    fn block(&mut self, v: &Value, depth: usize) -> Result<Value, ImportError> {
        self.budget(depth)?;
        let raw_name = text(&v["_"]);
        let name = raw_name.strip_prefix("PageBlock").unwrap_or(raw_name);
        if ["Paragraph", "Footer", "Thinking"].contains(&name) {
            return Ok(json!({"type":name.to_lowercase(),"text":self.text(&v["text"],depth+1)?}));
        }
        if name.starts_with("Heading")
            || ["Title", "Header", "Subtitle", "Subheader", "Kicker"].contains(&name)
        {
            let size = name
                .strip_prefix("Heading")
                .and_then(|size| size.parse::<u8>().ok())
                .filter(|size| (1..=6).contains(size))
                .unwrap_or(if ["Title", "Header"].contains(&name) {
                    1
                } else {
                    2
                });
            return Ok(json!({"type":"heading","size":size,"text":self.text(&v["text"],depth+1)?}));
        }
        match name {
            "Preformatted" => {
                return Ok(
                    json!({"type":"pre","text":self.text(&v["text"],depth+1)?,"language":text(&v["language"])}),
                );
            }
            "Divider" => return Ok(json!({"type":"divider"})),
            "Anchor" => return Ok(json!({"type":"anchor","name":v["name"]})),
            "Math" => {
                return Ok(json!({"type":"mathematical_expression","expression":v["source"]}));
            }
            "Blockquote" | "BlockquoteBlocks" | "Pullquote" => {
                let credit = self.text(&v["caption"], depth + 1)?;
                if name == "Pullquote" || flag(&v["collapsed"]) {
                    return Ok(
                        json!({"type":if name=="Pullquote"{"pullquote"}else{"expandable_blockquote"},"text":self.text(&v["text"],depth+1)?,"credit":credit}),
                    );
                }
                let blocks = if v.get("blocks").is_some() {
                    self.blocks_at(array(&v["blocks"]), depth)?
                } else {
                    vec![json!({"type":"paragraph","text":self.text(&v["text"],depth+1)?})]
                };
                return Ok(json!({"type":"blockquote","blocks":blocks,"credit":credit}));
            }
            "List" | "OrderedList" => {
                let source = array(&v["items"]);
                let mut counter =
                    v["start"]
                        .as_i64()
                        .filter(|v| *v != 0)
                        .unwrap_or(if flag(&v["reversed"]) {
                            source.len() as i64
                        } else {
                            1
                        });
                let mut items = Vec::new();
                for item in source {
                    let blocks = if item.get("blocks").is_some() {
                        self.blocks_at(array(&item["blocks"]), depth)?
                    } else {
                        vec![json!({"type":"paragraph","text":self.text(&item["text"],depth+1)?})]
                    };
                    let mut out = json!({"blocks":blocks,"has_checkbox":flag(&item["checkbox"]),"is_checked":flag(&item["checked"])});
                    if name == "OrderedList" {
                        if let Some(number) = item["value"]
                            .as_i64()
                            .or_else(|| text(&item["num"]).parse().ok())
                        {
                            counter = number;
                        }
                        out["value"] = json!(counter);
                        out["type"] = json!(if !text(&item["type"]).is_empty() {
                            text(&item["type"])
                        } else if !text(&v["type"]).is_empty() {
                            text(&v["type"])
                        } else {
                            "1"
                        });
                        counter += if flag(&v["reversed"]) { -1 } else { 1 };
                    }
                    items.push(out);
                }
                return Ok(json!({"type":"list","items":items}));
            }
            "Collage" | "Slideshow" => {
                return Ok(
                    json!({"type":name.to_lowercase(),"blocks":self.blocks_at(array(&v["items"]),depth)?,"caption":self.caption(&v["caption"],depth)?}),
                );
            }
            "Cover" => return self.block(&v["cover"], depth + 1),
            "Details" => {
                return Ok(
                    json!({"type":"details","summary":self.text(&v["title"],depth+1)?,"blocks":self.blocks_at(array(&v["blocks"]),depth)?,"is_open":flag(&v["open"])}),
                );
            }
            "Table" => {
                let mut rows = Vec::new();
                for row in array(&v["rows"]) {
                    let mut cells = Vec::new();
                    for c in array(&row["cells"]) {
                        let mut cell = json!({"align":if flag(&c["align_right"]){"right"}else if flag(&c["align_center"]){"center"}else{"left"},"valign":if flag(&c["valign_bottom"]){"bottom"}else if flag(&c["valign_middle"]){"middle"}else{"top"},"is_header":flag(&c["header"])});
                        if !c["text"].is_null() {
                            cell["text"] = self.text(&c["text"], depth + 1)?;
                        }
                        for key in ["colspan", "rowspan"] {
                            if c[key].as_i64().is_some_and(|v| v != 0) {
                                cell[key] = c[key].clone();
                            }
                        }
                        cells.push(cell);
                    }
                    rows.push(cells);
                }
                return Ok(
                    json!({"type":"table","cells":rows,"caption":self.text(&v["title"],depth+1)?,"is_bordered":flag(&v["bordered"]),"is_striped":flag(&v["striped"]),"is_compact":flag(&v["compact"])}),
                );
            }
            "Map" => {
                return Ok(
                    json!({"type":"map","location":{"latitude":v["geo"]["lat"],"longitude":v["geo"]["long"]},"zoom":v["zoom"],"width":v["w"],"height":v["h"],"caption":self.caption(&v["caption"],depth)?}),
                );
            }
            "Photo" | "Video" | "Audio" | "Document" => {
                if let Some(item) = self
                    .media
                    .get(&string(&v[format!("{}_id", name.to_lowercase())]))
                    .cloned()
                {
                    let kind = string(&item["type"]);
                    let mut item = item;
                    item["has_spoiler"] = json!(flag(&v["spoiler"]));
                    return Ok(
                        json!({"type":kind,kind.clone():item,"caption":self.caption(&v["caption"],depth)?}),
                    );
                }
            }
            _ => {}
        }
        self.warn(format!(
            "Блок {} не перенесён: в редакторе оставлена отметка.",
            if raw_name.is_empty() { "?" } else { raw_name }
        ));
        Ok(
            json!({"type":"paragraph","text":format!("[Не импортировано: {}]",if raw_name.is_empty(){"неизвестный блок"}else{raw_name})}),
        )
    }
}

pub fn entities_text(
    text: &str,
    entities: &[Value],
    warnings: &mut Vec<String>,
) -> Result<Value, ImportError> {
    if text.chars().count() > 100_000 || entities.len() > 2000 {
        return Err(invalid(
            "Слишком много текста или диапазонов форматирования для импорта.",
        ));
    }
    let raw = text.encode_utf16().collect::<Vec<_>>();
    let mut boundaries = BTreeSet::from([0, raw.len()]);
    let mut valid = Vec::new();
    for e in entities {
        let offset = e["offset"].as_i64().unwrap_or(-1);
        let length = e["length"].as_i64().unwrap_or(0);
        let end = offset.checked_add(length).unwrap_or(-1);
        if offset < 0
            || end <= offset
            || end > raw.len() as i64
            || String::from_utf16(&raw[..offset.max(0).min(raw.len() as i64) as usize]).is_err()
            || String::from_utf16(&raw[..end.max(0).min(raw.len() as i64) as usize]).is_err()
        {
            warnings.push("Некорректный диапазон форматирования пропущен.".to_owned());
            continue;
        }
        let a = offset as usize;
        let b = end as usize;
        boundaries.extend([a, b]);
        valid.push((a, b, e));
    }
    valid.sort_by_key(|(a, b, _)| b - a);
    let points = boundaries.into_iter().collect::<Vec<_>>();
    let mut out = Vec::new();
    for pair in points.windows(2) {
        let a = pair[0];
        let b = pair[1];
        let chunk =
            String::from_utf16(&raw[a..b]).map_err(|_| invalid("Некорректный UTF-16 текст."))?;
        let mut node = json!(chunk);
        for (start, end, e) in &valid {
            if *start > a || *end < b {
                continue;
            }
            let kind = crate::spec::import_content::text(&e["type"]);
            match kind {
                "bold" | "italic" | "underline" | "strikethrough" | "spoiler" | "code" | "pre" => {
                    node = json!({"type":if kind=="pre"{"code"}else{kind},"text":node})
                }
                "text_link" | "url" => {
                    let url = if !crate::spec::import_content::text(&e["url"]).is_empty() {
                        string(&e["url"])
                    } else {
                        String::from_utf16(&raw[*start..*end])
                            .map_err(|_| invalid("Некорректный UTF-16 текст."))?
                    };
                    if !safe_link(&url).is_empty() {
                        node = json!({"type":"url","url":url,"text":node});
                    }
                }
                "custom_emoji" => {
                    node = json!({"type":"custom_emoji","custom_emoji_id":string(&e["custom_emoji_id"]),"alternative_text":chunk})
                }
                "mention" | "hashtag" | "cashtag" | "bot_command" | "email" | "phone_number" => {}
                _ => warnings.push(format!("Форматирование {kind} перенесено как текст.")),
            }
        }
        out.push(node);
    }
    Ok(if out.is_empty() {
        json!(text)
    } else {
        json!(out)
    })
}

pub fn escape_html(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

pub fn text_html(value: &Value) -> String {
    if let Some(value) = value.as_str() {
        return escape_html(value).replace('\n', "<br>");
    }
    if let Some(items) = value.as_array() {
        return items.iter().map(text_html).collect();
    }
    if !value.is_object() {
        return String::new();
    }
    let kind = text(&value["type"]);
    let content = text_html(&value["text"]);
    if let Some(tag) = tag(kind) {
        return format!("<{tag}>{content}</{tag}>");
    }
    match kind {
        "url" if !safe_link(text(&value["url"])).is_empty() => format!(
            "<a href=\"{}\">{content}</a>",
            escape_html(text(&value["url"]))
        ),
        "custom_emoji" => format!(
            "<tg-emoji emoji-id=\"{}\">{}</tg-emoji>",
            escape_html(&string(&value["custom_emoji_id"])),
            escape_html(text(&value["alternative_text"]))
        ),
        "mathematical_expression" => format!(
            "<tg-math>{}</tg-math>",
            escape_html(text(&value["expression"]))
        ),
        "anchor" => format!("<a name=\"{}\"></a>", escape_html(text(&value["name"]))),
        _ => {
            if content.is_empty() {
                escape_html(&plain(value))
            } else {
                content
            }
        }
    }
}

fn audit(value: &Value, depth: usize, warnings: &mut Vec<String>) -> Result<(), ImportError> {
    if depth > 40 {
        return Err(invalid("Слишком глубокая структура содержимого."));
    }
    if let Some(items) = value.as_array() {
        for item in items {
            audit(item, depth + 1, warnings)?;
        }
    } else if let Some(items) = value.as_object() {
        let kind = text(&value["type"]);
        if !kind.is_empty()
            && tag(kind).is_none()
            && ![
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
            ]
            .contains(&kind)
        {
            warnings.push(format!(
                "HTML/Markdown: оформление {kind} упрощено; проверьте визуальный оригинал."
            ));
        }
        for child in items.values() {
            if child.is_array() || child.is_object() {
                audit(child, depth + 1, warnings)?;
            }
        }
    }
    Ok(())
}

struct Renderer<'a> {
    media: Vec<Value>,
    warnings: &'a mut Vec<String>,
}

impl Renderer<'_> {
    fn caption(&self, value: &Value) -> String {
        if value.is_null() {
            return String::new();
        }
        let credit = text_html(&value["credit"]);
        format!(
            "<figcaption>{}{}</figcaption>",
            text_html(&value["text"]),
            if credit.is_empty() {
                String::new()
            } else {
                format!("<cite>{credit}</cite>")
            }
        )
    }
    fn blocks(&mut self, values: &Value) -> String {
        array(values)
            .iter()
            .map(|value| self.render(value))
            .collect()
    }
    fn render(&mut self, b: &Value) -> String {
        let kind = text(&b["type"]);
        let content = text_html(&b["text"]);
        match kind {
            "paragraph" | "heading" | "footer" | "thinking" => {
                let tag = if kind == "heading" {
                    format!("h{}", string(&b["size"]))
                } else if kind == "footer" {
                    "footer".to_owned()
                } else {
                    "p".to_owned()
                };
                format!("<{tag}>{content}</{tag}>")
            }
            "pre" => format!(
                "<pre><code class=\"language-{}\">{}</code></pre>",
                escape_html(text(&b["language"])),
                escape_html(&plain(&b["text"]))
            ),
            "map" => format!(
                "<figure><tg-map lat=\"{:?}\" long=\"{:?}\" zoom=\"{}\"></tg-map>{}</figure>",
                b["location"]["latitude"].as_f64().unwrap_or(0.0),
                b["location"]["longitude"].as_f64().unwrap_or(0.0),
                b["zoom"].as_i64().unwrap_or(12),
                self.caption(&b["caption"])
            ),
            "divider" => "<hr>".to_owned(),
            "anchor" => format!("<a name=\"{}\"></a>", escape_html(text(&b["name"]))),
            "mathematical_expression" => format!(
                "<tg-math-block>{}</tg-math-block>",
                escape_html(text(&b["expression"]))
            ),
            "blockquote" | "expandable_blockquote" | "pullquote" => {
                let tag = if kind == "pullquote" {
                    "aside"
                } else {
                    "blockquote"
                };
                let attr = if kind == "expandable_blockquote" {
                    " expandable"
                } else {
                    ""
                };
                let body = self.blocks(&b["blocks"]);
                let credit = text_html(&b["credit"]);
                format!(
                    "<{tag}{attr}>{}{}</{tag}>",
                    if body.is_empty() { content } else { body },
                    if credit.is_empty() {
                        String::new()
                    } else {
                        format!("<cite>{credit}</cite>")
                    }
                )
            }
            "list" => {
                let ordered = array(&b["items"])
                    .iter()
                    .any(|item| item.get("value").is_some());
                let tag = if ordered { "ol" } else { "ul" };
                let mut rows = String::new();
                for item in array(&b["items"]) {
                    let attr = if item.get("value").is_some() {
                        format!(
                            " value=\"{}\" type=\"{}\"",
                            string(&item["value"]),
                            escape_html(item["type"].as_str().unwrap_or("1"))
                        )
                    } else {
                        String::new()
                    };
                    let check = if flag(&item["has_checkbox"]) {
                        format!(
                            "<input type=\"checkbox\"{}>",
                            if flag(&item["is_checked"]) {
                                " checked"
                            } else {
                                ""
                            }
                        )
                    } else {
                        String::new()
                    };
                    rows.push_str(&format!(
                        "<li{attr}>{check}{}</li>",
                        self.blocks(&item["blocks"])
                    ));
                }
                format!("<{tag}>{rows}</{tag}>")
            }
            "details" => format!(
                "<details{}><summary>{}</summary>{}</details>",
                if flag(&b["is_open"]) { " open" } else { "" },
                text_html(&b["summary"]),
                self.blocks(&b["blocks"])
            ),
            "collage" | "slideshow" => format!(
                "<tg-{kind}>{}{}</tg-{kind}>",
                self.blocks(&b["blocks"]),
                self.caption(&b["caption"])
            ),
            "table" => {
                let mut rows = String::new();
                for row in array(&b["cells"]) {
                    let mut cells = String::new();
                    for c in array(row) {
                        if c.get("text").is_none() {
                            continue;
                        }
                        let tag = if flag(&c["is_header"]) { "th" } else { "td" };
                        let mut attrs = String::new();
                        for key in ["align", "valign", "colspan", "rowspan"] {
                            if c.get(key).is_some() {
                                attrs.push_str(&format!(
                                    " {key}=\"{}\"",
                                    escape_html(&string(&c[key]))
                                ));
                            }
                        }
                        cells.push_str(&format!("<{tag}{attrs}>{}</{tag}>", text_html(&c["text"])));
                    }
                    rows.push_str(&format!("<tr>{cells}</tr>"));
                }
                let attrs = ["bordered", "striped", "compact"]
                    .iter()
                    .filter(|key| flag(&b[format!("is_{key}")]))
                    .map(|key| format!(" {key}"))
                    .collect::<String>();
                format!(
                    "<table{attrs}><caption>{}</caption>{rows}</table>",
                    text_html(&b["caption"])
                )
            }
            "photo" | "video" | "audio" | "animation" | "voice_note" | "document" => {
                let item = &b[kind];
                let mid = format!("import{}", self.media.len() + 1);
                self.media.push(json!({"id":mid,"media":item}));
                let scheme = if kind == "voice_note" { "voice" } else { kind };
                let tag = match kind {
                    "photo" => "img",
                    "animation" => "video",
                    "voice_note" => "audio",
                    "document" => "tg-document",
                    _ => kind,
                };
                let attrs = if flag(&item["has_spoiler"]) {
                    " tg-spoiler"
                } else {
                    ""
                };
                format!(
                    "<figure><{tag} src=\"tg://{scheme}?id={mid}\"{attrs}>{}{}{}</figure>",
                    if tag == "img" {
                        String::new()
                    } else {
                        format!("</{tag}>")
                    },
                    self.caption(&b["caption"]),
                    ""
                )
            }
            _ => {
                self.warnings.push(format!("HTML/Markdown: блок {kind} заменён отметкой; исходник сохранён в визуальном режиме."));
                format!(
                    "<p>{}</p>",
                    escape_html(&format!("[Блок {kind}: доступен в визуальном режиме]"))
                )
            }
        }
    }
}

pub fn representations(
    blocks: &[Value],
    warnings: &mut Vec<String>,
) -> Result<(String, String, Vec<Value>), ImportError> {
    audit(&json!(blocks), 0, warnings)?;
    let mut renderer = Renderer {
        media: Vec::new(),
        warnings,
    };
    let html_parts = blocks
        .iter()
        .map(|block| renderer.render(block))
        .collect::<Vec<_>>();
    let mut markdown = Vec::new();
    for (block, html) in blocks.iter().zip(&html_parts) {
        if ["paragraph", "heading"].contains(&text(&block["type"])) && block["text"].is_string() {
            let mut escaped = String::new();
            for ch in text(&block["text"]).chars() {
                if "\\`*_{}[]()#+.!|>~=$-".contains(ch) {
                    escaped.push('\\');
                }
                escaped.push(ch);
            }
            let escaped = escape_html(&escaped).replace('\n', "  \n");
            let prefix = if text(&block["type"]) == "heading" {
                format!(
                    "{} ",
                    "#".repeat(block["size"].as_u64().unwrap_or(1).min(6) as usize)
                )
            } else {
                String::new()
            };
            markdown.push(format!("{prefix}{escaped}"));
        } else {
            markdown.push(html.clone());
        }
    }
    Ok((html_parts.join("\n"), markdown.join("\n\n"), renderer.media))
}
