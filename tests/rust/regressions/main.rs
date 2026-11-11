//! Regression scenarios ported from the Python reference `tests/test_regressions.py`.
//!
//! Each case protects a bug that was once visible to a user. Keep new cases
//! close to the behaviour they guard instead of grouping them by implementation.
//!
//! Cases already covered elsewhere are mapped instead of duplicated:
//! - `test_favicon_is_not_a_404` → `public_site::icons_discovery_head_and_fingerprint_cache_are_usable`
//! - `test_comment_exception_restores_normal_transport_policy` (refused path) →
//!   `events::reply_transport_failure_restores_account_policy`; the successful path is
//!   kept here as `linked_comment_does_not_change_transport_policy`.

mod delivery;
mod drafts;
mod quotas;
mod resilience;
mod rights;
mod support;
mod updates;
