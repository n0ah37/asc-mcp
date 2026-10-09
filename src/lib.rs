//! `asc-mcp` — an MCP server exposing the Apple App Store Connect API.
//!
//! The binary in `main.rs` is a thin wrapper over this library: it reads
//! [`config::Config`] from the environment and serves [`server::AppStoreServer`]
//! over stdio. Keeping the logic in a library is what lets the integration tests
//! in `tests/` drive a real server against a mock API.
//!
//! Module map:
//!
//! - [`config`] — every knob, read from the environment, never fatal.
//! - [`auth`] — ES256 JWT minting and caching.
//! - [`client`] — the authenticated HTTP client: timeouts, retries, pagination.
//! - [`retry`] — which failures are safe to replay, and how long to wait.
//! - [`upload`] — the reserve → upload → commit asset protocol.
//! - [`report`] — decoding gzipped analytics report segments.
//! - [`json`] — trimming responses to fit an agent's context budget.
//! - [`server`] — the tools themselves, grouped by domain.

pub mod auth;
pub mod client;
pub mod config;
pub mod error;
pub mod json;
pub mod report;
pub mod retry;
pub mod server;
pub mod spec;
pub mod upload;

#[cfg(test)]
pub(crate) mod testing;
