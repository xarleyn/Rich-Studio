//! Проверка HTML-виджетов (Bot API 10.4) до отправки: размеры и запрещённые возможности.
//!
//! Лимиты из опубликованного профиля runtime 1 (WidgetSandbox): исходник 32 768 байт,
//! после zlib level 9 — 4 096 (иначе сервер отвечает `INPUT_MEDIA_WIDGET_TOO_LARGE`),
//! `chat_data`/`public_data` — по 1 024 байта. Запрещённые API клиенты вырезают,
//! остальной код виджета продолжает работать, поэтому скан даёт предупреждения, а не ошибки.

use std::io::Write as _;

use serde_json::{Value, json};

/// Исходник виджета (HTML со встроенными CSS и JS), байт UTF-8.
pub const MAX_SOURCE_BYTES: usize = 32_768;
/// Сжатый виджет (zlib level 9), байт.
pub const MAX_COMPRESSED_BYTES: usize = 4_096;
/// Каждое из `chat_data`/`public_data`, байт UTF-8.
pub const MAX_DATA_BYTES: usize = 1_024;

/// Что вырезается из виджетов: подстрока (в нижнем регистре) и пояснение.
const FORBIDDEN: &[(&str, &str)] = &[
    ("fetch(", "сеть отключена: fetch не работает"),
    (
        "xmlhttprequest",
        "сеть отключена: XMLHttpRequest не работает",
    ),
    ("websocket", "сеть отключена: WebSocket не работает"),
    ("eventsource", "сеть отключена: EventSource не работает"),
    ("sendbeacon", "сеть отключена: sendBeacon не работает"),
    ("new worker", "сеть отключена: Workers не работают"),
    ("sharedworker", "сеть отключена: Workers не работают"),
    (
        "importscripts",
        "сеть отключена: внешние скрипты не грузятся",
    ),
    ("<audio", "аудио вырезается: тег <audio> не работает"),
    ("<video", "видео вырезается: тег <video> не работает"),
    ("<iframe", "iframe вырезается"),
    ("<embed", "внешние объекты вырезаются"),
    ("<object", "внешние объекты вырезаются"),
    (
        "<input",
        "поля ввода вырезаются: фокус и клавиатура недоступны",
    ),
    (
        "<textarea",
        "поля ввода вырезаются: фокус и клавиатура недоступны",
    ),
    (
        "<select",
        "поля ввода вырезаются: фокус и клавиатура недоступны",
    ),
    (
        "getcontext('webgl",
        "WebGL отключён: работает только Canvas 2D",
    ),
    (
        "getcontext(\"webgl",
        "WebGL отключён: работает только Canvas 2D",
    ),
    (
        "offscreencanvas",
        "OffscreenCanvas отключён: работает только Canvas 2D",
    ),
    ("alert(", "диалоги отключены: alert не работает"),
    ("confirm(", "диалоги отключены: confirm не работает"),
    ("prompt(", "диалоги отключены: prompt не работает"),
    (
        "@font-face",
        "внешние шрифты не работают (только системные)",
    ),
    (
        "window.open",
        "навигация отключена: window.open не работает",
    ),
    (
        "navigator.clipboard",
        "буфер обмена: используйте SDK-метод copyText",
    ),
];

pub fn compress_zlib9(source: &str) -> Result<Vec<u8>, String> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(9));
    encoder
        .write_all(source.as_bytes())
        .map_err(|error| error.to_string())?;
    encoder.finish().map_err(|error| error.to_string())
}

/// Полный отчёт проверки виджета: `ok`, `errors`, `warnings`, `size_raw`, `size_compressed`.
pub fn report(widget: &Value) -> Value {
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let html = widget["html"].as_str().unwrap_or_default();
    if html.trim().is_empty() {
        errors.push("виджет: пустой HTML".to_owned());
    }
    if html.contains('\0') {
        errors.push("виджет: html содержит NUL".to_owned());
    }
    if let Some(value) = widget.get("allow_queries") {
        if !value.is_boolean() {
            errors.push("виджет: allow_queries должно быть true/false".to_owned());
        } else if value == true {
            errors.push(
                "студия поддерживает автономные виджеты: allow_queries=true пока недоступно"
                    .to_owned(),
            );
        }
    }
    let size_raw = html.len();
    let size_compressed = match compress_zlib9(html) {
        Ok(compressed) => compressed.len(),
        Err(error) => {
            errors.push(format!("виджет: не удалось проверить сжатие zlib: {error}"));
            0
        }
    };
    if size_raw > MAX_SOURCE_BYTES {
        errors.push(format!(
            "виджет: исходник {size_raw} байт, лимит {MAX_SOURCE_BYTES}"
        ));
    }
    if size_compressed > MAX_COMPRESSED_BYTES {
        errors.push(format!(
            "виджет: после сжатия zlib {size_compressed} байт, лимит {MAX_COMPRESSED_BYTES} (сервер отклонит: INPUT_MEDIA_WIDGET_TOO_LARGE)"
        ));
    }
    errors.extend(data_errors(widget));
    scan(html, &mut warnings);
    json!({
        "ok": errors.is_empty(),
        "errors": errors,
        "warnings": warnings,
        "size_raw": size_raw,
        "size_compressed": size_compressed,
    })
}

/// Общие правила данных для создания и частичной правки. Только отсутствие ключа
/// оставляет поле неизменным; null, числа и объекты не являются командой очистки.
pub fn data_errors(value: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    for key in ["chat_data", "public_data"] {
        let Some(value) = value.get(key) else {
            continue;
        };
        let Some(data) = value.as_str() else {
            errors.push(format!("виджет: {key} должно быть строкой"));
            continue;
        };
        let size = data.len();
        if size > MAX_DATA_BYTES {
            errors.push(format!("виджет: {key} {size} байт, лимит {MAX_DATA_BYTES}"));
        }
        if data.contains('\0') {
            errors.push(format!("виджет: {key} содержит NUL"));
        }
    }
    errors
}

fn scan(html: &str, warnings: &mut Vec<String>) {
    let lowered = html.to_lowercase();
    for (needle, message) in FORBIDDEN {
        if lowered.contains(needle) {
            warnings.push(format!("в виджете {message}"));
        }
    }
}
