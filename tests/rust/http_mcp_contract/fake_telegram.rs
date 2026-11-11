use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[derive(Clone)]
pub(crate) struct FakeTelegram {
    calls: Arc<Mutex<Vec<Value>>>,
    queued: Arc<Mutex<BTreeMap<String, VecDeque<Value>>>>,
    next_message_id: Arc<AtomicI64>,
}

impl FakeTelegram {
    pub(crate) fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            queued: Arc::new(Mutex::new(BTreeMap::new())),
            next_message_id: Arc::new(AtomicI64::new(100)),
        }
    }

    pub(crate) fn record(&self, method: &str, body: &Value) {
        if method == "getUpdates" {
            return;
        }
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .push(json!({"method":method,"body":body}));
    }

    pub(crate) fn all_calls(&self) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .clone()
    }

    pub(crate) fn calls_to(&self, method: &str) -> Vec<Value> {
        self.all_calls()
            .into_iter()
            .filter(|call| call["method"].as_str() == Some(method))
            .collect()
    }

    pub(crate) fn last(&self, method: &str) -> Option<Value> {
        self.calls_to(method).pop()
    }

    /// A raw Bot API envelope answered before the mock's own reply, like `tg.queue` in the Python reference.
    pub(crate) fn queue(&self, method: &str, response: Value) {
        self.queued
            .lock()
            .unwrap_or_else(|error| panic!("queue: {error}"))
            .entry(method.to_owned())
            .or_default()
            .push_back(response);
    }

    pub(crate) fn take(&self, method: &str) -> Option<Value> {
        self.queued
            .lock()
            .unwrap_or_else(|error| panic!("queue: {error}"))
            .get_mut(method)
            .and_then(VecDeque::pop_front)
    }

    pub(crate) fn answer(&self, method: &str, body: &Value) -> Value {
        match method {
            "getMe" => {
                json!({"id":42,"is_bot":true,"first_name":"Bot","username":"studio_test_bot"})
            }
            "getChat" => json!({"id":-1001,"type":"channel","title":"Тест"}),
            "getChatMember" => {
                json!({"status":"administrator","can_post_messages":true,"can_edit_messages":true})
            }
            "sendRichMessage" => {
                let id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
                json!({
                    "message_id":id,
                    "chat":{"id":body["chat_id"].clone(),"type":"channel","title":"Тест"},
                    "rich_message":{"blocks":rich_blocks(&body["rich_message"])},
                })
            }
            "sendPoll" => {
                let id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
                json!({
                    "message_id":id,
                    "chat":{"id":body["chat_id"].clone()},
                    "poll":{"id":format!("poll{id}"),"question":body["question"]},
                })
            }
            "sendMessage" => {
                let id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
                json!({"message_id":id,"chat":{"id":body["chat_id"].clone()},"text":body["text"]})
            }
            "editMessageText" | "editMessageReplyMarkup" => {
                json!({"message_id":body["message_id"],"chat":{"id":body["chat_id"].clone()}})
            }
            "stopPoll" => json!({
                "id":"x","total_voter_count":3,"is_closed":true,
                "options":[{"text":"Да","voter_count":2},{"text":"Нет","voter_count":1}],
            }),
            _ => json!(true),
        }
    }
}

fn rich_blocks(rich: &Value) -> Vec<Value> {
    let parsed = match rich {
        Value::String(text) => serde_json::from_str::<Value>(text).unwrap_or(Value::Null),
        other => other.clone(),
    };
    if let Some(blocks) = parsed["blocks"].as_array() {
        return blocks.clone();
    }
    if let Some(text) = parsed["markdown"]
        .as_str()
        .or_else(|| parsed["html"].as_str())
    {
        return vec![json!({"type":"paragraph","text":text})];
    }
    Vec::new()
}

pub(crate) async fn fake_telegram() -> (String, FakeTelegram) {
    let fake = FakeTelegram::new();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("address: {error}"));
    let server = fake.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut data = Vec::new();
            let mut buffer = [0u8; 8192];
            loop {
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                data.extend_from_slice(&buffer[..read]);
                if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&data);
            let request_line = request.lines().next().unwrap_or_default();
            let mut parts = request_line.split_whitespace();
            let http_method = parts.next().unwrap_or_default();
            let target = parts.next().unwrap_or_default().to_owned();
            let body = request
                .split_once("\r\n\r\n")
                .map(|(_, body)| body)
                .unwrap_or_default();
            let response_body = if http_method == "GET" && target == "/log" {
                Value::Array(server.all_calls()).to_string()
            } else {
                let payload = serde_json::from_str::<Value>(body).unwrap_or_else(|_| json!({}));
                let method = target.rsplit('/').next().unwrap_or_default().to_owned();
                server.record(&method, &payload);
                server
                    .take(&method)
                    .unwrap_or_else(|| json!({"ok":true,"result":server.answer(&method,&payload)}))
                    .to_string()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            if socket.write_all(response.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    (format!("http://{address}"), fake)
}
