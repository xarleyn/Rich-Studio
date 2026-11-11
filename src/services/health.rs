//! Read-only health checks; public readiness exposes only failing check names.

mod database;
mod loops;
mod report;
mod storage;
mod telegram;

pub use report::{ReadyCache, report};
