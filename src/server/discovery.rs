//! Optional compact tool surface. The filtered domain router remains private;
//! discovery and execution use that same router so group and read-only policy
//! cannot diverge.

use rmcp::{
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::*,
    schemars, tool, tool_router, ErrorData as McpError, RoleServer,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::AppStoreServer;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchToolsArgs {
    /// Words describing the App Store Connect operation needed.
    query: String,
    /// Maximum matches to return (1–20; default 10).
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetToolDetailsArgs {
    /// Exact name returned by search_tools.
    name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CallDiscoveredToolArgs {
    /// Exact name of a discoverable tool. Inspect its schema first.
    name: String,
    /// Arguments matching that tool's inputSchema.
    #[serde(default)]
    arguments: Map<String, Value>,
}

pub fn annotate(router: &mut ToolRouter<AppStoreServer>, read_only: bool) {
    for route in router.map.values_mut() {
        let annotations = ToolAnnotations::new()
            .open_world(false)
            .read_only(true)
            .destructive(false)
            .idempotent(true);
        route.attr.annotations = Some(if route.attr.name == "call_discovered_tool" {
            // One generic execution entry point can reach destructive Apple
            // operations. Never claim it is safe for a host to auto-approve.
            ToolAnnotations::new()
                .open_world(true)
                .read_only(read_only)
                .destructive(!read_only)
                .idempotent(false)
        } else {
            annotations
        });
    }
}

#[tool_router(router = discovery_router, vis = "pub(crate)")]
impl AppStoreServer {
    /// Search available App Store Connect tools by name and description.
    #[tool(
        description = "Search available App Store Connect operations. Returns short names and descriptions; inspect a match with get_tool_details before calling it. Only tools allowed by ASC_TOOLS and ASC_READ_ONLY appear."
    )]
    async fn search_tools(
        &self,
        Parameters(args): Parameters<SearchToolsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let query = args.query.trim().to_ascii_lowercase();
        if query.is_empty() || query.len() > 200 {
            return Err(McpError::invalid_params(
                "query must contain 1–200 characters",
                None,
            ));
        }
        let terms: Vec<&str> = query
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|term| !term.is_empty())
            .collect();
        let router = self.discovery_router.as_ref().expect("discovery mode");
        let mut matches: Vec<(usize, &Tool)> = router
            .map
            .values()
            .filter_map(|route| {
                let tool = &route.attr;
                let name = tool.name.to_ascii_lowercase();
                let description = tool
                    .description
                    .as_deref()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let mut score = 0;
                for term in &terms {
                    if name == *term {
                        score += 12;
                    } else if name.starts_with(term) {
                        score += 8;
                    } else if name.contains(term) {
                        score += 5;
                    }
                    if description.contains(term) {
                        score += 1;
                    }
                }
                (score > 0).then_some((score, tool))
            })
            .collect();
        matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        let total = matches.len();
        let limit = args.limit.unwrap_or(10).clamp(1, 20);
        let found: Vec<Value> = matches
            .into_iter()
            .take(limit)
            .map(|(_, tool)| {
                json!({
                    "name": tool.name,
                    "description": tool.description.as_deref().unwrap_or("")
                        .lines().next().unwrap_or("").chars().take(180).collect::<String>()
                })
            })
            .collect();
        self.ok_json(json!({ "matches": found, "totalMatches": total }))
    }

    /// Return full definition of one discoverable operation.
    #[tool(
        description = "Inspect one App Store Connect operation by exact name. Returns its full input schema, description, and safety annotations. Use before call_discovered_tool."
    )]
    async fn get_tool_details(
        &self,
        Parameters(args): Parameters<GetToolDetailsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tool = self
            .discovery_router
            .as_ref()
            .expect("discovery mode")
            .get(&args.name)
            .ok_or_else(|| McpError::invalid_params("tool not available", None))?;
        self.ok_json(
            serde_json::to_value(tool)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?,
        )
    }

    /// Execute an inspected operation using its original handler and policy.
    #[tool(
        description = "Call an App Store Connect operation found by search_tools and inspected with get_tool_details. Pass its exact name and arguments matching its input schema. This can modify or delete account data; check the inspected tool's safety annotations and obtain approval before writes."
    )]
    async fn call_discovered_tool(
        &self,
        Parameters(args): Parameters<CallDiscoveredToolArgs>,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let router = self.discovery_router.as_ref().expect("discovery mode");
        if router.get(&args.name).is_none() {
            return Err(McpError::invalid_params("tool not available", None));
        }
        let request = CallToolRequestParams::new(args.name).with_arguments(args.arguments);
        router
            .call(ToolCallContext::new(self, request, context))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ToolsConfig};

    fn server(groups: Option<&str>, read_only: bool) -> AppStoreServer {
        AppStoreServer::new(
            Config::from_parts(None, None, None).with_tools(ToolsConfig {
                groups: groups.map(str::to_string),
                read_only,
                discovery: true,
            }),
        )
    }

    fn result_json(result: CallToolResult) -> Value {
        serde_json::from_str(result.content[0].as_text().unwrap().text.as_str()).unwrap()
    }

    #[tokio::test]
    async fn search_and_inspect_use_only_filtered_tools() {
        let server = server(Some("users"), true);
        let result = server
            .search_tools(Parameters(SearchToolsArgs {
                query: "users".into(),
                limit: None,
            }))
            .await
            .unwrap();
        let matches = result_json(result);
        assert_eq!(matches["totalMatches"], 1);
        assert_eq!(matches["matches"][0]["name"], "list_users");

        let details = server
            .get_tool_details(Parameters(GetToolDetailsArgs {
                name: "list_users".into(),
            }))
            .await
            .unwrap();
        let details = result_json(details);
        assert_eq!(details["name"], "list_users");
        assert_eq!(details["annotations"]["readOnlyHint"], true);
        assert!(details["inputSchema"].is_object());

        let denied = server
            .get_tool_details(Parameters(GetToolDetailsArgs {
                name: "remove_user".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(denied.message, "tool not available");
    }

    #[tokio::test]
    async fn empty_or_oversized_search_is_rejected() {
        let server = server(None, false);
        for query in [" ".to_string(), "a".repeat(201)] {
            assert!(server
                .search_tools(Parameters(SearchToolsArgs { query, limit: None }))
                .await
                .is_err());
        }
    }
}
