//! App Store Connect webhooks: Apple POSTs to your URL when a version's state
//! changes, a build finishes processing, a TestFlight tester sends feedback,
//! and similar events (API 4.0+).

use rmcp::{
    handler::server::wrapper::Parameters, model::*, schemars, tool, tool_router,
    ErrorData as McpError,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{push_opt, AppStoreServer};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListWebhooksArgs {
    /// The app's App Store Connect ID.
    pub app_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateWebhookArgs {
    /// The app's App Store Connect ID.
    pub app_id: String,
    /// A name for the webhook.
    pub name: String,
    /// The HTTPS URL Apple posts events to.
    pub url: String,
    /// Shared secret Apple uses to sign each delivery (verify the
    /// `X-Apple-Signature` header with it).
    pub secret: String,
    /// Events to send, e.g. ["APP_STORE_VERSION_APP_VERSION_STATE_UPDATED",
    /// "BUILD_UPLOAD_STATE_UPDATED", "BETA_FEEDBACK_SCREENSHOT_SUBMISSION_CREATED",
    /// "BETA_FEEDBACK_CRASH_SUBMISSION_CREATED",
    /// "BUILD_BETA_DETAIL_EXTERNAL_BUILD_STATE_UPDATED"]. api_describe on
    /// POST /v1/webhooks lists them all.
    pub event_types: Vec<String>,
    /// Start enabled (default true).
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateWebhookArgs {
    /// The webhook ID.
    pub webhook_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
    /// Replaces the whole list of events.
    #[serde(default)]
    pub event_types: Option<Vec<String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WebhookIdArgs {
    /// The webhook ID.
    pub webhook_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListDeliveriesArgs {
    /// The webhook ID.
    pub webhook_id: String,
    /// Comma-separated: SUCCEEDED, FAILED, PENDING.
    #[serde(default)]
    pub delivery_state: Option<String>,
    /// Only deliveries created at or after this ISO-8601 time.
    #[serde(default)]
    pub since: Option<String>,
    /// Page size (maximum 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RedeliverArgs {
    /// The ID of the delivery to send again (from list_webhook_deliveries).
    pub delivery_id: String,
}

#[tool_router(router = webhooks_router, vis = "pub(crate)")]
impl AppStoreServer {
    #[tool(description = "List an app's App Store Connect webhooks.")]
    async fn list_webhooks(
        &self,
        Parameters(args): Parameters<ListWebhooksArgs>,
    ) -> Result<CallToolResult, McpError> {
        let value = self
            .client
            .get(&format!("/v1/apps/{}/webhooks", args.app_id), &[])
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(value)
    }

    #[tool(
        description = "Create a webhook so Apple posts App Store Connect events (version state \
changes, build processing, TestFlight feedback, background asset releases) to your HTTPS URL, \
signed with your secret. Send a test with create_webhook_ping."
    )]
    async fn create_webhook(
        &self,
        Parameters(args): Parameters<CreateWebhookArgs>,
    ) -> Result<CallToolResult, McpError> {
        let value = self
            .client
            .post("/v1/webhooks", create_webhook_body(&args))
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(value)
    }

    #[tool(
        description = "Update a webhook: name, URL, secret, event list, or enabled. Only the \
fields you pass change."
    )]
    async fn update_webhook(
        &self,
        Parameters(args): Parameters<UpdateWebhookArgs>,
    ) -> Result<CallToolResult, McpError> {
        let value = self
            .client
            .patch(
                &format!("/v1/webhooks/{}", args.webhook_id),
                update_webhook_body(&args),
            )
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(value)
    }

    #[tool(description = "Delete a webhook by ID.")]
    async fn delete_webhook(
        &self,
        Parameters(args): Parameters<WebhookIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.client
            .delete(&format!("/v1/webhooks/{}", args.webhook_id))
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(json!({ "deleted": args.webhook_id }))
    }

    #[tool(
        description = "List a webhook's deliveries with their state, response and event, \
optionally only failed ones or only those since a time."
    )]
    async fn list_webhook_deliveries(
        &self,
        Parameters(args): Parameters<ListDeliveriesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut query = vec![("include".to_string(), "event".to_string())];
        push_opt(&mut query, "filter[deliveryState]", args.delivery_state);
        push_opt(
            &mut query,
            "filter[createdDateGreaterThanOrEqualTo]",
            args.since,
        );
        push_opt(&mut query, "limit", args.limit);
        let value = self
            .client
            .get(
                &format!("/v1/webhooks/{}/deliveries", args.webhook_id),
                &query,
            )
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(value)
    }

    #[tool(description = "Send a past webhook delivery again (for one your server missed).")]
    async fn create_webhook_redelivery(
        &self,
        Parameters(args): Parameters<RedeliverArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = json!({ "data": {
            "type": "webhookDeliveries",
            "relationships": { "template": {
                "data": { "type": "webhookDeliveries", "id": args.delivery_id }
            }}
        }});
        let value = self
            .client
            .post("/v1/webhookDeliveries", body)
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(value)
    }

    #[tool(description = "Send a test ping to a webhook's URL.")]
    async fn create_webhook_ping(
        &self,
        Parameters(args): Parameters<WebhookIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = json!({ "data": {
            "type": "webhookPings",
            "relationships": { "webhook": {
                "data": { "type": "webhooks", "id": args.webhook_id }
            }}
        }});
        let value = self
            .client
            .post("/v1/webhookPings", body)
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(value)
    }
}

fn create_webhook_body(args: &CreateWebhookArgs) -> Value {
    json!({ "data": {
        "type": "webhooks",
        "attributes": {
            "name": args.name,
            "url": args.url,
            "secret": args.secret,
            "eventTypes": args.event_types,
            "enabled": args.enabled.unwrap_or(true),
        },
        "relationships": { "app": { "data": { "type": "apps", "id": args.app_id } } }
    }})
}

fn update_webhook_body(args: &UpdateWebhookArgs) -> Value {
    let mut attributes = serde_json::Map::new();
    for (k, v) in [
        ("name", args.name.as_ref().map(|v| json!(v))),
        ("url", args.url.as_ref().map(|v| json!(v))),
        ("secret", args.secret.as_ref().map(|v| json!(v))),
        ("eventTypes", args.event_types.as_ref().map(|v| json!(v))),
        ("enabled", args.enabled.map(|v| json!(v))),
    ] {
        if let Some(v) = v {
            attributes.insert(k.into(), v);
        }
    }
    json!({ "data": { "type": "webhooks", "id": args.webhook_id, "attributes": attributes } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_bodies_match_apples_schemas() {
        let create = create_webhook_body(&CreateWebhookArgs {
            app_id: "1".into(),
            name: "builds".into(),
            url: "https://example.com/hook".into(),
            secret: "s".into(),
            event_types: vec!["BUILD_UPLOAD_STATE_UPDATED".into()],
            enabled: None,
        });
        assert!(crate::spec::validate_named("WebhookCreateRequest", &create).is_empty());

        let update = update_webhook_body(&UpdateWebhookArgs {
            webhook_id: "w".into(),
            name: None,
            url: None,
            secret: None,
            event_types: None,
            enabled: Some(false),
        });
        assert_eq!(update["data"]["attributes"], json!({ "enabled": false }));
        assert!(crate::spec::validate_named("WebhookUpdateRequest", &update).is_empty());
    }
}
