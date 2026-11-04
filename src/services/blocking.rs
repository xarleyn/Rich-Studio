//! Finite SQLite and filesystem work runs outside Tokio's async workers.

use crate::account_context::AccountContext;
use crate::errors::ServiceError;

pub async fn run<T, F>(context: &AccountContext, action: F) -> Result<T, ServiceError>
where
    T: Send + 'static,
    F: FnOnce(&AccountContext) -> Result<T, ServiceError> + Send + 'static,
{
    let context = context.clone();
    tokio::task::spawn_blocking(move || action(&context))
        .await
        .map_err(|_| ServiceError::Internal("операция хранилища прервана".to_owned()))?
}
