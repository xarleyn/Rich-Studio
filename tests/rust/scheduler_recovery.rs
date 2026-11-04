use axum::{Json, Router, extract::State, routing::post};
use rich_studio::{
    AppState,
    account_context::AccountContext,
    services::{in_flight, loops, scheduler},
    settings::Settings,
    store::{
        db::{self, Store},
        meta, posts,
    },
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tempfile::TempDir;
use tokio::sync::Notify;

#[derive(Clone)]
struct Mock {
    calls: Arc<AtomicUsize>,
    mode: u8,
    accepted: Arc<Notify>,
    release: Arc<Notify>,
}
struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn reply(
    State(state): State<Mock>,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    state.calls.fetch_add(1, Ordering::SeqCst);
    if state.mode == 2 {
        state.accepted.notify_one();
        state.release.notified().await;
    }
    let status = if state.mode == 1 {
        429
    } else if state.mode == 3 {
        502
    } else {
        200
    };
    let result = match state.mode {
        1 => {
            json!({"ok":false,"error_code":429,"description":"wait","parameters":{"retry_after":120}})
        }
        3 => json!({"ok":false,"description":"lost upstream"}),
        _ => json!({"ok":true,"result":{"message_id":42,"chat":{"id":body["chat_id"]}}}),
    };
    (
        axum::http::StatusCode::from_u16(status).unwrap_or_else(|error| panic!("status: {error}")),
        Json(result),
    )
}
async fn mock(mode: u8) -> (String, Mock, Server) {
    let state = Mock {
        calls: Arc::new(AtomicUsize::new(0)),
        mode,
        accepted: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("listen: {error}"));
    let base = format!(
        "http://{}",
        listener
            .local_addr()
            .unwrap_or_else(|error| panic!("address: {error}"))
    );
    let app = Router::new()
        .route("/{*path}", post(reply))
        .with_state(state.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .unwrap_or_else(|error| panic!("mock: {error}"));
    });
    (base, state, Server(task))
}
fn app(dir: &TempDir, base: &str) -> AppState {
    let settings = Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), base.to_owned()),
        ("TG_BOT_TOKEN".to_owned(), "1:x".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
        ("SCHEDULE_LATE_MIN".to_owned(), "60".to_owned()),
    ]))
    .unwrap_or_else(|error| panic!("settings: {error}"));
    AppState::new(settings)
}
fn add(context: &AccountContext, at: i64) -> String {
    posts::add_scheduled(
        context,
        &[json!(-100123)],
        &json!({"rich_message":{"markdown":"scheduled"}}),
        at,
        None,
        "rich",
        "api",
    )
    .unwrap_or_else(|error| panic!("scheduled row: {error}"))[0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("post id"))
        .to_owned()
}
#[tokio::test]
async fn due_claim_and_recovery_never_repeat_confirmed_or_unknown_sends() {
    let dir = TempDir::new().unwrap();
    let (base, mock, _server) = mock(0).await;
    let state = app(&dir, &base);
    let id = add(&state.account, 1000);
    let stop = Arc::new(AtomicBool::new(false));
    assert_eq!(
        scheduler::run_due(&state, state.account.clone(), 1001, stop.clone())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        posts::get(&state.account, &id).unwrap().unwrap()["status"],
        "sent"
    );
    assert_eq!(
        scheduler::run_due(&state, state.account.clone(), 1002, stop)
            .await
            .unwrap(),
        0
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    let interrupted = add(&state.account, 1000);
    assert!(rich_studio::store::scheduler::claim(&state.account, &interrupted, 1001).unwrap());
    assert_eq!(in_flight::recover(&state.account).unwrap(), 1);
    assert_eq!(
        posts::get(&state.account, &interrupted).unwrap().unwrap()["status"],
        "failed"
    );
}

#[test]
fn stale_due_snapshot_cannot_send_or_mark_missed_a_rescheduled_post() {
    let dir = TempDir::new().unwrap();
    let state = app(&dir, "http://127.0.0.1:1");
    let id = add(&state.account, 1000);
    assert_eq!(
        rich_studio::store::scheduler::due(&state.account, 5000)
            .unwrap()
            .len(),
        1
    );
    assert!(rich_studio::store::editing::reschedule(&state.account, &id, 9000).unwrap());
    assert!(
        in_flight::claim(&state.account, &id, 5000)
            .unwrap()
            .is_none()
    );
    assert!(!rich_studio::store::scheduler::missed(&state.account, &id, "stale snapshot").unwrap());
    let row = posts::get(&state.account, &id).unwrap().unwrap();
    assert_eq!(row["status"], "scheduled");
    assert_eq!(row["scheduled_at"], 9000);
    let claim = in_flight::claim(&state.account, &id, 9001)
        .unwrap()
        .unwrap();
    assert!(!rich_studio::store::editing::reschedule(&state.account, &id, 10000).unwrap());
    drop(claim);
}
#[tokio::test]
async fn rate_limit_is_requeued_but_ambiguous_gateway_is_never_retried() {
    for (mode, status, retry) in [(1, "scheduled", Some(1121)), (3, "failed", None)] {
        let dir = TempDir::new().unwrap();
        let (base, mock, _server) = mock(mode).await;
        let state = app(&dir, &base);
        let id = add(&state.account, 1000);
        scheduler::run_due(
            &state,
            state.account.clone(),
            1001,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        let row = posts::get(&state.account, &id).unwrap().unwrap();
        assert_eq!(row["status"], status);
        assert_eq!(row["retry_at"].as_i64(), retry);
        scheduler::run_due(
            &state,
            state.account.clone(),
            1002,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn stopped_and_late_queues_do_not_publish() {
    let dir = TempDir::new().unwrap();
    let (base, mock, _server) = mock(0).await;
    let state = app(&dir, &base);
    let id = add(&state.account, 1000);
    assert_eq!(
        scheduler::run_due(
            &state,
            state.account.clone(),
            1001,
            Arc::new(AtomicBool::new(true))
        )
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        posts::get(&state.account, &id).unwrap().unwrap()["status"],
        "scheduled"
    );
    scheduler::run_due(
        &state,
        state.account.clone(),
        5000,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    assert_eq!(
        posts::get(&state.account, &id).unwrap().unwrap()["status"],
        "missed"
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn cancellation_after_accept_leaves_a_marker_and_live_sends_are_excluded_from_recovery() {
    let dir = TempDir::new().unwrap();
    let (base, mock, _server) = mock(2).await;
    let state = app(&dir, &base);
    let id = add(&state.account, 1000);
    let cloned = state.clone();
    let task = tokio::spawn(async move {
        scheduler::run_due(
            &cloned,
            cloned.account.clone(),
            1001,
            Arc::new(AtomicBool::new(false)),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), mock.accepted.notified())
        .await
        .unwrap();
    assert_eq!(in_flight::recover(&state.account).unwrap(), 0);
    assert_eq!(
        posts::get(&state.account, &id).unwrap().unwrap()["status"],
        "sending"
    );
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(in_flight::recover(&state.account).unwrap(), 1);
    scheduler::run_due(
        &state,
        state.account.clone(),
        1002,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    assert!(
        posts::get(&state.account, &id).unwrap().unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("мог уйти")
    );
}
#[test]
fn leases_and_webhook_claims_are_atomic_and_survive_reopen() {
    let dir = TempDir::new().unwrap();
    let state = app(&dir, "http://127.0.0.1:9");
    let context = state.account;
    assert!(meta::take_lease(&context, "loops", "one", 60.0, 1000.0).unwrap());
    assert!(!meta::take_lease(&context, "loops", "two", 60.0, 1001.0).unwrap());
    assert!(meta::take_lease(&context, "loops", "one", 60.0, 1050.0).unwrap());
    assert!(!meta::take_lease(&context, "loops", "two", 60.0, 1061.0).unwrap());
    assert!(meta::take_lease(&context, "loops", "two", 60.0, 1111.0).unwrap());
    meta::drop_lease(&context, "loops", "one").unwrap();
    assert_eq!(
        meta::lease_holder(&context, "loops").unwrap().as_deref(),
        Some("two")
    );
    assert!(meta::claim_webhook_update(&context, 77, 1000).unwrap());
    assert!(!meta::claim_webhook_update(&context, 77, 1001).unwrap());
    assert!(loops::may_keep_leading(
        true,
        std::time::Duration::from_secs(39)
    ));
    assert!(!loops::may_keep_leading(
        true,
        std::time::Duration::from_secs(40)
    ));
    assert!(!loops::may_keep_leading(false, std::time::Duration::ZERO));
    let mut connection = Store::connect(&context).unwrap();
    db::migrate(&mut connection).unwrap();
}
#[tokio::test]
async fn no_connection_is_definite_and_safe_to_retry() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let dir = TempDir::new().unwrap();
    let state = app(&dir, &base);
    let id = add(&state.account, 1000);
    scheduler::run_due(
        &state,
        state.account.clone(),
        1001,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    let row = posts::get(&state.account, &id).unwrap().unwrap();
    assert_eq!(row["status"], "scheduled");
    assert_eq!(row["retry_at"], 1061);
}
