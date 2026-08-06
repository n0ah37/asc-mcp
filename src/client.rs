//! The authenticated App Store Connect HTTP client.
//!
//! [`AscClient`] owns a `reqwest` client, the resolved [`Config`], and a
//! [`TokenProvider`]. Every API call goes through [`AscClient::request`], which
//! injects the bearer token, sends the request, retries the failures worth
//! retrying (see [`crate::retry`]), and maps non-2xx responses into
//! [`AscError::Api`] with parsed JSON:API error details.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use reqwest::{header::RETRY_AFTER, Client, Method, RequestBuilder, Response};
use serde_json::{json, Value};

use crate::auth::TokenProvider;
use crate::config::Config;
use crate::error::{api_error, AscError};
use crate::retry;

/// Hard ceiling on `max_pages`, so a runaway auto-paginating call can't walk a
/// collection of tens of thousands of resources.
pub const MAX_PAGES_LIMIT: u32 = 20;

/// Authenticated client for `https://api.appstoreconnect.apple.com`.
pub struct AscClient {
    /// Public within the crate so the upload workflow (a separate module) can
    /// issue un-authenticated `PUT`s to Apple's pre-signed upload URLs.
    pub(crate) http: Client,
    pub(crate) config: Arc<Config>,
    auth: TokenProvider,
}

impl AscClient {
    /// Build a client from configuration. Construction never performs I/O or
    /// validates credentials, so the server can start without them.
    pub fn new(config: Config) -> Self {
        let config = Arc::new(config);
        let mut builder =
            Client::builder().user_agent(concat!("appstore-mcp/", env!("CARGO_PKG_VERSION")));
        // Without these a stalled connection hangs the tool call forever, and an
        // MCP client on stdio has no way to cancel it.
        if let Some(t) = config.http.connect_timeout {
            builder = builder.connect_timeout(t);
        }
        if let Some(t) = config.http.request_timeout {
            builder = builder.timeout(t);
        }
        let http = builder.build().expect("failed to build reqwest client");
        let auth = TokenProvider::new(config.clone());
        Self { http, config, auth }
    }

    /// Perform an authenticated JSON request against the API.
    ///
    /// `path` may be a server-relative path (`"/v1/apps"`, `"v2/inAppPurchases/{id}"`)
    /// or an absolute URL. `query` is a list of pre-stringified key/value pairs;
    /// `body` is an optional JSON:API request document.
    ///
    /// Returns the parsed response body (`Value::Null` for empty 2xx bodies, e.g.
    /// `204 No Content` from a `DELETE`).
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<Value>,
    ) -> Result<Value, AscError> {
        let url = self.resolve_url(path);
        let idempotent = retry::is_idempotent(&method);

        let response = self
            .send_with_retry(idempotent, &url, || async {
                let token = self.auth.token().await?;
                let mut req = self.http.request(method.clone(), &url).bearer_auth(token);
                if !query.is_empty() {
                    req = req.query(query);
                }
                if let Some(b) = &body {
                    req = req.json(b);
                }
                Ok(req)
            })
            .await?;

        let status = response.status();
        let text = response.text().await?;

        if status.is_success() {
            if text.trim().is_empty() {
                Ok(Value::Null)
            } else {
                serde_json::from_str(&text).map_err(|e| AscError::Parse(e.to_string()))
            }
        } else {
            Err(api_error(status.as_u16(), &text))
        }
    }

    /// Convenience: authenticated `GET`.
    pub async fn get(&self, path: &str, query: &[(String, String)]) -> Result<Value, AscError> {
        self.request(Method::GET, path, query, None).await
    }

    /// Convenience: authenticated `POST` with a JSON:API body.
    pub async fn post(&self, path: &str, body: Value) -> Result<Value, AscError> {
        self.request(Method::POST, path, &[], Some(body)).await
    }

    /// Convenience: authenticated `PATCH` with a JSON:API body.
    pub async fn patch(&self, path: &str, body: Value) -> Result<Value, AscError> {
        self.request(Method::PATCH, path, &[], Some(body)).await
    }

    /// Convenience: authenticated `DELETE`.
    pub async fn delete(&self, path: &str) -> Result<Value, AscError> {
        self.request(Method::DELETE, path, &[], None).await
    }

    /// `GET` a collection, following `links.next` up to `max_pages` times.
    ///
    /// Walking pages here instead of returning a cursor to the agent turns an
    /// N-turn conversation into one tool call. The merged document keeps the
    /// last page's `links.next`, so a caller can always pick up where it stopped.
    pub async fn get_paged(
        &self,
        path: &str,
        query: &[(String, String)],
        max_pages: u32,
    ) -> Result<Value, AscError> {
        let mut merged = self.get(path, query).await?;
        let budget = max_pages.clamp(1, MAX_PAGES_LIMIT);
        let mut pages = 1;

        while pages < budget {
            let Some(next) = next_link(&merged) else {
                break;
            };
            let page = self.get(&next, &[]).await?;
            merge_page(&mut merged, page);
            pages += 1;
        }

        let has_more = next_link(&merged).is_some();
        if let Some(obj) = merged.as_object_mut() {
            let meta = obj.entry("meta").or_insert_with(|| json!({}));
            if let Some(meta) = meta.as_object_mut() {
                meta.insert("pagesFetched".into(), json!(pages));
                meta.insert("hasMore".into(), json!(has_more));
            }
        }
        Ok(merged)
    }

    /// `GET` an absolute URL **without** the bearer token, returning raw bytes.
    ///
    /// Analytics report segments live at pre-signed URLs that reject requests
    /// carrying an unexpected `Authorization` header. Uses the bulk-transfer
    /// timeout, since report files are far larger than an API response.
    pub async fn download_unauthenticated(&self, url: &str) -> Result<Vec<u8>, AscError> {
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err(AscError::InvalidRequest(format!(
                "expected an absolute http(s) URL to download, got '{url}'"
            )));
        }
        let response = self
            .send_with_retry(true, url, || async {
                Ok(self.with_transfer_timeout(self.http.get(url)))
            })
            .await?;

        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(AscError::Api {
                status: status.as_u16(),
                errors: Vec::new(),
                raw: Some(String::from_utf8_lossy(&bytes).into_owned()),
            });
        }
        Ok(bytes.to_vec())
    }

    /// Apply the bulk-transfer timeout to a request that moves a whole file.
    pub(crate) fn with_transfer_timeout(&self, req: RequestBuilder) -> RequestBuilder {
        match self.config.http.transfer_timeout {
            Some(t) => req.timeout(t),
            None => req,
        }
    }

    /// Send a request, rebuilding it for each attempt, and retry the failures
    /// [`crate::retry`] considers safe for this method.
    ///
    /// `build` is a closure rather than a prepared request because a
    /// `RequestBuilder` is consumed by `send()` and a body may not be cloneable.
    pub(crate) async fn send_with_retry<F, Fut>(
        &self,
        idempotent: bool,
        url: &str,
        build: F,
    ) -> Result<Response, AscError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<RequestBuilder, AscError>>,
    {
        let max_retries = self.config.http.max_retries;
        let mut attempt = 0u32;

        loop {
            match build().await?.send().await {
                Ok(response) => {
                    let status = response.status().as_u16();
                    if attempt >= max_retries || !retry::should_retry_status(status, idempotent) {
                        return Ok(response);
                    }
                    attempt += 1;
                    let after = retry::parse_retry_after(
                        response
                            .headers()
                            .get(RETRY_AFTER)
                            .and_then(|v| v.to_str().ok()),
                    );
                    let delay = retry::retry_delay(&self.config.http, attempt, after);
                    tracing::warn!(
                        url,
                        status,
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        "App Store Connect returned a retryable status; retrying"
                    );
                    sleep(delay).await;
                }
                Err(err) => {
                    if attempt >= max_retries || !retry::should_retry_transport(&err, idempotent) {
                        return Err(err.into());
                    }
                    attempt += 1;
                    let delay = retry::retry_delay(&self.config.http, attempt, None);
                    tracing::warn!(
                        url,
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        error = %err,
                        "transport failure; retrying"
                    );
                    sleep(delay).await;
                }
            }
        }
    }

    /// Join a relative path onto the configured base URL, or pass an absolute URL through.
    fn resolve_url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else {
            format!(
                "{}/{}",
                self.config.base_url.trim_end_matches('/'),
                path.trim_start_matches('/')
            )
        }
    }
}

async fn sleep(delay: Duration) {
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
}

/// The `links.next` URL of a collection response, if there is another page.
fn next_link(doc: &Value) -> Option<String> {
    doc.get("links")?
        .get("next")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Fold one more page into an accumulated collection document.
///
/// `data` concatenates, `included` concatenates without repeating a resource
/// already present, and `links` is replaced so `next` always points at the page
/// after the last one fetched.
fn merge_page(merged: &mut Value, page: Value) {
    let Value::Object(mut page) = page else {
        return;
    };
    let Some(target) = merged.as_object_mut() else {
        return;
    };

    if let (Some(Value::Array(into)), Some(Value::Array(from))) =
        (target.get_mut("data"), page.get_mut("data"))
    {
        into.append(from);
    }

    if let Some(Value::Array(from)) = page.get_mut("included") {
        let mut seen: HashSet<(String, String)> = target
            .get("included")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(identity).collect())
            .unwrap_or_default();
        let into = target
            .entry("included")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(into) = into {
            for item in from.drain(..) {
                if let Some(key) = identity(&item) {
                    if !seen.insert(key) {
                        continue;
                    }
                }
                into.push(item);
            }
        }
    }

    match page.remove("links") {
        Some(links) => {
            target.insert("links".into(), links);
        }
        None => {
            target.remove("links");
        }
    }
}

/// The `(type, id)` pair identifying a JSON:API resource.
fn identity(item: &Value) -> Option<(String, String)> {
    Some((
        item.get("type")?.as_str()?.to_string(),
        item.get("id")?.as_str()?.to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(ids: &[&str], next: Option<&str>) -> Value {
        let mut doc = json!({
            "data": ids.iter().map(|id| json!({ "type": "apps", "id": id })).collect::<Vec<_>>(),
            "links": { "self": "https://api.example.com/v1/apps" },
        });
        if let Some(next) = next {
            doc["links"]["next"] = json!(next);
        }
        doc
    }

    #[test]
    fn next_link_is_read_only_when_present_and_non_empty() {
        assert_eq!(
            next_link(&page(&["1"], Some("https://x/page2"))).as_deref(),
            Some("https://x/page2")
        );
        assert_eq!(next_link(&page(&["1"], None)), None);
        assert_eq!(next_link(&json!({ "links": { "next": "" } })), None);
        assert_eq!(next_link(&json!({ "data": [] })), None);
    }

    #[test]
    fn merge_page_concatenates_data_and_advances_links() {
        let mut merged = page(&["1", "2"], Some("https://x/page2"));
        merge_page(&mut merged, page(&["3"], Some("https://x/page3")));
        let ids: Vec<&str> = merged["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["1", "2", "3"]);
        assert_eq!(merged["links"]["next"], "https://x/page3");
    }

    #[test]
    fn merge_page_clears_next_on_the_final_page() {
        let mut merged = page(&["1"], Some("https://x/page2"));
        merge_page(&mut merged, page(&["2"], None));
        assert!(next_link(&merged).is_none(), "stale next link kept");
    }

    #[test]
    fn merge_page_does_not_repeat_included_resources() {
        let mut merged = json!({
            "data": [], "included": [{ "type": "builds", "id": "b1" }]
        });
        merge_page(
            &mut merged,
            json!({
                "data": [],
                "included": [{ "type": "builds", "id": "b1" }, { "type": "builds", "id": "b2" }]
            }),
        );
        let included = merged["included"].as_array().unwrap();
        assert_eq!(
            included.len(),
            2,
            "duplicate included resource: {included:?}"
        );
    }

    #[test]
    fn resolve_url_handles_relative_and_absolute_paths() {
        let client = AscClient::new(
            Config::from_parts(None, None, None).with_base_url("https://api.example.com/"),
        );
        assert_eq!(
            client.resolve_url("/v1/apps"),
            "https://api.example.com/v1/apps"
        );
        assert_eq!(
            client.resolve_url("v2/inAppPurchases/1"),
            "https://api.example.com/v2/inAppPurchases/1"
        );
        assert_eq!(
            client.resolve_url("https://other.example.com/v1/apps?cursor=x"),
            "https://other.example.com/v1/apps?cursor=x"
        );
    }

    // ---- Against a mock API ------------------------------------------------
    //
    // These cover what the pure unit tests above cannot: that a request is
    // actually replayed (or actually isn't), that the bearer token goes on
    // (or stays off), and that Apple's error envelope survives the round trip.

    mod http {
        use super::*;
        use crate::config::HttpConfig;
        use crate::testing::{fast_retries, test_client, test_config};
        use std::time::Duration;
        use wiremock::matchers::{header, header_exists, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        #[tokio::test]
        async fn a_successful_call_carries_the_bearer_token() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/apps"))
                .and(header_exists("authorization"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
                .expect(1)
                .mount(&server)
                .await;

            let value = test_client(&server.uri())
                .get("/v1/apps", &[])
                .await
                .unwrap();
            assert_eq!(value["data"], json!([]));
        }

        #[tokio::test]
        async fn query_parameters_are_sent() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/apps"))
                .and(query_param("filter[bundleId]", "com.example.app"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
                .expect(1)
                .mount(&server)
                .await;

            test_client(&server.uri())
                .get(
                    "/v1/apps",
                    &[("filter[bundleId]".into(), "com.example.app".into())],
                )
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn an_empty_204_body_becomes_null_rather_than_a_parse_error() {
            let server = MockServer::start().await;
            Mock::given(method("DELETE"))
                .respond_with(ResponseTemplate::new(204))
                .mount(&server)
                .await;

            let value = test_client(&server.uri())
                .delete("/v1/inAppPurchases/1")
                .await
                .unwrap();
            assert_eq!(value, Value::Null);
        }

        #[tokio::test]
        async fn apples_error_envelope_reaches_the_agent_intact() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(409).set_body_json(json!({
                    "errors": [{
                        "status": "409", "code": "ENTITY_ERROR.ATTRIBUTE.INVALID.DUPLICATE",
                        "title": "An attribute value is not acceptable",
                        "detail": "The product ID 'com.example.pro' is already in use",
                        "source": { "pointer": "/data/attributes/productId" }
                    }]
                })))
                .mount(&server)
                .await;

            let err = test_client(&server.uri())
                .post("/v2/inAppPurchases", json!({}))
                .await
                .unwrap_err();

            let AscError::Api { status, errors, .. } = err else {
                panic!("expected a parsed API error, got: {err:?}");
            };
            assert_eq!(status, 409);
            assert_eq!(errors.len(), 1);
            assert_eq!(
                errors[0].detail.as_deref(),
                Some("The product ID 'com.example.pro' is already in use")
            );
        }

        #[tokio::test]
        async fn an_unparseable_error_body_is_still_reported_verbatim() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(
                    ResponseTemplate::new(502).set_body_string("<html>bad gateway</html>"),
                )
                .mount(&server)
                .await;

            let config = test_config(&server.uri()).with_http(fast_retries(0));
            let err = AscClient::new(config)
                .get("/v1/apps", &[])
                .await
                .unwrap_err();
            let AscError::Api { status, raw, .. } = err else {
                panic!("expected an API error, got: {err:?}");
            };
            assert_eq!(status, 502);
            assert!(raw.unwrap().contains("bad gateway"));
        }

        #[tokio::test]
        async fn a_rate_limited_request_is_retried_until_it_succeeds() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
                .up_to_n_times(2)
                .expect(2)
                .with_priority(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
                .expect(1)
                .with_priority(2)
                .mount(&server)
                .await;

            let value = test_client(&server.uri())
                .get("/v1/apps", &[])
                .await
                .unwrap();
            assert_eq!(value["data"], json!([]));
        }

        #[tokio::test]
        async fn a_rate_limited_post_is_retried_because_apple_never_applied_it() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
                .up_to_n_times(1)
                .expect(1)
                .with_priority(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(201).set_body_json(json!({ "data": { "id": "new" } })),
                )
                .expect(1)
                .with_priority(2)
                .mount(&server)
                .await;

            let value = test_client(&server.uri())
                .post("/v1/betaGroups", json!({}))
                .await
                .unwrap();
            assert_eq!(value["data"]["id"], "new");
        }

        #[tokio::test]
        async fn a_post_is_never_replayed_after_a_server_error() {
            // Replaying could create a second resource, and Apple permanently
            // reserves identifiers like a product ID.
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(500))
                .expect(1)
                .mount(&server)
                .await;

            let err = test_client(&server.uri())
                .post("/v2/inAppPurchases", json!({}))
                .await
                .unwrap_err();
            assert!(matches!(err, AscError::Api { status: 500, .. }));
            // `expect(1)` is verified when the server drops.
        }

        #[tokio::test]
        async fn a_get_is_replayed_after_a_server_error_up_to_the_budget() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(503))
                .expect(3) // the first attempt plus two retries
                .mount(&server)
                .await;

            let config = test_config(&server.uri()).with_http(fast_retries(2));
            let err = AscClient::new(config)
                .get("/v1/apps", &[])
                .await
                .unwrap_err();
            assert!(matches!(err, AscError::Api { status: 503, .. }));
        }

        #[tokio::test]
        async fn a_client_error_is_not_retried() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "errors": [] })))
                .expect(1)
                .mount(&server)
                .await;

            let err = test_client(&server.uri())
                .get("/v1/apps/nope", &[])
                .await
                .unwrap_err();
            assert!(matches!(err, AscError::Api { status: 404, .. }));
        }

        #[tokio::test]
        async fn a_stalled_response_times_out_instead_of_hanging_forever() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
                .mount(&server)
                .await;

            let config = test_config(&server.uri()).with_http(HttpConfig {
                request_timeout: Some(Duration::from_millis(150)),
                ..fast_retries(0)
            });
            let started = std::time::Instant::now();
            let err = AscClient::new(config)
                .get("/v1/apps", &[])
                .await
                .unwrap_err();

            assert!(matches!(err, AscError::Http(_)), "{err:?}");
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "waited {:?}",
                started.elapsed()
            );
        }

        #[tokio::test]
        async fn paging_follows_next_and_returns_one_merged_document() {
            let server = MockServer::start().await;
            let next = format!("{}/v1/apps?cursor=PAGE2", server.uri());
            Mock::given(method("GET"))
                .and(path("/v1/apps"))
                .and(query_param("cursor", "PAGE2"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "data": [{ "type": "apps", "id": "3" }],
                    "links": {}
                })))
                .with_priority(1)
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/v1/apps"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "data": [{ "type": "apps", "id": "1" }, { "type": "apps", "id": "2" }],
                    "links": { "next": next }
                })))
                .with_priority(2)
                .expect(1)
                .mount(&server)
                .await;

            let merged = test_client(&server.uri())
                .get_paged("/v1/apps", &[], 5)
                .await
                .unwrap();
            assert_eq!(merged["data"].as_array().unwrap().len(), 3);
            assert_eq!(merged["meta"]["pagesFetched"], 2);
            assert_eq!(merged["meta"]["hasMore"], false);
        }

        #[tokio::test]
        async fn paging_stops_at_max_pages_and_says_there_is_more() {
            let server = MockServer::start().await;
            let next = format!("{}/v1/apps?cursor=MORE", server.uri());
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "data": [{ "type": "apps", "id": "1" }],
                    "links": { "next": next }
                })))
                .expect(1)
                .mount(&server)
                .await;

            let merged = test_client(&server.uri())
                .get_paged("/v1/apps", &[], 1)
                .await
                .unwrap();
            assert_eq!(merged["meta"]["pagesFetched"], 1);
            assert_eq!(merged["meta"]["hasMore"], true, "the caller can resume");
        }

        #[tokio::test]
        async fn paging_is_bounded_even_if_the_api_never_stops_offering_pages() {
            let server = MockServer::start().await;
            let next = format!("{}/v1/apps?cursor=FOREVER", server.uri());
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "data": [{ "type": "apps", "id": "1" }],
                    "links": { "next": next }
                })))
                .expect(MAX_PAGES_LIMIT as u64)
                .mount(&server)
                .await;

            let merged = test_client(&server.uri())
                .get_paged("/v1/apps", &[], u32::MAX)
                .await
                .unwrap();
            assert_eq!(merged["meta"]["pagesFetched"], MAX_PAGES_LIMIT);
        }

        #[tokio::test]
        async fn a_presigned_download_carries_no_authorization_header() {
            // Apple's pre-signed URLs reject requests with an unexpected
            // Authorization header, so this must not inherit the bearer token.
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/segment.csv.gz"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(b"col\n1\n".to_vec()))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(header_exists("authorization"))
                .respond_with(ResponseTemplate::new(403))
                .with_priority(1)
                .expect(0)
                .mount(&server)
                .await;

            let bytes = test_client(&server.uri())
                .download_unauthenticated(&format!("{}/segment.csv.gz", server.uri()))
                .await
                .unwrap();
            assert_eq!(bytes, b"col\n1\n".to_vec());
        }

        #[tokio::test]
        async fn a_failed_download_reports_the_status() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(header("x-none", "x"))
                .respond_with(ResponseTemplate::new(200))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(403).set_body_string("expired"))
                .mount(&server)
                .await;

            let config = test_config(&server.uri()).with_http(fast_retries(0));
            let err = AscClient::new(config)
                .download_unauthenticated(&format!("{}/segment.csv.gz", server.uri()))
                .await
                .unwrap_err();
            assert!(matches!(err, AscError::Api { status: 403, .. }), "{err:?}");
        }

        #[tokio::test]
        async fn a_relative_download_url_is_rejected_before_any_request() {
            let err = test_client("http://127.0.0.1:1")
                .download_unauthenticated("/v1/segments/1")
                .await
                .unwrap_err();
            assert!(matches!(err, AscError::InvalidRequest(_)), "{err:?}");
        }
    }
}
