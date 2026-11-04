//! Contract tests ported from the Python reference `tests/test_api.py`.
//!
//! The HTTP/MCP surface is shared with `http_mcp_contract.rs`; this target keeps
//! the larger end-to-end API scenarios separate so they can grow without making
//! that file unwieldy.

mod interactive;
mod polls;
mod posts;
mod publishing;
mod render;
mod schedule;
mod security;
mod spec;
mod support;
mod updates;
mod validation;
