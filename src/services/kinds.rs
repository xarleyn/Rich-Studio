use serde_json::{Map, Value, json};

use crate::errors::ServiceError;

pub const RICH: &str = "rich";
pub const POLL: &str = "poll";
pub const LIVE_PHOTO: &str = "live_photo";
pub const PAID_MEDIA: &str = "paid_media";
pub const STARS_MAX: i64 = 25000;

#[derive(Clone, Copy, Debug)]
pub struct Kind {
    pub name: &'static str,
    pub method: &'static str,
    pub label: &'static str,
    pub key: Option<&'static str>,
    pub badge: Option<&'static str>,
    pub editable: bool,
    pub stoppable: bool,
    pub media: bool,
}

pub const KINDS: &[Kind] = &[
    Kind {
        name: RICH,
        method: "sendRichMessage",
        label: "пост",
        key: None,
        badge: None,
        editable: true,
        stoppable: false,
        media: false,
    },
    Kind {
        name: POLL,
        method: "sendPoll",
        label: "опрос",
        key: Some(POLL),
        badge: Some("опрос"),
        editable: false,
        stoppable: true,
        media: false,
    },
    Kind {
        name: LIVE_PHOTO,
        method: "sendLivePhoto",
        label: "Live Photo",
        key: Some(LIVE_PHOTO),
        badge: Some("Live Photo"),
        editable: false,
        stoppable: false,
        media: true,
    },
    Kind {
        name: PAID_MEDIA,
        method: "sendPaidMedia",
        label: "платный контент",
        key: Some(PAID_MEDIA),
        badge: Some("за звёзды"),
        editable: false,
        stoppable: false,
        media: true,
    },
];

pub fn get(name: Option<&str>) -> Result<&'static Kind, ServiceError> {
    let name = name.unwrap_or(RICH);
    KINDS.iter().find(|kind| kind.name == name).ok_or_else(|| {
        ServiceError::BadRequest(format!(
            "неизвестный тип сообщения {name:?}: {}",
            KINDS
                .iter()
                .map(|kind| kind.name)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })
}

pub fn describe() -> Value {
    KINDS.iter().map(|kind| (kind.name.to_owned(), json!({
        "label":kind.label,"badge":kind.badge,"editable":kind.editable,"stoppable":kind.stoppable,"media":kind.media,
    }))).collect::<Map<_,_>>().into()
}

pub fn prepare(kind: &Kind, params: &Value) -> Result<Value, ServiceError> {
    if kind.name == RICH || kind.key.is_none() {
        return Err(ServiceError::BadRequest(format!(
            "{}: отправляется не так (POST /api/posts)",
            kind.label
        )));
    }
    if !params.is_object() {
        return invalid(format!("{}: ожидался объект {}", kind.label, kind.method));
    }
    if params.get("chat_id").is_some() {
        return Err(ServiceError::BadRequest(format!(
            "chat_id задаётся не в {}",
            kind.key.unwrap_or_default()
        )));
    }
    let mut params = params.clone();
    if kind.name == POLL {
        let question = params["question"].as_str().unwrap_or_default().trim();
        if question.is_empty() {
            return Err(ServiceError::BadRequest("опрос: пустой вопрос".to_owned()));
        }
        if question.chars().count() > 300 {
            return invalid("опрос: вопрос длиннее 300 символов".to_owned());
        }
        let Some(options) = params["options"].as_array() else {
            return Err(ServiceError::BadRequest(
                "опрос: нужно минимум два варианта".to_owned(),
            ));
        };
        if options.len() < 2 {
            return Err(ServiceError::BadRequest(
                "опрос: нужно минимум два варианта".to_owned(),
            ));
        }
        if params
            .get("is_anonymous")
            .is_some_and(|value| !value.is_boolean())
        {
            return invalid("опрос: is_anonymous должно быть true/false".to_owned());
        }
        if options.len() > 12 {
            return invalid("опрос: больше 12 вариантов".to_owned());
        }
        let normalized = options
            .iter()
            .map(|option| {
                if option.is_object() {
                    option.clone()
                } else {
                    json!({"text":python_string(option)})
                }
            })
            .collect::<Vec<_>>();
        if normalized
            .iter()
            .filter(|option| {
                option["text"]
                    .as_str()
                    .map(str::trim)
                    .is_some_and(|text| !text.is_empty())
            })
            .count()
            < 2
        {
            return invalid("опрос: нужно минимум два варианта".to_owned());
        }
        if params["type"] == "quiz"
            && params["correct_option_id"].is_null()
            && params["correct_option_ids"]
                .as_array()
                .is_none_or(|ids| ids.is_empty())
        {
            return invalid("опрос: у викторины не отмечен правильный ответ".to_owned());
        }
        params["options"] = Value::Array(normalized);
    }
    if kind.name == PAID_MEDIA {
        let count = params["media"].as_array().map_or(0, Vec::len);
        if !(1..=10).contains(&count) {
            return invalid("платный контент: в media от 1 до 10 фото или видео".to_owned());
        }
        let Some(stars) = params["star_count"].as_i64() else {
            return invalid("платный контент: star_count обязателен".to_owned());
        };
        if !(1..=STARS_MAX).contains(&stars) {
            return invalid(format!(
                "платный контент: star_count от 1 до {STARS_MAX} звёзд"
            ));
        }
    }
    Ok(params)
}

pub fn poll_error(poll: &Value) -> Option<String> {
    prepare(get(Some(POLL)).ok()?, poll)
        .err()
        .map(|error| error.to_string())
}

fn invalid(message: String) -> Result<Value, ServiceError> {
    Err(ServiceError::Invalid(
        json!({"message":"сообщение не прошло проверку","ok":false,"errors":[message],"warnings":[]}),
    ))
}

fn python_string(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}
