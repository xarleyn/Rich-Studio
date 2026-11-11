use crate::{
    AppState,
    errors::ServiceError,
    services::{accounts, blocking, in_flight, scheduler, updates},
    store::meta,
};
use serde::Serialize;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tokio::{sync::watch, task::JoinHandle};

pub const LEASE_TTL: f64 = 60.0;
pub const RENEW: u64 = 20;
#[derive(Clone, Debug, Serialize)]
pub struct LoopStatus {
    pub role: String,
    pub owner: String,
    pub holder: Option<String>,
    pub stopping: bool,
    pub error: Option<String>,
}
impl Default for LoopStatus {
    fn default() -> Self {
        Self {
            role: "off".to_owned(),
            owner: format!("{}:{}", std::process::id(), uuid::Uuid::new_v4()),
            holder: None,
            stopping: false,
            error: None,
        }
    }
}

pub struct Loops {
    stop: watch::Sender<bool>,
    draining: watch::Sender<bool>,
    keeper: Option<JoinHandle<Result<(), ServiceError>>>,
}
impl Loops {
    pub fn start(state: AppState) -> Self {
        let (stop, receiver) = watch::channel(false);
        let (draining, drain_receiver) = watch::channel(false);
        let keeper = state
            .settings
            .background
            .then(|| tokio::spawn(keep(state, receiver, drain_receiver)));
        Self {
            stop,
            draining,
            keeper,
        }
    }
    pub fn request_stop(&self) {
        let _ = self.draining.send(true);
    }
    pub async fn stop(mut self) -> Result<(), ServiceError> {
        self.request_stop();
        let _ = self.stop.send(true);
        if let Some(keeper) = self.keeper.take() {
            keeper
                .await
                .map_err(|_| ServiceError::Internal("lease keeper interrupted".to_owned()))??;
        }
        Ok(())
    }
}
impl Drop for Loops {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        // The owner normally joins in stop; an aborted owner must not leave background sends running.
        if let Some(keeper) = &self.keeper {
            keeper.abort();
        }
    }
}

struct Worker {
    stopping: Arc<AtomicBool>,
    task: JoinHandle<()>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.task.abort();
    }
}
impl Worker {
    fn start(state: AppState) -> Self {
        let stopping = Arc::new(AtomicBool::new(false));
        let flag = stopping.clone();
        let task = tokio::spawn(async move {
            tokio::join!(
                scheduler_loop(&state, flag.clone()),
                update_loop(&state, flag.clone()),
                super::maintenance::run(&state, flag)
            );
        });
        Self { stopping, task }
    }
    async fn stop(mut self, grace: Duration) {
        self.stopping.store(true, Ordering::Release);
        if tokio::time::timeout(grace, &mut self.task).await.is_err() {
            self.task.abort();
            // Cancellation leaves the durable sending marker for recovery.
            let _ = (&mut self.task).await;
        }
    }
}
pub fn may_keep_leading(was_leader: bool, since_success: Duration) -> bool {
    was_leader && since_success.as_secs_f64() < LEASE_TTL - RENEW as f64
}
async fn recover(state: &AppState) -> Result<(), ServiceError> {
    let settings = state.settings.clone();
    let contexts = tokio::task::spawn_blocking(move || accounts::all_contexts(&settings))
        .await
        .map_err(|_| ServiceError::Internal("account worker interrupted".to_owned()))?
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    for context in contexts {
        let recovered = blocking::run(&context, |context| {
            let ids = in_flight::recover_ids(context)?;
            crate::store::bot_dialog::recover_comments(context)?;
            Ok(ids)
        })
        .await;
        match recovered {
            Ok(ids) => {
                let publisher = super::publisher::Publisher::new(
                    context,
                    state.transport(),
                    state.settings.updates_on,
                )
                .with_settings(&state.settings);
                for id in ids {
                    publisher.notify(&id, Some("uncertain"), None).await;
                }
            }
            Err(error) => state.logs.event(
                "ERROR",
                "rich-studio.loops",
                &error.to_string(),
                serde_json::json!({"event":"recovery.error","account":context.account_id}),
            ),
        }
    }
    Ok(())
}

pub(crate) async fn pause(stopping: &AtomicBool, seconds: u64) {
    for _ in 0..seconds {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn scheduler_loop(state: &AppState, stopping: Arc<AtomicBool>) {
    while !stopping.load(Ordering::Acquire) {
        let error = crate::logs::scope(
            crate::logs::new_id("scheduler-"),
            scheduler::run_all(state, stopping.clone()),
        )
        .await
        .err()
        .map(|error| error.to_string());
        if let Some(error) = &error {
            state.loop_status.write().await.error = Some(error.clone());
            state.logs.event(
                "ERROR",
                "rich-studio.scheduler",
                error,
                serde_json::json!({"event":"scheduler.error"}),
            );
        }
        state.metrics.beat("scheduler", 10.0, error.as_deref());
        pause(&stopping, 10).await;
    }
}

async fn update_loop(state: &AppState, stopping: Arc<AtomicBool>) {
    while !stopping.load(Ordering::Acquire) {
        if let Err(error) = updates::run(state.clone(), stopping.clone()).await {
            state.logs.event(
                "ERROR",
                "rich-studio.updates",
                &error.to_string(),
                serde_json::json!({"event":"updates.error"}),
            );
            state
                .metrics
                .beat("updates", 30.0, Some(&error.to_string()));
        }
        pause(&stopping, 5).await;
    }
}
async fn keep(
    state: AppState,
    mut stopped: watch::Receiver<bool>,
    mut draining: watch::Receiver<bool>,
) -> Result<(), ServiceError> {
    let owner = state.loop_status.read().await.owner.clone();
    let mut worker: Option<Worker> = None;
    let mut last_ok = Instant::now();
    loop {
        if *stopped.borrow() {
            break;
        }
        if *draining.borrow() {
            state.loop_status.write().await.stopping = true;
            if let Some(worker) = &worker {
                worker.stopping.store(true, Ordering::Release);
            }
        }
        let claimed_owner = owner.clone();
        let held = blocking::run(&state.account, move |context| {
            Ok(meta::take_lease(
                context,
                "loops",
                &claimed_owner,
                LEASE_TTL,
                scheduler::now() as f64,
            )?)
        })
        .await;
        let leading = match held {
            Ok(held) => {
                last_ok = Instant::now();
                held
            }
            Err(error) => {
                state.loop_status.write().await.error = Some(error.to_string());
                may_keep_leading(worker.is_some(), last_ok.elapsed())
            }
        };
        if leading && worker.is_none() && !*draining.borrow() {
            match recover(&state).await {
                Ok(()) => {
                    // Recovery may scan many databases; establish a fresh lease before any I/O worker starts.
                    let confirmed_owner = owner.clone();
                    let confirmed = blocking::run(&state.account, move |context| {
                        Ok(meta::take_lease(
                            context,
                            "loops",
                            &confirmed_owner,
                            LEASE_TTL,
                            scheduler::now() as f64,
                        )?)
                    })
                    .await;
                    if !matches!(confirmed, Ok(true)) {
                        state.loop_status.write().await.error =
                            Some("lease unavailable after recovery".to_owned());
                    } else {
                        last_ok = Instant::now();
                        worker = Some(Worker::start(state.clone()));
                        let mut status = state.loop_status.write().await;
                        status.role = "leader".to_owned();
                        status.holder = Some(owner.clone());
                    }
                }
                Err(error) => {
                    state.loop_status.write().await.error = Some(error.to_string());
                }
            }
        } else if !leading {
            if let Some(previous) = worker.take() {
                previous.stop(Duration::ZERO).await;
            }
            let holder = blocking::run(&state.account, |context| {
                Ok(meta::lease_holder(context, "loops")?)
            })
            .await;
            let mut status = state.loop_status.write().await;
            status.role = "standby".to_owned();
            status.holder = holder.ok().flatten();
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(RENEW)) => {}
            changed = stopped.changed() => { if changed.is_err() || *stopped.borrow() { break; } }
            _ = draining.changed() => {}
        }
    }
    state.loop_status.write().await.stopping = true;
    if let Some(worker) = worker {
        // Keep renewing during a long graceful drain; a new leader must not race an in-flight send.
        let mut owned = true;
        {
            let drain = worker.stop(Duration::from_secs_f64(state.settings.shutdown_grace));
            tokio::pin!(drain);
            loop {
                tokio::select! {
                    () = &mut drain => break,
                    _ = tokio::time::sleep(Duration::from_secs(RENEW)) => {
                        let owner = owner.clone();
                        owned=blocking::run(&state.account,move |context| Ok(meta::take_lease(context,"loops",&owner,LEASE_TTL,scheduler::now() as f64)?)).await?;
                        if !owned {break;}
                    }
                }
            }
        }
        if owned {
            recover(&state).await?;
        }
    }
    blocking::run(&state.account, move |context| {
        Ok(meta::drop_lease(context, "loops", &owner)?)
    })
    .await?;
    state.loop_status.write().await.role = "off".to_owned();
    Ok(())
}
