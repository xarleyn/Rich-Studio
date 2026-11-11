//! MTProto content to editable Bot API blocks. Unsupported content remains visible.

use serde_json::Value;

use crate::errors::ImportError;

mod blocks;
mod inline;
mod render;

pub use self::{
    blocks::Converter,
    inline::{entities_text, escape_html, plain, safe_link, text_html},
    render::representations,
};

pub(super) fn invalid(message: impl Into<String>) -> ImportError {
    ImportError::new(422, message)
}
pub(super) fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}
pub(super) fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}
pub(super) fn string(value: &Value) -> String {
    value.as_str().map(str::to_owned).unwrap_or_else(|| {
        if value.is_null() {
            String::new()
        } else {
            value.to_string()
        }
    })
}
pub(super) fn flag(value: &Value) -> bool {
    value.as_bool().unwrap_or(false)
}
