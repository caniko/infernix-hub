//! `fleetix-hub-mcp`: read-only hub tools over stdio, with no packs.
//!
//! The hub token still decides what the API accepts; this process holds no
//! database or service secret.

use clap::Parser;
use fleetix_hubd::plugin::ClientContext;

#[derive(Parser)]
#[command(name = "fleetix-hub-mcp", version, about = "Fleetix Hub MCP server (read tools)")]
struct Cli {
    #[arg(long, env = "HUB_URL", default_value = "http://127.0.0.1:8470")]
    url: String,
    #[arg(long, env = "HUB_TOKEN", hide_env_values = true)]
    token: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let ctx = ClientContext { hub: contracts::client::HubClient::new(cli.url, cli.token), config: serde_json::json!({}) };
    fleetix_hubd::mcp::serve(ctx, Vec::new()).await
}
