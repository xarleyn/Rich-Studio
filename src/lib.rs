use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use tokio::sync::RwLock;

pub mod account_context;
pub mod api;
pub mod errors;
pub mod logs;
pub mod mcp_tools;
pub mod metrics;
pub mod public_site;
pub mod runtime;
pub mod security;
pub mod services;
pub mod settings;
pub mod spec;
pub mod store;
pub mod telegram;

use account_context::AccountContext;
use settings::Settings;
use spec::source::Spec;

#[derive(Clone, Debug)]
pub struct AppState {
    pub(crate) static_dir: PathBuf,
    pub settings: Arc<Settings>,
    pub account: AccountContext,
    pub principal: security::Principal,
    pub loop_status: Arc<RwLock<services::loops::LoopStatus>>,
    pub updates_status: Arc<RwLock<services::updates::Status>>,
    pub ready_cache: Arc<services::health::ReadyCache>,
    pub logs: Arc<logs::Logger>,
    pub metrics: Arc<metrics::Metrics>,
    pub spec: Arc<RwLock<Spec>>,
    pub spec_refresh: Arc<tokio::sync::Mutex<()>>,
}

impl AppState {
    pub fn transport(&self) -> telegram::client::TelegramTransport {
        telegram::client::TelegramTransport::new(
            self.settings.api_base.clone(),
            self.settings.bot_token.clone(),
        )
        .with_observability(self.metrics.clone(), self.logs.clone())
    }
    pub fn new(settings: Settings) -> Self {
        let metrics = Arc::new(metrics::Metrics::default());
        let logs = Arc::new(logs::Logger::new(&settings, metrics.clone()));
        Self {
            static_dir: runtime::static_dir(),
            account: AccountContext::root(&settings),
            principal: security::Principal::owner(),
            loop_status: Arc::new(RwLock::new(services::loops::LoopStatus::default())),
            updates_status: Arc::new(RwLock::new(services::updates::Status::new(&settings))),
            ready_cache: Arc::new(services::health::ReadyCache::default()),
            logs,
            metrics,
            spec: Arc::new(RwLock::new(Spec::load_initial(&settings))),
            spec_refresh: Arc::new(tokio::sync::Mutex::new(())),
            settings: Arc::new(settings),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        let settings = Settings::from_map(&BTreeMap::new())
            .unwrap_or_else(|error| panic!("default settings: {error}"));
        Self::new(settings)
    }
}

pub fn build_router(state: AppState) -> Router {
    api::router(state)
}

pub fn bind_address(configured: Option<String>) -> String {
    configured.unwrap_or_else(|| "127.0.0.1:7424".to_owned())
}
