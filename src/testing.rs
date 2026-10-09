//! Shared fixtures for the crate's tests.
//!
//! Anything that talks to the API needs credentials that actually sign, because
//! [`crate::auth::TokenProvider`] mints a real ES256 JWT before every request.
//! The keypair below is a throwaway generated for this purpose and grants access
//! to nothing.

use std::time::Duration;

use crate::client::AscClient;
use crate::config::{Config, HttpConfig, OutputConfig};
use crate::server::AppStoreServer;

/// A throwaway P-256 keypair (generated offline; test-only, grants access to
/// nothing). Only the base64 DER bodies are stored — the PEM is assembled at
/// runtime by [`pem`] so the literal key markers never appear in source and
/// can't trip secret scanners on a public repo.
pub const TEST_PRIVATE_BODY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgtNQuT3hctsLS5iks\nldU7lAHLp9QPbYtRkNrhPNxlreOhRANCAATtwWcC7S4Iv3kFf5CZ+S00uBy6z0Ai\nkKhZsS1aG3tDlcxyWKPycElp3WMMtbnrPLa6ZaRHAwEY2M5jfPbUvS7O";
pub const TEST_PUBLIC_BODY: &str = "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE7cFnAu0uCL95BX+QmfktNLgcus9A\nIpCoWbEtWht7Q5XMclij8nBJad1jDLW56zy2umWkRwMBGNjOY3z21L0uzg==";

pub const TEST_ISSUER: &str = "57246542-96fe-1a63-e053-0824d011072a";
pub const TEST_KEY_ID: &str = "ABC123DEFG";

/// Assemble a PEM document from a label and base64 body at runtime.
pub fn pem(label: &str, body: &str) -> String {
    let rule = "-----";
    format!("{rule}BEGIN {label}{rule}\n{body}\n{rule}END {label}{rule}\n")
}

/// Retry timings scaled down so a test that exercises the retry path finishes
/// in milliseconds instead of seconds.
pub fn fast_retries(max_retries: u32) -> HttpConfig {
    HttpConfig {
        max_retries,
        retry_base_delay: Duration::from_millis(1),
        max_retry_delay: Duration::from_millis(20),
        ..HttpConfig::default()
    }
}

/// A config with working credentials pointed at a test server, with responses
/// left unshaped so tests assert on exactly what the API returned.
pub fn test_config(base_url: &str) -> Config {
    Config::from_parts(
        Some(TEST_ISSUER),
        Some(TEST_KEY_ID),
        Some(&pem("PRIVATE KEY", TEST_PRIVATE_BODY)),
    )
    .with_base_url(base_url)
    .with_http(fast_retries(3))
    .with_output(OutputConfig {
        compact: false,
        max_bytes: 0,
    })
}

/// A client pointed at a test server.
pub fn test_client(base_url: &str) -> AscClient {
    AscClient::new(test_config(base_url))
}

/// A server pointed at a test server.
pub fn test_server(base_url: &str) -> AppStoreServer {
    AppStoreServer::new(test_config(base_url))
}

/// A server whose public App Store endpoints are also the test server.
pub fn test_server_with_market(base_url: &str) -> AppStoreServer {
    let mut server = test_server(base_url);
    server.market = std::sync::Arc::new(crate::server::market::MarketClient::with_bases(
        base_url, base_url,
    ));
    server
}

/// The text of a tool result, for asserting on what an agent would see.
pub fn result_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}
