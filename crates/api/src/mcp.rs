//! MCP entry point: the generic hub tools over stdio.
//!
//! Every tool is a thin `HubClient` call, so the hub's own role checks decide
//! what the token may do. Read tools observe; Write tools change managed state.
//! `Destructive` stays denied because no hub tool may mutate a host or run a
//! shell.
//!
//! The generic `hub_*` tools live here rather than in a product pack so any hub
//! MCP client gets them. A pack that still registers an older copy makes
//! startup fail loudly: `Server::register` refuses a duplicate name and the
//! error names the tool.

use std::future::Future;

use contracts::client::{ClientError, HubClient};
use contracts::{RaiseNotificationIn, Severity};
use fleetix::mcp_server::{Effect, Server};
use rmcp::handler::server::common::schema_for_input;
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Tool};
use rmcp::schemars;
use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::plugin::{ClientContext, McpHost, Pack, validate_packs};

/// Stringify a contract value for a tool result.
macro_rules! json_text {
    ($value:expr) => {
        serde_json::to_string_pretty(&$value).unwrap_or_else(|error| format!("serialize failed: {error}"))
    };
}

/// The catalog with its authorizer installed, ready to serve or to inspect
/// without stdio.
pub fn catalog(ctx: ClientContext, packs: Vec<Box<dyn Pack>>) -> anyhow::Result<Server> {
    validate_packs(&packs)?;
    let mut server = Server::new("fleetix-hub-mcp", env!("CARGO_PKG_VERSION"));
    for (effect, route) in routes(&ctx) {
        let name = route.name().to_owned();
        // A duplicate name is a packaging error (for example a pack that still
        // ships an old copy of a generic hub tool): fail startup, never shadow.
        server.register(effect, route).map_err(|e| anyhow::anyhow!("registering MCP tool {name}: {e:?}"))?;
    }
    {
        let mut host = McpHost::new(&ctx, &mut server);
        for pack in &packs {
            pack.install_mcp(&mut host)?;
        }
    }
    // The hub token decides what the API accepts; this gate only keeps
    // Destructive tools off a server that never registers one.
    Ok(server.with_authorizer(|invocation| matches!(invocation.effect, Effect::Read | Effect::Write)))
}

/// Serve the catalog on stdin/stdout until EOF.
pub async fn serve(ctx: ClientContext, packs: Vec<Box<dyn Pack>>) -> anyhow::Result<()> {
    catalog(ctx, packs)?.serve_stdio().await.map_err(|e| anyhow::anyhow!("serving MCP over stdio: {e}"))?;
    Ok(())
}

/// The catalog in registration order, with its effects fixed here.
fn routes(ctx: &ClientContext) -> Vec<(Effect, ToolRoute<()>)> {
    let hub = ctx.hub.clone();
    vec![
        (Effect::Read, hub_status_route(hub.clone())),
        (
            Effect::Read,
            read_tool("hub_canon_review", "Canon proposals waiting for comments.", hub.clone(), |hub| async move {
                hub.canon_reviews().await.map(|value| json_text!(value))
            }),
        ),
        (Effect::Write, canon_comment_tool(hub.clone())),
        (Effect::Read, individual_pass_status_tool(hub.clone())),
        (
            Effect::Read,
            read_tool("hub_projects", "Projects, their members, accounts, dynamics and model settings.", hub.clone(), |hub| async move {
                hub.projects().await.map(|value| json_text!(value))
            }),
        ),
        (
            Effect::Read,
            read_tool("hub_secrets", "Secret inventory and expiry states.", hub.clone(), |hub| async move {
                hub.secrets().await.map(|value| json_text!(value))
            }),
        ),
        (
            Effect::Read,
            read_tool(
                "hub_today",
                "Operator queues: pending reviews, active production, due releases, failed operations and overdue interactions.",
                hub.clone(),
                |hub| async move { hub.today().await.map(|value| json_text!(value)) },
            ),
        ),
        (Effect::Read, content_tool(hub.clone())),
        (
            Effect::Read,
            read_tool(
                "hub_relationships",
                "Engagement threads with their owner, owner epoch and follow-up time.",
                hub.clone(),
                |hub| async move { hub.relationships().await.map(|value| json_text!(value)) },
            ),
        ),
        (
            Effect::Read,
            read_tool(
                "hub_intents",
                "The publication queue: intents with their releases, due time, state and delivery attempts.",
                hub.clone(),
                |hub| async move { hub.intents().await.map(|value| json_text!(value)) },
            ),
        ),
        (
            Effect::Read,
            read_tool(
                "hub_renewals",
                "Credential and account renewal deadlines: cadence, warn window, due time and the procedure to renew.",
                hub.clone(),
                |hub| async move { hub.renewals().await.map(|value| json_text!(value)) },
            ),
        ),
        (Effect::Read, opportunities_tool(hub.clone())),
        (Effect::Read, opportunity_tool(hub.clone())),
        (Effect::Read, notifications_tool(hub.clone())),
        (Effect::Write, create_brief_tool(hub.clone())),
        (Effect::Write, draft_reply_tool(hub.clone())),
        (Effect::Write, assistant_draft_tool(hub.clone())),
        (Effect::Write, ingest_opportunity_tool(hub.clone())),
        (Effect::Write, notify_tool(hub.clone())),
        (Effect::Write, summary_tool(hub)),
    ]
}

fn individual_pass_status_tool(hub: HubClient) -> ToolRoute<()> {
    ToolRoute::new(
        Tool::new(
            "hub_individual_pass_status",
            "Generation pass status for an individual.",
            schema(json!({"persona_id": {"type": "string", "description": "Persona UUID."}}), &["persona_id"]),
        ),
        move |Parameters(args): Parameters<Value>| {
            let hub = hub.clone();
            async move {
                let id = match required_uuid(&args, "persona_id").and_then(|id| uuid::Uuid::parse_str(id).map_err(|e| e.to_string())) {
                    Ok(id) => id,
                    Err(error) => return failed(error),
                };
                match hub.individual_look(id).await {
                    Ok(look) => {
                        let passes: Vec<Value> = look.passes.into_iter().map(|pass| json!({
                            "generation_id": pass.generation.id,
                            "state": pass.generation.state,
                            "source_asset_id": pass.generation.source_asset_id,
                            "attempts": pass.generation.attempts.into_iter().map(|a| json!({"state": a.state, "progress": a.progress})).collect::<Vec<_>>(),
                            "asset_ids": pass.assets.into_iter().map(|asset| asset.id).collect::<Vec<_>>()
                        })).collect();
                        render(json_text!(passes))
                    }
                    Err(error) => failed(error.to_string()),
                }
            }
        },
    )
}

fn canon_comment_tool(hub: HubClient) -> ToolRoute<()> {
    ToolRoute::new(
        Tool::new(
            "hub_canon_comment",
            "Comment on a proposed canon revision.",
            schema(
                json!({
                    "revision_id": {"type": "string", "description": "Revision UUID."},
                    "body": {"type": "string"}
                }),
                &["revision_id", "body"],
            ),
        ),
        move |Parameters(args): Parameters<Value>| {
            let hub = hub.clone();
            async move {
                let id = match required_uuid(&args, "revision_id") {
                    Ok(id) => match uuid::Uuid::parse_str(id) {
                        Ok(id) => id,
                        Err(error) => return failed(error.to_string()),
                    },
                    Err(error) => return failed(error),
                };
                let body: contracts::CanonCommentIn = match serde_json::from_value(args) {
                    Ok(body) => body,
                    Err(error) => return failed(error.to_string()),
                };
                match hub.comment_canon(id, &body).await {
                    Ok(value) => render(json_text!(value)),
                    Err(error) => failed(error.to_string()),
                }
            }
        },
    )
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HubStatusArgs {}

/// Read the hub's status as the configured principal; server-side role checks
/// still apply to the token.
fn hub_status_route(hub: HubClient) -> ToolRoute<()> {
    let schema = schema_for_input::<HubStatusArgs>().expect("hub_status has a static schema");
    ToolRoute::new(
        Tool::new("hub_status", "Integration modes, pause state and the authenticated principal.", schema),
        move |Parameters(_): Parameters<HubStatusArgs>| {
            let hub = hub.clone();
            async move {
                match hub.get::<contracts::SystemStatus>("/status").await {
                    Ok(status) => render(json_text!(status)),
                    Err(e) => failed(e.to_string()),
                }
            }
        },
    )
}

/// One no-argument read tool over a typed `HubClient` call.
fn read_tool<F, Fut>(name: &'static str, description: &'static str, hub: HubClient, call: F) -> ToolRoute<()>
where
    F: Fn(HubClient) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<String, ClientError>> + Send + 'static,
{
    ToolRoute::new(Tool::new(name, description, schema(json!({}), &[])), move |Parameters(_): Parameters<Value>| {
        let future = call(hub.clone());
        async move {
            match future.await {
                Ok(text) => render(text),
                Err(error) => failed(error.to_string()),
            }
        }
    })
}

/// Content list, or one item with its generations, assets, releases, intents
/// and history when `id` is given.
fn content_tool(hub: HubClient) -> ToolRoute<()> {
    ToolRoute::new(
        Tool::new(
            "hub_content",
            "Content items, or one item with its generations, assets, releases, intents and history when `id` is given.",
            schema(json!({ "id": { "type": "string", "description": "Content item UUID; omit to list every item." } }), &[]),
        ),
        move |Parameters(args): Parameters<Value>| {
            let hub = hub.clone();
            async move {
                match args.get("id").and_then(Value::as_str) {
                    None => match hub.content().await {
                        Ok(items) => render(json_text!(items)),
                        Err(error) => failed(error.to_string()),
                    },
                    Some(id) => {
                        let id = match uuid_text(id, "id") {
                            Ok(id) => id.to_owned(),
                            Err(error) => return failed(error),
                        };
                        match hub.get::<contracts::ContentDetail>(&format!("/content/{id}")).await {
                            Ok(detail) => render(json_text!(detail)),
                            Err(error) => failed(error.to_string()),
                        }
                    }
                }
            }
        },
    )
}

/// Opportunity list, filtered by wire state and a deadline window.
fn opportunities_tool(hub: HubClient) -> ToolRoute<()> {
    ToolRoute::new(
        Tool::new(
            "hub_opportunities",
            "Opportunities: funding calls, fellowships, prizes, venues and programmes with their state, deadline and derived `expired`.",
            schema(
                json!({
                    "state": { "type": "string", "description": "new|shortlisted|drafting|submitted|awarded|rejected|declined." },
                    "due_within_days": { "type": "integer", "description": "Only calls due within this many days." },
                }),
                &[],
            ),
        ),
        move |Parameters(args): Parameters<Value>| {
            let hub = hub.clone();
            async move {
                let state = args.get("state").and_then(Value::as_str);
                if let Some(state) = state
                    && contracts::OpportunityState::parse(state).is_none()
                {
                    return failed(format!("state must be one of the opportunity states, got {state:?}"));
                }
                let due = match args.get("due_within_days") {
                    None | Some(Value::Null) => None,
                    Some(value) => match value.as_i64().and_then(|days| i32::try_from(days).ok()) {
                        Some(days) => Some(days),
                        None => return failed("due_within_days must be an integer".to_string()),
                    },
                };
                match hub.opportunities(state, due).await {
                    Ok(items) => render(json_text!(items)),
                    Err(error) => failed(error.to_string()),
                }
            }
        },
    )
}

fn opportunity_tool(hub: HubClient) -> ToolRoute<()> {
    ToolRoute::new(
        Tool::new(
            "hub_opportunity",
            "One opportunity by id, with its decision state and derived `expired`.",
            schema(json!({ "id": { "type": "string", "description": "Opportunity UUID." } }), &["id"]),
        ),
        move |Parameters(args): Parameters<Value>| {
            let hub = hub.clone();
            async move {
                let Some(id) = args.get("id").and_then(Value::as_str) else {
                    return failed("id is required and must be a string".to_string());
                };
                let Ok(id) = id.parse::<uuid::Uuid>() else {
                    return failed("id must be a UUID".to_string());
                };
                match hub.opportunity(id).await {
                    Ok(item) => render(json_text!(item)),
                    Err(error) => failed(error.to_string()),
                }
            }
        },
    )
}

fn ingest_opportunity_tool(hub: HubClient) -> ToolRoute<()> {
    let evidence = json!({
        "type": "object",
        "properties": { "url": { "type": "string" }, "quote": { "type": "string" } },
        "required": ["url", "quote"],
    });
    ToolRoute::new(
        Tool::new(
            "hub_ingest_opportunity",
            "File a funding call, fellowship, prize, venue or programme. Body fields follow the hub `OpportunityIn` contract; at least one evidence entry is required.",
            schema(
                json!({
                    "persona_id": { "type": ["string", "null"], "description": "Optional persona UUID." },
                    "source": { "type": "string", "description": "Feed identity, e.g. `n8n:funder-feed`." },
                    "external_id": { "type": "string", "description": "Sender's stable id; retries reuse it." },
                    "kind": { "type": "string", "enum": ["grant", "fellowship", "prize", "venue", "program", "other"] },
                    "title": { "type": "string" },
                    "funder": { "type": "string" },
                    "url": { "type": "string" },
                    "deadline": { "type": ["string", "null"], "description": "RFC 3339; null means rolling or unknown." },
                    "amount": { "type": ["string", "null"] },
                    "eligibility": { "type": "string" },
                    "fit": { "type": "string" },
                    "evidence": { "type": "array", "items": evidence },
                }),
                &["source", "external_id", "kind", "title", "funder", "url", "evidence"],
            ),
        ),
        move |Parameters(args): Parameters<Value>| {
            let hub = hub.clone();
            async move {
                match serde_json::from_value::<contracts::OpportunityIn>(args) {
                    Ok(body) => match hub.ingest_opportunity(&body).await {
                        Ok(item) => render(json_text!(item)),
                        Err(error) => failed(error.to_string()),
                    },
                    Err(error) => failed(format!("invalid OpportunityIn: {error}")),
                }
            }
        },
    )
}

fn create_brief_tool(hub: HubClient) -> ToolRoute<()> {
    let brief = json!({
        "type": "object",
        "properties": {
            "angle": { "type": "string" },
            "audience": { "type": "string" },
            "copy_direction": { "type": "string" },
            "visual_direction": { "type": "string" },
            "producer": { "type": "string", "enum": ["delegated", "direct"], "default": "delegated" },
        },
        "required": ["angle", "audience", "copy_direction", "visual_direction"],
    });
    ToolRoute::new(
        Tool::new(
            "hub_create_brief",
            "Create a content brief. Body fields follow the hub `CreateBriefIn` contract; `producer` defaults to `delegated`.",
            schema(
                json!({
                    "persona_id": { "type": "string", "description": "Persona UUID." },
                    "news_item_id": { "type": ["string", "null"], "description": "Optional news item UUID." },
                    "title": { "type": "string" },
                    "brief": brief,
                }),
                &["persona_id", "title", "brief"],
            ),
        ),
        move |Parameters(args): Parameters<Value>| {
            let hub = hub.clone();
            async move {
                match serde_json::from_value::<contracts::CreateBriefIn>(args) {
                    Ok(body) => match hub.create_brief(&body).await {
                        Ok(item) => render(json_text!(item)),
                        Err(error) => failed(error.to_string()),
                    },
                    Err(error) => failed(format!("invalid CreateBriefIn: {error}")),
                }
            }
        },
    )
}

fn draft_reply_tool(hub: HubClient) -> ToolRoute<()> {
    ToolRoute::new(
        Tool::new(
            "hub_draft_reply",
            "Draft a reply on an engagement thread. `owner_epoch` is the epoch the author saw; stale epochs are rejected.",
            schema(
                json!({
                    "relationship_id": { "type": "string", "description": "Relationship UUID." },
                    "body": { "type": "string" },
                    "in_reply_to": { "type": ["string", "null"], "description": "Optional interaction UUID." },
                    "owner_epoch": { "type": "integer" },
                }),
                &["relationship_id", "body", "owner_epoch"],
            ),
        ),
        move |Parameters(args): Parameters<Value>| {
            let hub = hub.clone();
            async move {
                let relationship = match required_uuid(&args, "relationship_id") {
                    Ok(id) => id.to_owned(),
                    Err(error) => return failed(error),
                };
                let body: contracts::DraftReplyIn = match serde_json::from_value(args) {
                    Ok(body) => body,
                    Err(error) => return failed(format!("invalid DraftReplyIn: {error}")),
                };
                match hub
                    .post::<contracts::DraftReplyIn, contracts::ReplyDraft>(&format!("/relationships/{relationship}/drafts"), &body)
                    .await
                {
                    Ok(draft) => render(json_text!(draft)),
                    Err(error) => failed(error.to_string()),
                }
            }
        },
    )
}

fn assistant_draft_tool(hub: HubClient) -> ToolRoute<()> {
    ToolRoute::new(
        Tool::new(
            "hub_assistant_draft_reply",
            "Ask the persona assistant to draft a reply on an engagement thread. The draft waits for operator approval.",
            schema(json!({ "relationship_id": { "type": "string", "description": "Relationship UUID." } }), &["relationship_id"]),
        ),
        move |Parameters(args): Parameters<Value>| {
            let hub = hub.clone();
            async move {
                let relationship = match required_uuid(&args, "relationship_id") {
                    Ok(id) => id.to_owned(),
                    Err(error) => return failed(error),
                };
                match hub.post::<(), contracts::ReplyDraft>(&format!("/relationships/{relationship}/draft-reply"), &()).await {
                    Ok(draft) => render(json_text!(draft)),
                    Err(error) => failed(error.to_string()),
                }
            }
        },
    )
}

/// The operator's notification set: open rows by default, everything with
/// `all`. Snapshot only; the feed cursor belongs to the desktop and the tray.
fn notifications_tool(hub: HubClient) -> ToolRoute<()> {
    ToolRoute::new(
        Tool::new(
            "hub_notifications",
            "Notifications for the operator: severity, title, body, source, dedupe key and whether each row is still open. `all` includes acknowledged and resolved rows.",
            schema(json!({ "all": { "type": "boolean", "description": "Include acknowledged and resolved rows." } }), &[]),
        ),
        move |Parameters(args): Parameters<Value>| {
            let hub = hub.clone();
            async move {
                let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
                match hub.notifications(None, 0, all).await {
                    Ok(page) => render(json_text!(page)),
                    Err(error) => failed(error.to_string()),
                }
            }
        },
    )
}

/// An agent or procedure raises one row. `source` stays server-side (`agent`):
/// a caller that could name a producer source would fake and then resolve it.
fn notify_tool(hub: HubClient) -> ToolRoute<()> {
    ToolRoute::new(
        Tool::new(
            "hub_notify",
            "Raise a notification for the operator. `dedupe_key` is its stable identity: the same key and text again touches nothing, changed text replaces the row. No tool acknowledges or snoozes.",
            schema(
                json!({
                    "dedupe_key": { "type": "string", "description": "Stable identity, e.g. `agent:nightly:atlas`." },
                    "severity": { "type": "string", "enum": ["info", "warning", "critical"] },
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                }),
                &["dedupe_key", "severity", "title"],
            ),
        ),
        move |Parameters(args): Parameters<Value>| {
            let hub = hub.clone();
            async move {
                let Some(dedupe_key) = args.get("dedupe_key").and_then(Value::as_str) else {
                    return failed("dedupe_key is required and must be a string");
                };
                let Some(severity) = args.get("severity").and_then(Value::as_str).and_then(Severity::parse) else {
                    return failed("severity must be one of info, warning or critical");
                };
                let Some(title) = args.get("title").and_then(Value::as_str) else {
                    return failed("title is required and must be a string");
                };
                let input = RaiseNotificationIn {
                    source: None,
                    dedupe_key: dedupe_key.to_owned(),
                    severity,
                    title: title.to_owned(),
                    body: args.get("body").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    subject_type: None,
                    subject_id: None,
                    url: None,
                };
                match hub.raise_notification(&input).await {
                    Ok(notification) => render(json_text!(notification)),
                    Err(error) => failed(error.to_string()),
                }
            }
        },
    )
}

/// The daily summary is a notification keyed `summary:<day>`: a re-run of the
/// same day replaces its body, and only a changed body is announced again.
fn summary_tool(hub: HubClient) -> ToolRoute<()> {
    ToolRoute::new(
        Tool::new(
            "hub_post_summary",
            "Write the day's summary as one notification, keyed `summary:<day>`. Re-posting the same day replaces its body; only a changed one is announced again.",
            schema(
                json!({
                    "day": { "type": "string", "description": "The day the summary covers, `YYYY-MM-DD` in Europe/Berlin." },
                    "body": { "type": "string", "description": "At most 25 lines of plain text." },
                    "severity": { "type": "string", "enum": ["info", "warning", "critical"], "default": "info" },
                }),
                &["day", "body"],
            ),
        ),
        move |Parameters(args): Parameters<Value>| {
            let hub = hub.clone();
            async move {
                let Some(day) = args.get("day").and_then(Value::as_str) else {
                    return failed("day is required and must be a string");
                };
                let Some(body) = args.get("body").and_then(Value::as_str) else {
                    return failed("body is required and must be a string");
                };
                let severity = match args.get("severity").and_then(Value::as_str) {
                    None => Severity::Info,
                    Some(raw) => match Severity::parse(raw) {
                        Some(severity) => severity,
                        None => return failed("severity must be one of info, warning or critical"),
                    },
                };
                let input = match summary_input(day, body, severity) {
                    Ok(input) => input,
                    Err(error) => return failed(error),
                };
                match hub.raise_notification(&input).await {
                    Ok(notification) => render(json_text!(notification)),
                    Err(error) => failed(error.to_string()),
                }
            }
        },
    )
}

/// The `hub_post_summary` request for one day. `day` becomes part of a dedupe
/// key, so it is shape-checked here and a bad one never reaches the API.
pub fn summary_input(day: &str, body: &str, severity: Severity) -> Result<RaiseNotificationIn, String> {
    let bytes = day.as_bytes();
    // Ten bytes with dashes in place, then a real calendar date: `2026-13-01`
    // and `2026-11-15x` must not name a summary row.
    let shaped = bytes.len() == 10 && bytes[4] == b'-' && bytes[7] == b'-';
    if !shaped || chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").is_err() {
        return Err(format!("day must be a calendar date `YYYY-MM-DD`, got {day:?}"));
    }
    Ok(RaiseNotificationIn {
        source: Some("summary".into()),
        dedupe_key: format!("summary:{day}"),
        severity,
        title: format!("Daily summary {day}"),
        body: body.to_owned(),
        subject_type: None,
        subject_id: None,
        url: None,
    })
}

pub(crate) fn render(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

pub(crate) fn failed(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

pub(crate) fn schema(properties: Value, required: &[&str]) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    map.insert("type".into(), json!("object"));
    map.insert("properties".into(), properties);
    map.insert("required".into(), json!(required));
    map.insert("additionalProperties".into(), json!(false));
    map
}

fn required_uuid<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    let value = args.get(key).and_then(Value::as_str).ok_or_else(|| format!("{key} is required and must be a string"))?;
    uuid_text(value, key)
}

/// Ids travel as path segments; anything but a UUID could change the request
/// path.
fn uuid_text<'a>(value: &'a str, key: &str) -> Result<&'a str, String> {
    let bytes = value.as_bytes();
    let dashed: [usize; 4] = [8, 13, 18, 23];
    let shaped = bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, byte)| if dashed.contains(&i) { *byte == b'-' } else { byte.is_ascii_hexdigit() });
    if shaped { Ok(value) } else { Err(format!("{key} must be a UUID")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ClientContext {
        ClientContext { hub: HubClient::new("http://127.0.0.1:9", "test-token"), config: json!({}) }
    }

    #[tokio::test]
    async fn registers_the_generic_hub_tools_once() {
        let routes = routes(&context());
        let actual: Vec<(&str, Effect)> = routes.iter().map(|(effect, route)| (route.name(), *effect)).collect();
        assert_eq!(
            actual,
            [
                ("hub_status", Effect::Read),
                ("hub_canon_review", Effect::Read),
                ("hub_canon_comment", Effect::Write),
                ("hub_individual_pass_status", Effect::Read),
                ("hub_projects", Effect::Read),
                ("hub_secrets", Effect::Read),
                ("hub_today", Effect::Read),
                ("hub_content", Effect::Read),
                ("hub_relationships", Effect::Read),
                ("hub_intents", Effect::Read),
                ("hub_renewals", Effect::Read),
                ("hub_opportunities", Effect::Read),
                ("hub_opportunity", Effect::Read),
                ("hub_notifications", Effect::Read),
                ("hub_create_brief", Effect::Write),
                ("hub_draft_reply", Effect::Write),
                ("hub_assistant_draft_reply", Effect::Write),
                ("hub_ingest_opportunity", Effect::Write),
                ("hub_notify", Effect::Write),
                ("hub_post_summary", Effect::Write),
            ]
        );
        let mut names: Vec<&str> = actual.iter().map(|(name, _)| *name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "every tool name is registered exactly once");
        for (_, route) in &routes {
            assert!(route.attr.description.is_some(), "{} has no description", route.name());
        }
    }

    #[test]
    fn uuid_path_segments_reject_path_metacharacters() {
        assert!(uuid_text("3f2504e0-4f89-11d3-9a0c-0305e82c3301", "id").is_ok());
        assert!(uuid_text("../../status", "id").is_err());
        assert!(uuid_text("3f2504e0-4f89-11d3-9a0c-0305e82c330", "id").is_err());
    }

    #[test]
    fn summary_days_are_calendar_dates_and_name_their_row() {
        let input = summary_input("2026-11-15", "headline\nFleet: nothing", Severity::Warning).unwrap();
        assert_eq!(input.source.as_deref(), Some("summary"));
        assert_eq!(input.dedupe_key, "summary:2026-11-15");
        assert_eq!(input.title, "Daily summary 2026-11-15");
        assert_eq!(input.severity, Severity::Warning);

        for bad in ["2026-13-01", "2026-11-15x", "2026-02-30", "26-11-15", "2026-1-15", ""] {
            assert!(summary_input(bad, "body", Severity::Info).is_err(), "{bad:?} must not be a summary day");
        }
    }
}
