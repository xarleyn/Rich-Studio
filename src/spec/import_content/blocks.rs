use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::errors::ImportError;

use super::inline::{inline_kind, plain, safe_link};
use super::{array, flag, invalid, string, text};

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
