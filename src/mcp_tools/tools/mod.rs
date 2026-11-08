pub mod bot_api;
pub mod chats;
pub mod community;
pub mod drafts;
pub mod files;
pub mod imports;
pub mod media_posts;
pub mod published;
pub mod publishing;
pub mod studio;
pub mod templates;
pub mod widgets;

use serde_json::{Value, json};

pub fn definition(
    name: &str,
    description: &str,
    read_only: bool,
    destructive: bool,
    open_world: bool,
    properties: Value,
) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": properties, "additionalProperties": false},
        "annotations": {"readOnlyHint": read_only, "destructiveHint": destructive, "openWorldHint": open_world}
    })
}

pub fn success(id: Value, value: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": value.to_string()}],
            "structuredContent": value,
            "isError": false
        }
    })
}

pub fn failure(id: Value, message: impl ToString) -> Value {
    let message = message.to_string();
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {"content": [{"type": "text", "text": message}], "isError": true}
    })
}
