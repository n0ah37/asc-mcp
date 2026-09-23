//! Environment-based configuration for the App Store Connect MCP server.
//!
//! Reading config is intentionally **infallible** ([`Config::from_env`] never
//! errors) so the MCP server always starts and can advertise its tools. Missing
//! or invalid credentials only surface when a tool actually needs to call the
//! API, via [`Config::credentials`], which returns an actionable error.
//!
//! The same principle applies to the tuning knobs below: an unparseable value
//! logs a warning and falls back to the default rather than refusing to start.

use std::time::Duration;

use crate::error::AscError;

/// Default App Store Connect API origin.
pub const DEFAULT_BASE_URL: &str = "https://api.appstoreconnect.apple.com";

/// Transport tuning: how long to wait, and how hard to retry.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// TCP/TLS connect timeout. `None` disables it.
    pub connect_timeout: Option<Duration>,
    /// Whole-request timeout for ordinary API calls. `None` disables it.
    pub request_timeout: Option<Duration>,
    /// Whole-request timeout for bulk transfers — asset-upload chunk `PUT`s and
    /// analytics report downloads — which move far more bytes than an API call
    /// and legitimately take longer. `None` disables it.
    pub transfer_timeout: Option<Duration>,
    /// Retries attempted *after* the first try. `0` disables retrying.
    pub max_retries: u32,
    /// First backoff delay; each subsequent retry doubles it.
    pub retry_base_delay: Duration,
    /// Ceiling for any single backoff wait, including a server `Retry-After`.
    /// Bounds the worst case so a tool call cannot hang for minutes.
    pub max_retry_delay: Duration,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Some(Duration::from_secs(10)),
            request_timeout: Some(Duration::from_secs(60)),
            transfer_timeout: Some(Duration::from_secs(300)),
            max_retries: 3,
            retry_base_delay: Duration::from_millis(500),
            max_retry_delay: Duration::from_secs(30),
        }
    }
}

impl HttpConfig {
    fn from_env() -> Self {
        let d = Self::default();
        Self {
            connect_timeout: env_timeout("ASC_CONNECT_TIMEOUT_SECS", d.connect_timeout),
            request_timeout: env_timeout("ASC_TIMEOUT_SECS", d.request_timeout),
            transfer_timeout: env_timeout("ASC_TRANSFER_TIMEOUT_SECS", d.transfer_timeout),
            max_retries: env_parse("ASC_MAX_RETRIES", d.max_retries),
            ..d
        }
    }
}

/// How tool results are rendered back to the agent.
///
/// App Store Connect responses are verbose JSON:API documents; left untouched a
/// single list call can swamp an agent's context. These knobs bound that cost.
#[derive(Debug, Clone)]
pub struct OutputConfig {
    /// Strip per-resource `links` and link-only relationships from responses.
    /// Purely redundant navigation metadata — no semantic content is lost.
    pub compact: bool,
    /// Soft ceiling on the serialized size of a tool result, in bytes. Oversized
    /// documents shed `included` resources and then `data` items, and carry a
    /// note saying what was dropped. `0` disables the cap.
    pub max_bytes: usize,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            compact: true,
            max_bytes: 60_000,
        }
    }
}

impl OutputConfig {
    fn from_env() -> Self {
        let d = Self::default();
        Self {
            compact: env_bool("ASC_COMPACT_RESPONSES", d.compact),
            max_bytes: env_parse("ASC_MAX_RESPONSE_BYTES", d.max_bytes),
        }
    }
}

/// Which tools this server exposes.
#[derive(Debug, Clone, Default)]
pub struct ToolsConfig {
    /// Serve only tools that cannot modify the account, and reject non-`GET`
    /// requests through the generic escape hatch.
    pub read_only: bool,
    /// Raw `ASC_TOOLS` value (comma-separated group names, or a preset).
    /// `None` exposes every tool. Parsed by the server's tool catalog.
    pub groups: Option<String>,
    /// Expose only discovery tools; keep filtered domain tools callable through
    /// `call_discovered_tool` after inspection.
    pub discovery: bool,
}

impl ToolsConfig {
    fn from_env() -> Self {
        Self {
            read_only: env_bool("ASC_READ_ONLY", false),
            groups: non_empty(std::env::var("ASC_TOOLS").ok()),
            discovery: env_bool("ASC_TOOL_DISCOVERY", false),
        }
    }
}

/// Resolved configuration captured from the process environment.
#[derive(Debug, Clone)]
pub struct Config {
    issuer_id: Option<String>,
    key_id: Option<String>,
    /// The private key PEM contents, resolved from `ASC_PRIVATE_KEY` or by
    /// reading the file at `ASC_PRIVATE_KEY_PATH`.
    private_key_pem: Option<String>,
    /// A problem encountered while resolving the key (e.g. file unreadable),
    /// surfaced lazily so it can be reported when credentials are requested.
    private_key_error: Option<String>,
    /// API origin; overridable via `ASC_BASE_URL`.
    pub base_url: String,
    pub http: HttpConfig,
    pub output: OutputConfig,
    pub tools: ToolsConfig,
}

/// Borrowed, fully-validated credentials ready to mint a JWT.
pub struct Credentials<'a> {
    pub issuer_id: &'a str,
    pub key_id: &'a str,
    pub private_key_pem: &'a str,
}

impl Config {
    /// Read configuration from the environment. Never fails.
    ///
    /// Credentials:
    /// - `ASC_ISSUER_ID` (required) — your App Store Connect issuer UUID.
    /// - `ASC_KEY_ID` (required) — the API key ID.
    /// - `ASC_PRIVATE_KEY` — the `.p8` PEM contents, or
    /// - `ASC_PRIVATE_KEY_PATH` — a path to the `.p8` file (one of the two is required).
    ///
    /// Behaviour:
    /// - `ASC_BASE_URL` — overrides the API origin.
    /// - `ASC_TIMEOUT_SECS`, `ASC_CONNECT_TIMEOUT_SECS`, `ASC_TRANSFER_TIMEOUT_SECS` — `0` disables.
    /// - `ASC_MAX_RETRIES` — retries after the first attempt (default 3).
    /// - `ASC_COMPACT_RESPONSES` — strip redundant JSON:API links (default on).
    /// - `ASC_MAX_RESPONSE_BYTES` — tool-result size cap (default 60000, `0` disables).
    /// - `ASC_TOOLS` — comma-separated tool groups to expose (default all).
    /// - `ASC_READ_ONLY` — expose only non-mutating tools.
    /// - `ASC_TOOL_DISCOVERY` — expose three discovery tools instead of domain tools.
    pub fn from_env() -> Self {
        let issuer_id = non_empty(std::env::var("ASC_ISSUER_ID").ok());
        let key_id = non_empty(std::env::var("ASC_KEY_ID").ok());

        let (private_key_pem, private_key_error) = resolve_private_key();

        let base_url = non_empty(std::env::var("ASC_BASE_URL").ok())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        Self {
            issuer_id,
            key_id,
            private_key_pem,
            private_key_error,
            base_url,
            http: HttpConfig::from_env(),
            output: OutputConfig::from_env(),
            tools: ToolsConfig::from_env(),
        }
    }

    /// Validate and borrow the credential triple, or report exactly what's missing.
    pub fn credentials(&self) -> Result<Credentials<'_>, AscError> {
        let mut missing = Vec::new();
        if self.issuer_id.is_none() {
            missing.push("ASC_ISSUER_ID");
        }
        if self.key_id.is_none() {
            missing.push("ASC_KEY_ID");
        }
        if self.private_key_pem.is_none() {
            if let Some(err) = &self.private_key_error {
                return Err(AscError::Config(err.clone()));
            }
            missing.push("ASC_PRIVATE_KEY or ASC_PRIVATE_KEY_PATH");
        }

        if !missing.is_empty() {
            return Err(AscError::Config(format!(
                "set the following environment variable(s): {}",
                missing.join(", ")
            )));
        }

        Ok(Credentials {
            issuer_id: self.issuer_id.as_deref().unwrap(),
            key_id: self.key_id.as_deref().unwrap(),
            private_key_pem: self.private_key_pem.as_deref().unwrap(),
        })
    }

    /// Whether the required credentials appear to be present (for diagnostics).
    pub fn is_configured(&self) -> bool {
        self.credentials().is_ok()
    }

    /// Build a config directly from explicit parts, bypassing the environment.
    /// Everything else takes its default; use the `with_*` setters to adjust.
    pub fn from_parts(
        issuer_id: Option<&str>,
        key_id: Option<&str>,
        private_key_pem: Option<&str>,
    ) -> Self {
        Self {
            issuer_id: issuer_id.map(String::from),
            key_id: key_id.map(String::from),
            private_key_pem: private_key_pem.map(String::from),
            private_key_error: None,
            base_url: DEFAULT_BASE_URL.to_string(),
            http: HttpConfig::default(),
            output: OutputConfig::default(),
            tools: ToolsConfig::default(),
        }
    }

    /// Point the client at a different API origin (a test server, typically).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_http(mut self, http: HttpConfig) -> Self {
        self.http = http;
        self
    }

    pub fn with_output(mut self, output: OutputConfig) -> Self {
        self.output = output;
        self
    }

    pub fn with_tools(mut self, tools: ToolsConfig) -> Self {
        self.tools = tools;
        self
    }
}

/// Resolve the PEM either from `ASC_PRIVATE_KEY` (inline) or `ASC_PRIVATE_KEY_PATH` (file).
fn resolve_private_key() -> (Option<String>, Option<String>) {
    if let Some(pem) = non_empty(std::env::var("ASC_PRIVATE_KEY").ok()) {
        return (Some(pem), None);
    }
    if let Some(path) = non_empty(std::env::var("ASC_PRIVATE_KEY_PATH").ok()) {
        return match std::fs::read_to_string(&path) {
            Ok(pem) => (Some(pem), None),
            Err(e) => (
                None,
                Some(format!(
                    "could not read ASC_PRIVATE_KEY_PATH ('{path}'): {e}"
                )),
            ),
        };
    }
    (None, None)
}

/// Treat empty/whitespace-only env values as absent.
fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read and parse an env var, warning and keeping `default` when it is unset or invalid.
fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    parse_value(key, non_empty(std::env::var(key).ok()), default)
}

/// Read a timeout in seconds from the environment, where `0` means "no timeout".
fn env_timeout(key: &str, default: Option<Duration>) -> Option<Duration> {
    parse_timeout(key, non_empty(std::env::var(key).ok()), default)
}

/// Read a boolean env var. Accepts `1/true/yes/on` and `0/false/no/off`.
fn env_bool(key: &str, default: bool) -> bool {
    parse_bool(key, non_empty(std::env::var(key).ok()), default)
}

fn parse_value<T: std::str::FromStr>(key: &str, raw: Option<String>, default: T) -> T {
    match raw {
        None => default,
        Some(raw) => raw.parse().unwrap_or_else(|_| {
            tracing::warn!("{key}='{raw}' is not a valid value; using the default");
            default
        }),
    }
}

fn parse_timeout(key: &str, raw: Option<String>, default: Option<Duration>) -> Option<Duration> {
    match raw {
        None => default,
        Some(raw) => match raw.parse::<u64>() {
            Ok(0) => None,
            Ok(secs) => Some(Duration::from_secs(secs)),
            Err(_) => {
                tracing::warn!("{key}='{raw}' is not a whole number of seconds; using the default");
                default
            }
        },
    }
}

fn parse_bool(key: &str, raw: Option<String>, default: bool) -> bool {
    match raw {
        None => default,
        Some(raw) => match raw.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            other => {
                tracing::warn!("{key}='{other}' is not a boolean; using the default ({default})");
                default
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_credentials_are_named_individually() {
        // Destructured rather than `unwrap_err()` on purpose: `Credentials`
        // deliberately has no `Debug`, so a private key can never reach a log.
        let Err(err) = Config::from_parts(None, Some("KEY"), Some("pem")).credentials() else {
            panic!("expected the missing issuer ID to be reported");
        };
        let msg = err.to_string();
        assert!(msg.contains("ASC_ISSUER_ID"), "{msg}");
        assert!(!msg.contains("ASC_KEY_ID"), "{msg}");
    }

    #[test]
    fn complete_credentials_validate() {
        let config = Config::from_parts(Some("iss"), Some("kid"), Some("pem"));
        assert!(config.is_configured());
        let creds = config.credentials().unwrap();
        assert_eq!(creds.issuer_id, "iss");
        assert_eq!(creds.key_id, "kid");
    }

    #[test]
    fn defaults_are_conservative() {
        let http = HttpConfig::default();
        assert!(
            http.request_timeout.is_some(),
            "a stalled call must not hang"
        );
        assert!(http.connect_timeout.is_some());
        assert!(http.max_retries >= 1);
        assert!(http.transfer_timeout > http.request_timeout);

        let output = OutputConfig::default();
        assert!(output.compact);
        assert!(output.max_bytes > 0);
    }

    #[test]
    fn timeouts_parse_zero_as_disabled_and_bad_input_as_default() {
        let default = Some(Duration::from_secs(60));
        assert_eq!(parse_timeout("K", None, default), default);
        assert_eq!(parse_timeout("K", Some("0".into()), default), None);
        assert_eq!(
            parse_timeout("K", Some("5".into()), default),
            Some(Duration::from_secs(5))
        );
        // Garbage must not silently disable the timeout — that is the unsafe direction.
        assert_eq!(parse_timeout("K", Some("soon".into()), default), default);
    }

    #[test]
    fn booleans_accept_common_spellings() {
        for yes in ["1", "true", "TRUE", "yes", "on"] {
            assert!(parse_bool("K", Some(yes.into()), false), "{yes}");
        }
        for no in ["0", "false", "No", "off"] {
            assert!(!parse_bool("K", Some(no.into()), true), "{no}");
        }
        assert!(parse_bool("K", Some("maybe".into()), true));
        assert!(!parse_bool("K", None, false));
    }

    #[test]
    fn numeric_values_fall_back_on_garbage() {
        assert_eq!(parse_value("K", Some("7".into()), 3u32), 7);
        assert_eq!(parse_value("K", Some("lots".into()), 3u32), 3);
        assert_eq!(parse_value("K", None, 3u32), 3);
    }

    #[test]
    fn with_setters_override_defaults() {
        let config = Config::from_parts(None, None, None)
            .with_base_url("http://localhost:1234")
            .with_output(OutputConfig {
                compact: false,
                max_bytes: 0,
            });
        assert_eq!(config.base_url, "http://localhost:1234");
        assert!(!config.output.compact);
        assert_eq!(config.output.max_bytes, 0);
    }
}
