//! Generic JSON:API escape-hatch tools.
//!
//! These two tools can reach *any* App Store Connect endpoint, covering
//! everything the curated domain tools don't.

use reqwest::Method;
use rmcp::{
    handler::server::wrapper::Parameters, model::*, schemars, tool, tool_router,
    ErrorData as McpError,
};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::{de_coerce_json_opt, de_coerce_map_opt, flatten_query, AppStoreServer};
use crate::error::AscError;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RequestArgs {
    /// HTTP method: GET, POST, PATCH, PUT, or DELETE.
    pub method: String,
    /// API path or full URL, e.g. "/v1/apps", "v2/inAppPurchases/{id}",
    /// or a `next` link returned by a previous list call.
    pub path: String,
    /// Optional query parameters, e.g. {"filter[bundleId]": "com.example.app", "limit": 50}.
    /// Array values are comma-joined.
    #[serde(default, deserialize_with = "de_coerce_map_opt")]
    pub query: Option<Map<String, Value>>,
    /// Optional JSON:API request body for POST/PATCH/PUT — the full document,
    /// e.g. {"data": {"type": "apps", "id": "123", "attributes": {...}}}.
    #[serde(default, deserialize_with = "de_coerce_json_opt")]
    pub body: Option<Value>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListArgs {
    /// Collection path or full URL to GET, e.g. "/v1/apps".
    pub path: String,
    /// Optional filters, e.g. {"filter[name]": "MyApp"}.
    #[serde(default, deserialize_with = "de_coerce_map_opt")]
    pub filters: Option<Map<String, Value>>,
    /// Comma-separated sort keys, e.g. "-createdDate".
    #[serde(default)]
    pub sort: Option<String>,
    /// Comma-separated related resources to include, e.g. "appStoreVersions".
    #[serde(default)]
    pub include: Option<String>,
    /// Page size (App Store Connect maximum is 200). Sparse-fieldset selections
    /// (`fields[...]`) can be passed via `filters` if needed.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Opaque pagination cursor from a previous response's `data.links.next`.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Follow `links.next` and merge up to this many pages into one result
    /// (default 1, maximum 20). Saves a round trip per page.
    #[serde(default)]
    pub max_pages: Option<u32>,
}

/// The methods App Store Connect's JSON:API surface uses.
///
/// Checked against a list rather than `Method::from_bytes`, which accepts any
/// syntactically valid token: a typo like `FETCH` would otherwise become a real
/// request and come back as an opaque transport or 405 error.
const ALLOWED_METHODS: [Method; 5] = [
    Method::GET,
    Method::POST,
    Method::PATCH,
    Method::PUT,
    Method::DELETE,
];

fn parse_method(raw: &str) -> Result<Method, AscError> {
    let normalized = raw.trim().to_ascii_uppercase();
    ALLOWED_METHODS
        .into_iter()
        .find(|m| m.as_str() == normalized)
        .ok_or_else(|| {
            AscError::InvalidRequest(format!(
                "invalid HTTP method '{raw}'; use GET, POST, PATCH, PUT, or DELETE"
            ))
        })
}

#[tool_router(router = generic_router, vis = "pub(crate)")]
impl AppStoreServer {
    /// Make a raw, authenticated request to any App Store Connect endpoint.
    #[tool(
        description = "Make a raw authenticated request to ANY App Store Connect API endpoint \
(method + path + optional query + optional JSON:API body). Use this for operations without a \
dedicated tool. Returns the parsed JSON response."
    )]
    async fn appstore_request(
        &self,
        Parameters(args): Parameters<RequestArgs>,
    ) -> Result<CallToolResult, McpError> {
        let method = parse_method(&args.method).map_err(AppStoreServer::map_err)?;

        // This tool can reach every write endpoint Apple has, so it is the one
        // that has to honour read-only mode itself rather than by being hidden.
        if method != Method::GET {
            self.ensure_writable(&format!("a {method} request"))?;
        }

        let query = args.query.as_ref().map(flatten_query).unwrap_or_default();
        let value = self
            .client
            .request(method, &args.path, &query, args.body)
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(value)
    }

    /// List a collection with filters/sort/pagination.
    #[tool(
        description = "List any App Store Connect collection with optional filters, sort, include, \
and pagination. Returns one page by default; set `max_pages` to follow `links.next` and merge \
several pages into one result, or pass a previous response's links.next back as `cursor` to resume."
    )]
    async fn appstore_list(
        &self,
        Parameters(args): Parameters<ListArgs>,
    ) -> Result<CallToolResult, McpError> {
        let max_pages = args.max_pages.unwrap_or(1);

        // A cursor is a full `next` URL, already carrying its query string.
        let (path, query) = match &args.cursor {
            Some(cursor) => (cursor.clone(), Vec::new()),
            None => {
                let mut query: Vec<(String, String)> = Vec::new();
                if let Some(filters) = &args.filters {
                    query.extend(flatten_query(filters));
                }
                if let Some(sort) = &args.sort {
                    query.push(("sort".into(), sort.clone()));
                }
                if let Some(include) = &args.include {
                    query.push(("include".into(), include.clone()));
                }
                if let Some(limit) = args.limit {
                    query.push(("limit".into(), limit.to_string()));
                }
                (args.path.clone(), query)
            }
        };

        let value = self
            .client
            .get_paged(&path, &query, max_pages)
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolsConfig;
    use crate::testing::{result_text, test_config, test_server};
    use serde_json::json;
    use wiremock::matchers::{method as http_method, path as http_path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn request_args(method: &str, path: &str) -> RequestArgs {
        RequestArgs {
            method: method.into(),
            path: path.into(),
            query: None,
            body: None,
        }
    }

    fn list_args(path: &str, max_pages: Option<u32>) -> ListArgs {
        ListArgs {
            path: path.into(),
            filters: None,
            sort: None,
            include: None,
            limit: None,
            cursor: None,
            max_pages,
        }
    }

    #[test]
    fn methods_are_checked_against_the_documented_set() {
        assert_eq!(parse_method("get").unwrap(), Method::GET);
        assert_eq!(parse_method(" PATCH ").unwrap(), Method::PATCH);
        // A typo must fail here rather than become a real request.
        for bad in ["FETCH", "TRACE", "", "GET POST"] {
            let err = parse_method(bad).unwrap_err();
            assert!(err.to_string().contains("invalid HTTP method"), "{bad}");
        }
    }

    #[tokio::test]
    async fn an_unknown_http_method_is_rejected_without_calling_apple() {
        let mock = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;

        let err = test_server(&mock.uri())
            .appstore_request(Parameters(request_args("FETCH", "/v1/apps")))
            .await
            .unwrap_err();
        assert!(
            err.message.contains("invalid HTTP method"),
            "{}",
            err.message
        );
    }

    #[tokio::test]
    async fn read_only_mode_refuses_writes_through_the_escape_hatch() {
        // Without this guard, read-only mode would be trivially bypassable:
        // `appstore_request` can reach every write endpoint Apple has.
        let mock = MockServer::start().await;
        Mock::given(http_method("POST"))
            .respond_with(ResponseTemplate::new(201))
            .expect(0)
            .mount(&mock)
            .await;

        let config = test_config(&mock.uri()).with_tools(ToolsConfig {
            read_only: true,
            groups: None,
        });
        let server = AppStoreServer::new(config);

        for verb in ["POST", "PATCH", "PUT", "DELETE"] {
            let err = server
                .appstore_request(Parameters(request_args(verb, "/v1/apps")))
                .await
                .unwrap_err();
            assert!(err.message.contains("read-only"), "{verb}: {}", err.message);
        }
    }

    #[tokio::test]
    async fn read_only_mode_still_allows_reads_through_the_escape_hatch() {
        let mock = MockServer::start().await;
        Mock::given(http_method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
            .expect(1)
            .mount(&mock)
            .await;

        let config = test_config(&mock.uri()).with_tools(ToolsConfig {
            read_only: true,
            groups: None,
        });
        let result = AppStoreServer::new(config)
            .appstore_request(Parameters(request_args("get", "/v1/apps")))
            .await
            .unwrap();
        assert!(result_text(&result).contains("\"data\""));
    }

    #[tokio::test]
    async fn listing_returns_a_single_page_by_default() {
        let mock = MockServer::start().await;
        let next = format!("{}/v1/apps?cursor=P2", mock.uri());
        Mock::given(http_method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "type": "apps", "id": "1" }],
                "links": { "next": next }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let result = test_server(&mock.uri())
            .appstore_list(Parameters(list_args("/v1/apps", None)))
            .await
            .unwrap();
        let doc: Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(doc["meta"]["pagesFetched"], 1);
        assert_eq!(doc["meta"]["hasMore"], true);
    }

    #[tokio::test]
    async fn max_pages_collapses_several_round_trips_into_one_call() {
        let mock = MockServer::start().await;
        let next = format!("{}/v1/apps?cursor=P2", mock.uri());
        Mock::given(http_method("GET"))
            .and(query_param("cursor", "P2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "type": "apps", "id": "2" }], "links": {}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(http_method("GET"))
            .and(http_path("/v1/apps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "type": "apps", "id": "1" }], "links": { "next": next }
            })))
            .with_priority(2)
            .expect(1)
            .mount(&mock)
            .await;

        let result = test_server(&mock.uri())
            .appstore_list(Parameters(list_args("/v1/apps", Some(3))))
            .await
            .unwrap();
        let doc: Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(doc["data"].as_array().unwrap().len(), 2);
        assert_eq!(doc["meta"]["pagesFetched"], 2);
        assert_eq!(doc["meta"]["hasMore"], false);
    }

    #[tokio::test]
    async fn a_cursor_is_followed_verbatim() {
        let mock = MockServer::start().await;
        Mock::given(http_method("GET"))
            .and(http_path("/v1/apps"))
            .and(query_param("cursor", "RESUME"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
            .expect(1)
            .mount(&mock)
            .await;

        let mut args = list_args("ignored", None);
        args.cursor = Some(format!("{}/v1/apps?cursor=RESUME", mock.uri()));
        test_server(&mock.uri())
            .appstore_list(Parameters(args))
            .await
            .unwrap();
    }
}
