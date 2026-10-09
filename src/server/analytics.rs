//! Analytics Reports tools (App Store Connect Analytics Reports API).
//!
//! Schemas verified against Apple's generated OpenAPI models in the AvdLee
//! Swift SDK (AnalyticsReportRequestCreateRequest).
//!
//! JSON key mapping from the SDK's `forKey:` decode lines:
//!   - data.type: "analyticsReportRequests"
//!   - attributes: accessType (required, "ONGOING" | "ONE_TIME_SNAPSHOT")
//!   - relationships: app → data { type: "apps", id }

use rmcp::{
    handler::server::wrapper::Parameters, model::*, schemars, tool, tool_router,
    ErrorData as McpError,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{push_opt, AppStoreServer};
use crate::error::AscError;
use crate::report;

/// The access type for an analytics report request.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AccessType {
    Ongoing,
    OneTimeSnapshot,
}

impl AccessType {
    fn as_api(self) -> &'static str {
        match self {
            AccessType::Ongoing => "ONGOING",
            AccessType::OneTimeSnapshot => "ONE_TIME_SNAPSHOT",
        }
    }
}

/// The granularity of an analytics report instance.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Granularity {
    Daily,
    Weekly,
    Monthly,
}

impl Granularity {
    fn as_api(self) -> &'static str {
        match self {
            Granularity::Daily => "DAILY",
            Granularity::Weekly => "WEEKLY",
            Granularity::Monthly => "MONTHLY",
        }
    }
}

// ---- Arg structs ------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RequestAnalyticsReportArgs {
    /// The app's App Store Connect ID.
    pub app_id: String,
    /// The access type for the report request: ONGOING or ONE_TIME_SNAPSHOT.
    pub access_type: AccessType,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListAnalyticsReportsArgs {
    /// The analytics report request ID.
    pub report_request_id: String,
    /// Filter by report category (e.g. "APP_USAGE", "COMMERCE", "ENGAGEMENT",
    /// "FRAMEWORK_USAGE", "PERFORMANCE").
    #[serde(default)]
    pub category: Option<String>,
    /// Page size (max 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListAnalyticsReportInstancesArgs {
    /// The analytics report ID.
    pub report_id: String,
    /// Filter by granularity: DAILY, WEEKLY, or MONTHLY.
    #[serde(default)]
    pub granularity: Option<Granularity>,
    /// Filter by processing date in YYYY-MM-DD format.
    #[serde(default)]
    pub processing_date: Option<String>,
    /// Page size (max 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListAnalyticsReportSegmentsArgs {
    /// The analytics report instance ID.
    pub instance_id: String,
    /// Page size (max 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DownloadAnalyticsSegmentArgs {
    /// The segment's presigned download URL, taken from a segment's
    /// `attributes.url` in list_analytics_report_segments. These URLs expire,
    /// so fetch a fresh one if the download is rejected.
    pub url: String,
    /// Maximum data rows to return (default 100, hard maximum 5000). The full
    /// row count is reported regardless of how many rows come back.
    #[serde(default)]
    pub max_rows: Option<usize>,
}

// ---- Tool impl block --------------------------------------------------------

#[tool_router(router = analytics_router, vis = "pub(crate)")]
impl AppStoreServer {
    /// Request an analytics report.
    #[tool(
        description = "Create an analytics report request for an app. Use access_type ONGOING for \
a recurring report or ONE_TIME_SNAPSHOT for a one-time snapshot. Returns the report request \
resource including its ID, which you then pass to list_analytics_reports."
    )]
    async fn request_analytics_report(
        &self,
        Parameters(args): Parameters<RequestAnalyticsReportArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = analytics_report_request_body(&args.app_id, args.access_type);
        let value = self
            .client
            .post("/v1/analyticsReportRequests", body)
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(value)
    }

    /// List analytics reports for a report request.
    #[tool(
        description = "List the analytics reports available for a report request. Optionally \
filter by category (e.g. APP_USAGE, COMMERCE, ENGAGEMENT, FRAMEWORK_USAGE, PERFORMANCE). \
Returns report resources whose IDs you pass to list_analytics_report_instances."
    )]
    async fn list_analytics_reports(
        &self,
        Parameters(args): Parameters<ListAnalyticsReportsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut query = Vec::new();
        push_opt(&mut query, "filter[category]", args.category);
        push_opt(&mut query, "limit", args.limit);
        let value = self
            .client
            .get(
                &format!(
                    "/v1/analyticsReportRequests/{}/reports",
                    args.report_request_id
                ),
                &query,
            )
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(value)
    }

    /// List instances of an analytics report.
    #[tool(
        description = "List instances of an analytics report, optionally filtered by granularity \
(DAILY, WEEKLY, or MONTHLY) and/or processing date (YYYY-MM-DD). Returns instance resources \
whose IDs you pass to list_analytics_report_segments."
    )]
    async fn list_analytics_report_instances(
        &self,
        Parameters(args): Parameters<ListAnalyticsReportInstancesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut query = Vec::new();
        if let Some(g) = args.granularity {
            query.push(("filter[granularity]".into(), g.as_api().to_string()));
        }
        push_opt(&mut query, "filter[processingDate]", args.processing_date);
        push_opt(&mut query, "limit", args.limit);
        let value = self
            .client
            .get(
                &format!("/v1/analyticsReports/{}/instances", args.report_id),
                &query,
            )
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(value)
    }

    /// List segments of an analytics report instance.
    #[tool(
        description = "List the downloadable segments for an analytics report instance. Each \
segment's attributes include a presigned `url` pointing to a gzipped delimited file, plus \
`sizeInBytes` and `checksum`. Pass that `url` to download_analytics_segment to read the rows."
    )]
    async fn list_analytics_report_segments(
        &self,
        Parameters(args): Parameters<ListAnalyticsReportSegmentsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut query = Vec::new();
        push_opt(&mut query, "limit", args.limit);
        let value = self
            .client
            .get(
                &format!("/v1/analyticsReportInstances/{}/segments", args.instance_id),
                &query,
            )
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(value)
    }

    /// Download and parse an analytics report segment.
    #[tool(
        description = "Download an analytics report segment and return its contents as JSON rows. \
Takes the presigned `url` from a segment returned by list_analytics_report_segments, decompresses \
the gzipped file, and parses it into rows keyed by column name. Returns the column list, the total \
row count, and up to `max_rows` rows (default 100). This is how you read the actual analytics \
numbers — the other analytics tools only navigate to the segment."
    )]
    async fn download_analytics_segment(
        &self,
        Parameters(args): Parameters<DownloadAnalyticsSegmentArgs>,
    ) -> Result<CallToolResult, McpError> {
        let bytes = self
            .client
            .download_unauthenticated(&args.url)
            .await
            .map_err(AppStoreServer::map_err)?;
        let max_rows = args.max_rows.unwrap_or(report::DEFAULT_MAX_ROWS);

        // Gunzipping and parsing a report is synchronous CPU work — tens of
        // milliseconds for a few hundred thousand rows, and linear beyond that.
        // Running it inline would stall an async worker for that whole time.
        let parsed = tokio::task::spawn_blocking(move || report::parse_segment(&bytes, max_rows))
            .await
            .map_err(|e| {
                AppStoreServer::map_err(AscError::Parse(format!(
                    "the report parsing task did not finish: {e}"
                )))
            })?
            .map_err(AppStoreServer::map_err)?;

        self.ok_json(parsed.to_json())
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SalesReportArgs {
    /// SALES (units and proceeds), SUBSCRIPTION, SUBSCRIPTION_EVENT, SUBSCRIBER,
    /// SUBSCRIPTION_OFFER_CODE_REDEMPTION, INSTALLS, FIRST_ANNUAL, PRE_ORDER,
    /// NEWSSTAND or WIN_BACK_ELIGIBILITY.
    pub report_type: String,
    /// SUMMARY (default), DETAILED, SUMMARY_INSTALL_TYPE, SUMMARY_TERRITORY or
    /// SUMMARY_CHANNEL. Each report type accepts only some.
    #[serde(default)]
    pub report_sub_type: Option<String>,
    /// DAILY (default), WEEKLY, MONTHLY or YEARLY.
    #[serde(default)]
    pub frequency: Option<String>,
    /// The period: YYYY-MM-DD for DAILY/WEEKLY, YYYY-MM for MONTHLY, YYYY for
    /// YEARLY. Omit for the latest available.
    #[serde(default)]
    pub report_date: Option<String>,
    /// Report format version, e.g. "1_0" (SALES) or "1_4" (SUBSCRIPTION). Omit
    /// for Apple's default.
    #[serde(default)]
    pub version: Option<String>,
    /// Your vendor number (Payments and Financial Reports). Defaults to the
    /// ASC_VENDOR_NUMBER environment variable.
    #[serde(default)]
    pub vendor_number: Option<String>,
    /// Maximum rows to return (default 100, maximum 5000).
    #[serde(default)]
    pub max_rows: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FinanceReportArgs {
    /// FINANCIAL or FINANCE_DETAIL.
    pub report_type: String,
    /// Fiscal month, YYYY-MM.
    pub report_date: String,
    /// Region code, e.g. "US", "CA", "EU", "ZZ" (all regions, FINANCE_DETAIL only).
    pub region_code: String,
    /// Your vendor number. Defaults to the ASC_VENDOR_NUMBER environment variable.
    #[serde(default)]
    pub vendor_number: Option<String>,
    /// Maximum rows to return (default 100, maximum 5000).
    #[serde(default)]
    pub max_rows: Option<usize>,
}

fn vendor_number(arg: Option<String>) -> Result<String, AscError> {
    arg.filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("ASC_VENDOR_NUMBER")
                .ok()
                .filter(|v| !v.is_empty())
        })
        .ok_or_else(|| {
            AscError::InvalidRequest(
                "a vendor number is required: pass vendor_number or set ASC_VENDOR_NUMBER \
(App Store Connect → Payments and Financial Reports, top left)"
                    .into(),
            )
        })
}

fn sales_report_query(args: &SalesReportArgs, vendor: String) -> Vec<(String, String)> {
    let mut q = vec![
        ("filter[vendorNumber]".to_string(), vendor),
        ("filter[reportType]".to_string(), args.report_type.clone()),
        (
            "filter[reportSubType]".to_string(),
            args.report_sub_type
                .clone()
                .unwrap_or_else(|| "SUMMARY".into()),
        ),
        (
            "filter[frequency]".to_string(),
            args.frequency.clone().unwrap_or_else(|| "DAILY".into()),
        ),
    ];
    if let Some(d) = &args.report_date {
        q.push(("filter[reportDate]".into(), d.clone()));
    }
    if let Some(v) = &args.version {
        q.push(("filter[version]".into(), v.clone()));
    }
    q
}

#[tool_router(router = reports_router, vis = "pub(crate)")]
impl AppStoreServer {
    /// Download a sales/trends report as rows.
    #[tool(
        description = "Download a Sales and Trends report (units, proceeds, installs, subscription \
counts and events, offer-code redemptions) and return it as JSON rows. Apple publishes daily \
reports the next day; omit report_date for the latest. Needs your vendor number (argument or \
ASC_VENDOR_NUMBER)."
    )]
    async fn download_sales_report(
        &self,
        Parameters(args): Parameters<SalesReportArgs>,
    ) -> Result<CallToolResult, McpError> {
        let vendor = vendor_number(args.vendor_number.clone()).map_err(AppStoreServer::map_err)?;
        let query = sales_report_query(&args, vendor);
        self.report_rows("/v1/salesReports", &query, args.max_rows)
            .await
    }

    /// Download a financial report as rows.
    #[tool(
        description = "Download a monthly financial report (FINANCIAL: earnings per region; \
FINANCE_DETAIL: per-transaction proceeds for all regions with region_code ZZ) and return it as \
JSON rows. Needs your vendor number (argument or ASC_VENDOR_NUMBER)."
    )]
    async fn download_finance_report(
        &self,
        Parameters(args): Parameters<FinanceReportArgs>,
    ) -> Result<CallToolResult, McpError> {
        let vendor = vendor_number(args.vendor_number).map_err(AppStoreServer::map_err)?;
        let query = vec![
            ("filter[vendorNumber]".to_string(), vendor),
            ("filter[reportType]".to_string(), args.report_type),
            ("filter[reportDate]".to_string(), args.report_date),
            ("filter[regionCode]".to_string(), args.region_code),
        ];
        self.report_rows("/v1/financeReports", &query, args.max_rows)
            .await
    }
}

impl AppStoreServer {
    async fn report_rows(
        &self,
        path: &str,
        query: &[(String, String)],
        max_rows: Option<usize>,
    ) -> Result<CallToolResult, McpError> {
        let bytes = self
            .client
            .get_bytes(path, query)
            .await
            .map_err(AppStoreServer::map_err)?;
        let max_rows = max_rows.unwrap_or(report::DEFAULT_MAX_ROWS);
        let parsed = tokio::task::spawn_blocking(move || report::parse_segment(&bytes, max_rows))
            .await
            .map_err(|e| {
                AppStoreServer::map_err(AscError::Parse(format!(
                    "the report parsing task did not finish: {e}"
                )))
            })?
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(parsed.to_json())
    }
}

// ---- Pure JSON:API document builders (unit-tested below) --------------------

/// Build the request body for POST /v1/analyticsReportRequests.
///
/// Schema verified against AnalyticsReportRequestCreateRequest in the AvdLee
/// Swift SDK: data.type = "analyticsReportRequests", required attribute
/// `accessType`, and an `app` relationship.
fn analytics_report_request_body(app_id: &str, access_type: AccessType) -> Value {
    json!({
        "data": {
            "type": "analyticsReportRequests",
            "attributes": {
                "accessType": access_type.as_api()
            },
            "relationships": {
                "app": {
                    "data": {
                        "type": "apps",
                        "id": app_id
                    }
                }
            }
        }
    })
}

// ---- Unit tests -------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_type_ongoing_maps_to_api_string() {
        assert_eq!(AccessType::Ongoing.as_api(), "ONGOING");
    }

    #[test]
    fn access_type_one_time_snapshot_maps_to_api_string() {
        assert_eq!(AccessType::OneTimeSnapshot.as_api(), "ONE_TIME_SNAPSHOT");
    }

    #[test]
    fn analytics_report_request_body_data_type() {
        let b = analytics_report_request_body("app-123", AccessType::Ongoing);
        assert_eq!(b["data"]["type"], "analyticsReportRequests");
    }

    #[test]
    fn analytics_report_request_body_access_type_ongoing() {
        let b = analytics_report_request_body("app-123", AccessType::Ongoing);
        assert_eq!(b["data"]["attributes"]["accessType"], "ONGOING");
    }

    #[test]
    fn analytics_report_request_body_access_type_one_time_snapshot() {
        let b = analytics_report_request_body("app-456", AccessType::OneTimeSnapshot);
        assert_eq!(b["data"]["attributes"]["accessType"], "ONE_TIME_SNAPSHOT");
    }

    #[test]
    fn analytics_report_request_body_app_relationship() {
        let b = analytics_report_request_body("app-789", AccessType::Ongoing);
        let rel = &b["data"]["relationships"]["app"]["data"];
        assert_eq!(rel["type"], "apps");
        assert_eq!(rel["id"], "app-789");
    }

    #[test]
    fn granularity_daily_maps_to_api_string() {
        assert_eq!(Granularity::Daily.as_api(), "DAILY");
    }

    #[test]
    fn granularity_weekly_maps_to_api_string() {
        assert_eq!(Granularity::Weekly.as_api(), "WEEKLY");
    }

    #[test]
    fn granularity_monthly_maps_to_api_string() {
        assert_eq!(Granularity::Monthly.as_api(), "MONTHLY");
    }

    #[test]
    fn analytics_report_request_body_no_id_field() {
        let b = analytics_report_request_body("app-123", AccessType::Ongoing);
        assert!(b["data"].get("id").is_none());
    }

    // ---- Segment download, against a mock CDN --------------------------------

    mod download {
        use super::*;
        use crate::testing::{result_text, test_server};
        use std::io::Write;
        use wiremock::matchers::{method as http_method, path as http_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const TSV: &str =
            "Date\tApp Name\tUnits\n2026-07-01\tMy App\t142\n2026-07-02\tMy App\t97\n";

        fn gzip(bytes: &[u8]) -> Vec<u8> {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(bytes).unwrap();
            encoder.finish().unwrap()
        }

        async fn serve_segment(server: &MockServer, body: Vec<u8>) -> String {
            Mock::given(http_method("GET"))
                .and(http_path("/segments/1"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
                .expect(1)
                .mount(server)
                .await;
            format!("{}/segments/1", server.uri())
        }

        #[tokio::test]
        async fn a_gzipped_segment_becomes_json_rows() {
            let mock = MockServer::start().await;
            let url = serve_segment(&mock, gzip(TSV.as_bytes())).await;

            let result = test_server(&mock.uri())
                .download_analytics_segment(Parameters(DownloadAnalyticsSegmentArgs {
                    url,
                    max_rows: None,
                }))
                .await
                .unwrap();

            let doc: Value = serde_json::from_str(&result_text(&result)).unwrap();
            assert_eq!(doc["columns"], json!(["Date", "App Name", "Units"]));
            assert_eq!(doc["totalRows"], 2);
            assert_eq!(doc["truncated"], false);
            assert_eq!(doc["rows"][0]["Units"], "142");
            assert_eq!(doc["rows"][1]["Date"], "2026-07-02");
        }

        #[tokio::test]
        async fn max_rows_samples_the_file_without_hiding_its_size() {
            let mut text = String::from("Date\tUnits\n");
            for i in 0..250 {
                text.push_str(&format!("2026-07-{:02}\t{i}\n", i % 28 + 1));
            }
            let mock = MockServer::start().await;
            let url = serve_segment(&mock, gzip(text.as_bytes())).await;

            let result = test_server(&mock.uri())
                .download_analytics_segment(Parameters(DownloadAnalyticsSegmentArgs {
                    url,
                    max_rows: Some(3),
                }))
                .await
                .unwrap();

            let doc: Value = serde_json::from_str(&result_text(&result)).unwrap();
            assert_eq!(doc["rowsReturned"], 3);
            assert_eq!(doc["totalRows"], 250);
            assert_eq!(doc["truncated"], true);
        }

        #[tokio::test]
        async fn an_expired_url_reports_apples_status() {
            let mock = MockServer::start().await;
            Mock::given(http_method("GET"))
                .respond_with(ResponseTemplate::new(403).set_body_string("Request has expired"))
                .mount(&mock)
                .await;

            let err = test_server(&mock.uri())
                .download_analytics_segment(Parameters(DownloadAnalyticsSegmentArgs {
                    url: format!("{}/segments/gone", mock.uri()),
                    max_rows: None,
                }))
                .await
                .unwrap_err();
            assert!(err.message.contains("403"), "{}", err.message);
        }
    }
}

#[cfg(test)]
mod report_tool_tests {
    use super::*;
    use crate::testing::{result_text, test_server};
    use std::io::Write;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn a_sales_report_comes_back_as_rows() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"Provider\tSKU\tUnits\nAPPLE\tcom.example\t3\n")
            .unwrap();
        let body = gz.finish().unwrap();

        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/salesReports"))
            .and(query_param("filter[vendorNumber]", "85000000"))
            .and(query_param("filter[reportSubType]", "SUMMARY"))
            .and(query_param("filter[frequency]", "DAILY"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .expect(1)
            .mount(&mock)
            .await;

        let result = test_server(&mock.uri())
            .download_sales_report(Parameters(SalesReportArgs {
                report_type: "SALES".into(),
                report_sub_type: None,
                frequency: None,
                report_date: None,
                version: None,
                vendor_number: Some("85000000".into()),
                max_rows: None,
            }))
            .await
            .unwrap();
        let text = result_text(&result);
        assert!(text.contains("com.example"), "{text}");
        assert!(text.contains("Units"), "{text}");
    }
}
