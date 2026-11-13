//! MCP entry point: read-only hub tools over stdio.
//!
//! The embedding binary installs the authorizer. v1 registers read tools and
//! future hub Write tools; `Destructive` stays denied because no hub tool may
//! mutate a host or run a shell.

use fleetix::mcp_server::{Effect, Server};
use rmcp::handler::server::common::schema_for_input;
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Tool};
use rmcp::schemars;
use rmcp::schemars::JsonSchema;
use serde::Deserialize;

use crate::plugin::{ClientContext, McpHost, Pack, validate_packs};

/// Serve the MCP catalog on stdin/stdout until EOF.
pub async fn serve(ctx: ClientContext, packs: Vec<Box<dyn Pack>>) -> anyhow::Result<()> {
    validate_packs(&packs)?;
    let mut server = Server::new("fleetix-hub-mcp", env!("CARGO_PKG_VERSION"));
    server.register(Effect::Read, hub_status_route(ctx.hub.clone())).map_err(|e| anyhow::anyhow!("registering hub_status: {e:?}"))?;
    {
        let mut host = McpHost::new(&ctx, &mut server);
        for pack in &packs {
            pack.install_mcp(&mut host)?;
        }
    }
    // The hub token decides what the API accepts; this gate only keeps
    // Destructive tools off a server that never registers one.
    let server = server.with_authorizer(|invocation| matches!(invocation.effect, Effect::Read | Effect::Write));
    server.serve_stdio().await.map_err(|e| anyhow::anyhow!("serving MCP over stdio: {e}"))?;
    Ok(())
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HubStatusArgs {}

/// Read the hub's status as the configured principal; server-side role checks
/// still apply to the token.
fn hub_status_route(hub: contracts::client::HubClient) -> ToolRoute<()> {
    let schema = schema_for_input::<HubStatusArgs>().expect("hub_status has a static schema");
    ToolRoute::new(
        Tool::new("hub_status", "Integration modes, pause state and the authenticated principal.", schema),
        move |Parameters(_): Parameters<HubStatusArgs>| {
            let hub = hub.clone();
            async move {
                match hub.get::<contracts::SystemStatus>("/status").await {
                    Ok(status) => CallToolResult::success(vec![ContentBlock::text(
                        serde_json::to_string_pretty(&status).unwrap_or_else(|e| format!("serialize failed: {e}")),
                    )]),
                    Err(e) => CallToolResult::error(vec![ContentBlock::text(e.to_string())]),
                }
            }
        },
    )
}
