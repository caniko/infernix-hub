//! `hubctl`: the hub from a terminal, through the same API and authorization
//! as the desktop app. Output is JSON so it composes with `jq`.

use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use contracts::client::HubClient;
use contracts::*;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "hubctl", version, about = "fleetix-hub CLI")]
struct Cli {
    #[arg(long, env = "HUB_URL", default_value = "http://127.0.0.1:8470")]
    url: String,
    #[arg(long, env = "HUB_TOKEN", hide_env_values = true)]
    token: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Integration modes, fixture flag, pause state.
    Status,
    /// Fleet infrastructure state; read-only.
    Fleet {
        #[command(subcommand)]
        cmd: FleetCmd,
    },
    Today,
    News,
    Personas,
    Workflows,
    Accounts,
    Content,
    /// Full detail of one content item (generations, assets, releases, history).
    Show {
        id: Uuid,
    },
    Search {
        query: String,
    },
    /// Create a persona-linked brief.
    Brief {
        #[arg(long)]
        persona: Uuid,
        #[arg(long)]
        news: Option<Uuid>,
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        angle: String,
    },
    /// Generate variants with a registered workflow.
    Generate {
        content: Uuid,
        #[arg(long)]
        workflow: Uuid,
        #[arg(long)]
        prompt: String,
        #[arg(long, default_value_t = 2)]
        variants: i32,
    },
    CancelGeneration {
        id: Uuid,
    },
    /// Select an asset for the release (alt text required to freeze).
    Select {
        asset: Uuid,
        #[arg(long)]
        alt: String,
    },
    /// Freeze a release revision for the given accounts.
    Freeze {
        content: Uuid,
        #[arg(long, required = true)]
        account: Vec<String>,
        #[arg(long)]
        text: String,
        #[arg(long)]
        not_before: DateTime<Utc>,
        #[arg(long)]
        not_after: DateTime<Utc>,
    },
    RequestReview {
        release: Uuid,
    },
    Schedule {
        release: Uuid,
        #[arg(long)]
        at: DateTime<Utc>,
    },
    CancelIntent {
        id: Uuid,
    },
    Relationships,
    Thread {
        id: Uuid,
    },
    Draft {
        relationship: Uuid,
        #[arg(long)]
        body: String,
        #[arg(long)]
        epoch: i32,
    },
    ApproveDraft {
        id: Uuid,
    },
    Takeover {
        id: Uuid,
    },
    Handoff {
        id: Uuid,
    },
    Pause {
        #[arg(long)]
        reason: Option<String>,
    },
    Resume,
}

#[derive(Subcommand)]
enum FleetCmd {
    /// The snapshot written by `canix fleet status --write`.
    Status,
}

fn print(v: impl serde::Serialize) {
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let c = HubClient::new(&cli.url, &cli.token);
    match cli.cmd {
        Cmd::Status => print(c.status().await?),
        Cmd::Fleet { cmd: FleetCmd::Status } => print(c.fleet_status().await?),
        Cmd::Today => print(c.today().await?),
        Cmd::News => print(c.news().await?),
        Cmd::Personas => print(c.personas().await?),
        Cmd::Workflows => print(c.workflows().await?),
        Cmd::Accounts => print(c.accounts().await?),
        Cmd::Content => print(c.content().await?),
        Cmd::Show { id } => print(c.content_detail(id).await?),
        Cmd::Search { query } => print(c.search(&query).await?),
        Cmd::Brief { persona, news, title, angle } => print(
            c.create_brief(&CreateBriefIn { persona_id: persona, news_item_id: news, title, brief: Brief { angle, ..Default::default() } })
                .await?,
        ),
        Cmd::Generate { content, workflow, prompt, variants } => {
            print(c.generate(content, &GenerateIn { workflow_id: workflow, inputs: json!({ "prompt": prompt }), variants }).await?)
        }
        Cmd::CancelGeneration { id } => print(c.cancel_generation(id).await?),
        Cmd::Select { asset, alt } => print(c.select_asset(asset, &SelectAssetIn { selected: true, alt_text: Some(alt) }).await?),
        Cmd::Freeze { content, account, text, not_before, not_after } => print(
            c.freeze_release(
                content,
                &FreezeReleaseIn {
                    account_ids: account,
                    text,
                    platform_settings: Value::Null,
                    reply_to: None,
                    timing: TimingScope { not_before, not_after },
                },
            )
            .await?,
        ),
        Cmd::RequestReview { release } => print(c.request_review(release).await?),
        Cmd::Schedule { release, at } => print(c.schedule(release, &ScheduleIn { due_at: at }).await?),
        Cmd::CancelIntent { id } => print(c.cancel_intent(id).await?),
        Cmd::Relationships => print(c.relationships().await?),
        Cmd::Thread { id } => print(c.relationship(id).await?),
        Cmd::Draft { relationship, body, epoch } => {
            print(c.draft_reply(relationship, &DraftReplyIn { in_reply_to: None, body, owner_epoch: epoch }).await?)
        }
        Cmd::ApproveDraft { id } => print(c.decide_draft(id, &DraftDecisionIn::Approve).await?),
        Cmd::Takeover { id } => print(c.takeover(id).await?),
        Cmd::Handoff { id } => print(c.handoff(id).await?),
        Cmd::Pause { reason } => print(c.set_pause(&PauseIn { paused: true, reason }).await?),
        Cmd::Resume => print(c.set_pause(&PauseIn { paused: false, reason: None }).await?),
    }
    Ok(())
}
