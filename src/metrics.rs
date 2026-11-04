//! Bounded, per-instance counters. A tenant report never receives these shared process metrics.

use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

const MAX_KEYS: usize = 100;
const LATENCY_KEPT: usize = 200;
const RECENT_KEPT: usize = 30;

#[derive(Default, Debug)]
struct Data {
    requests: BTreeMap<String, u64>,
    telegram: BTreeMap<String, u64>,
    problems: BTreeMap<String, u64>,
    http_ms: BTreeMap<String, VecDeque<u64>>,
    telegram_ms: BTreeMap<String, VecDeque<u64>>,
    loops: BTreeMap<String, Value>,
    recent: VecDeque<Value>,
    errors: VecDeque<Value>,
    telegram_last: Value,
}

#[derive(Debug)]
pub struct Metrics {
    started: Instant,
    data: Mutex<Data>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            data: Mutex::new(Data {
                telegram_last: json!({"ok_at":null,"error":null}),
                ..Data::default()
            }),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
fn bounded_key<T>(series: &BTreeMap<String, T>, key: &str) -> String {
    if series.contains_key(key) || series.len() < MAX_KEYS {
        key.to_owned()
    } else {
        "other".to_owned()
    }
}
fn push<T>(queue: &mut VecDeque<T>, value: T, limit: usize) {
    if queue.len() >= limit {
        queue.pop_front();
    }
    queue.push_back(value);
}
fn observe(series: &mut BTreeMap<String, VecDeque<u64>>, key: &str, value: u64) {
    let key = bounded_key(series, key);
    push(series.entry(key).or_default(), value, LATENCY_KEPT);
}

impl Metrics {
    pub fn http_observe(&self, method: &str, route: &str, status: u16, ms: u64) {
        let mut data = lock(&self.data);
        *data
            .requests
            .entry(format!("{}xx", status / 100))
            .or_default() += 1;
        let method =
            if ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"].contains(&method) {
                method
            } else {
                "OTHER"
            };
        observe(&mut data.http_ms, &format!("{method} {route}"), ms);
    }
    pub fn telegram_call(
        &self,
        method: &str,
        ok: bool,
        error: Option<&str>,
        ms: u64,
        code: Option<i64>,
        quiet: bool,
    ) {
        let mut data = lock(&self.data);
        let key = bounded_key(&data.telegram_ms, method);
        *data
            .telegram
            .entry(format!("{key} {}", if ok { "ok" } else { "error" }))
            .or_default() += 1;
        observe(&mut data.telegram_ms, &key, ms);
        if ok {
            if !quiet
                || data.telegram_last["error"].is_null()
                || data.telegram_last["error"]["quiet"] == true
            {
                data.telegram_last = json!({"ok_at":now(),"error":null});
            }
        } else {
            let message = error.unwrap_or_default();
            if code != Some(400)
                || [
                    "chat not found",
                    "rights",
                    "admin",
                    "upgraded",
                    "thread",
                    "topic",
                    "peer_id",
                    "chat_id",
                    "forbidden",
                    "not a member",
                ]
                .iter()
                .any(|part| message.to_lowercase().contains(part))
            {
                data.telegram_last["error"] = json!({"at":now(),"method":key,"error":message.chars().take(300).collect::<String>(),"code":code,"quiet":quiet});
            }
        }
    }
    pub fn beat(&self, name: &str, interval: f64, error: Option<&str>) {
        let mut data = lock(&self.data);
        let name = bounded_key(&data.loops, name);
        let entry = data.loops.entry(name).or_insert_with(
            || json!({"interval":interval,"beat_at":null,"errors":0,"last_error":null}),
        );
        entry["interval"] = json!(interval);
        entry["beat_at"] = json!(now());
        if let Some(error) = error {
            entry["errors"] = json!(entry["errors"].as_u64().unwrap_or(0) + 1);
            entry["last_error"] =
                json!({"at":now(),"error":error.chars().take(300).collect::<String>()});
        }
    }
    pub fn problem(&self, entry: &Value) {
        let mut data = lock(&self.data);
        let level = entry["level"].as_str().unwrap_or("WARNING");
        *data.problems.entry(level.to_owned()).or_default() += 1;
        let mut problem = json!({"at":now(),"level":level,"logger":entry["logger"],"message":entry["msg"].as_str().unwrap_or_default().chars().take(500).collect::<String>()});
        if let Some(id) = entry.get("request_id") {
            problem["request_id"] = id.clone();
        }
        push(&mut data.recent, problem.clone(), RECENT_KEPT);
        if ["ERROR", "CRITICAL"].contains(&level) && entry["event"] != "health.change" {
            push(&mut data.errors, problem, RECENT_KEPT);
        }
    }
    pub fn uptime(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
    pub fn recent(&self) -> Vec<Value> {
        lock(&self.data).recent.iter().cloned().collect()
    }
    pub fn errors(&self) -> Vec<Value> {
        lock(&self.data).errors.iter().cloned().collect()
    }
    pub fn loops(&self) -> BTreeMap<String, Value> {
        lock(&self.data).loops.clone()
    }
    pub fn telegram_last(&self) -> Value {
        lock(&self.data).telegram_last.clone()
    }
    pub fn snapshot(&self) -> Value {
        let data = lock(&self.data);
        json!({"uptime_s":self.uptime(),"requests":data.requests,"telegram_calls":data.telegram,"log_problems":data.problems,"http_latency":latencies(&data.http_ms),"telegram_latency":latencies(&data.telegram_ms)})
    }
}

fn latencies(series: &BTreeMap<String, VecDeque<u64>>) -> Value {
    let mut output = serde_json::Map::new();
    for (key, values) in series {
        let mut values: Vec<u64> = values.iter().copied().collect();
        values.sort_unstable();
        if values.is_empty() {
            continue;
        }
        let n = values.len();
        let median = if n.is_multiple_of(2) {
            (values[n / 2 - 1] + values[n / 2]) / 2
        } else {
            values[n / 2]
        };
        output.insert(key.clone(),json!({"n":n,"p50_ms":median,"p95_ms":values[(95*n).div_ceil(100)-1],"max_ms":values[n-1]}));
    }
    Value::Object(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn limits_cardinality_and_recent_history() {
        let metrics = Metrics::default();
        for i in 0..500 {
            metrics.http_observe("GET", &format!("/fixture/{i}"), 200, i);
            metrics.problem(&json!({"level":"ERROR","logger":"test","msg":"x"}));
        }
        assert_eq!(
            metrics.snapshot()["http_latency"]
                .as_object()
                .expect("latencies")
                .len(),
            101
        );
        assert_eq!(metrics.recent().len(), 30);
        assert_eq!(metrics.errors().len(), 30);
        assert_eq!(metrics.snapshot()["requests"]["2xx"], 500);
    }
}
