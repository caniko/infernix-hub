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
    Influencers,
    Influencer {
        #[command(subcommand)]
        cmd: InfluencerCmd,
    },
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
    /// Funding calls: `--state` and `--due-within` filter the list.
    Opportunities {
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        due_within: Option<i32>,
    },
    Opportunity {
        #[command(subcommand)]
        cmd: OpportunityCmd,
    },
    /// Credential renewals: what must be re-authorized before a channel dies.
    Renewals {
        #[command(subcommand)]
        cmd: RenewalCmd,
    },
    /// Operator notifications: the current set, plus the operator-only
    /// acknowledge and snooze actions.
    Notifications {
        #[command(subcommand)]
        cmd: NotificationCmd,
    },
    /// Raise one notification; the key is its dedupe identity.
    Notify {
        #[arg(long)]
        key: String,
        /// info|warning|critical.
        #[arg(long)]
        severity: String,
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        body: String,
    },
    /// Approve the release's native Paperclip approval with the operator's own
    /// board key, then link it. `--reject` records a rejection instead.
    Approve {
        release: Uuid,
        #[arg(long)]
        reject: bool,
        #[arg(long)]
        note: Option<String>,
    },
    Pause {
        #[arg(long)]
        reason: Option<String>,
    },
    Resume,
}

#[derive(Subcommand)]
enum InfluencerCmd {
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        kind: String,
        #[arg(long, default_value = "")]
        premise: String,
        #[arg(long, default_value = "")]
        dynamics: String,
    },
    AddMember {
        id: Uuid,
        #[arg(long)]
        persona: Uuid,
        #[arg(long, default_value = "")]
        role: String,
    },
}

#[derive(Subcommand)]
enum FleetCmd {
    /// The snapshot written by `canix fleet status --write`.
    Status,
}

#[derive(Subcommand)]
enum OpportunityCmd {
    /// One opportunity by id.
    Show { id: Uuid },
    /// Operator-only decision: new|shortlisted|drafting|submitted|awarded|rejected|declined.
    Decide {
        id: Uuid,
        state: String,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        issue: Option<String>,
    },
    /// File a call from a JSON `OpportunityIn` document.
    Ingest { file: std::path::PathBuf },
}

#[derive(Subcommand)]
enum RenewalCmd {
    /// Every renewal with its cadence, warn window, due time and procedure.
    List,
    /// Operator-only: mark renewed; `--due` wins over the stored cadence.
    Renewed {
        key: String,
        #[arg(long)]
        due: Option<DateTime<Utc>>,
    },
    /// Operator or procedure: record a provider observation; never a renewal.
    Observe {
        key: String,
        /// date | never | unverifiable | inactive | not_yet_valid
        #[arg(long)]
        outcome: String,
        /// Required for `date`, refused for every other outcome.
        #[arg(long)]
        expires_at: Option<DateTime<Utc>>,
        /// RFC 3339; defaults to now, refused when over 60 s in the future.
        #[arg(long)]
        observed_at: Option<DateTime<Utc>>,
    },
}

#[derive(Subcommand)]
enum NotificationCmd {
    /// Snapshot of the newest rows; `--all` includes acknowledged and resolved.
    List {
        #[arg(long)]
        all: bool,
    },
    /// Operator-only: acknowledge ids, or every open row with `--all`.
    Ack {
        ids: Vec<Uuid>,
        #[arg(long)]
        all: bool,
    },
    /// Operator-only: hide one row until a time.
    Snooze {
        id: Uuid,
        #[arg(long)]
        until: DateTime<Utc>,
    },
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
        Cmd::Influencers => print(c.influencers().await?),
        Cmd::Influencer { cmd: InfluencerCmd::Create { name, kind, premise, dynamics } } => {
            let kind = InfluencerKind::parse(&kind).ok_or_else(|| anyhow::anyhow!("kind must be solo or group"))?;
            print(c.create_influencer(&InfluencerIn { name, kind, premise, dynamics }).await?)
        }
        Cmd::Influencer { cmd: InfluencerCmd::AddMember { id, persona, role } } => {
            print(c.put_influencer_member(id, persona, &InfluencerMemberIn { role }).await?)
        }
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
        Cmd::Select { asset, alt } => {
            print(c.select_asset(asset, &SelectAssetIn { selected: true, alt_text: Some(alt), ..Default::default() }).await?)
        }
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
        Cmd::Opportunities { state, due_within } => print(c.opportunities(state.as_deref(), due_within).await?),
        Cmd::Opportunity { cmd } => match cmd {
            OpportunityCmd::Show { id } => print(c.opportunity(id).await?),
            OpportunityCmd::Decide { id, state, note, issue } => {
                let state = OpportunityState::parse(&state).ok_or_else(|| anyhow::anyhow!("unknown opportunity state {state:?}"))?;
                print(c.decide_opportunity(id, &OpportunityDecisionIn { state, note, paperclip_issue_id: issue }).await?)
            }
            OpportunityCmd::Ingest { file } => {
                let body: OpportunityIn = serde_json::from_str(&std::fs::read_to_string(&file)?)?;
                print(c.ingest_opportunity(&body).await?)
            }
        },
        Cmd::Renewals { cmd } => match cmd {
            RenewalCmd::List => print(c.renewals().await?),
            RenewalCmd::Renewed { key, due } => print(c.renewal_renewed(&key, &RenewalRenewedIn { due_at: due }).await?),
            RenewalCmd::Observe { key, outcome, expires_at, observed_at } => {
                let outcome = Observation::parse(&outcome).ok_or_else(|| {
                    anyhow::anyhow!("unknown outcome {outcome:?}; use date, never, unverifiable, inactive or not_yet_valid")
                })?;
                let body = ObserveRenewalIn { outcome, expires_at, observed_at: observed_at.unwrap_or_else(Utc::now) };
                print(c.observe_renewal(&key, &body).await?)
            }
        },
        Cmd::Notifications { cmd } => match cmd {
            NotificationCmd::List { all } => print(c.notifications(None, 0, all).await?),
            NotificationCmd::Ack { ids, all } => print(c.ack_notifications(&AckNotificationsIn { ids, all }).await?),
            NotificationCmd::Snooze { id, until } => print(c.snooze_notification(id, &SnoozeNotificationIn { until }).await?),
        },
        Cmd::Notify { key, severity, title, body } => {
            let severity = Severity::parse(&severity)
                .ok_or_else(|| anyhow::anyhow!("unknown severity {severity:?}; use info, warning or critical"))?;
            print(
                c.raise_notification(&RaiseNotificationIn {
                    source: None,
                    dedupe_key: key,
                    severity,
                    title,
                    body,
                    subject_type: None,
                    subject_id: None,
                    url: None,
                })
                .await?,
            )
        }
        Cmd::Approve { release, reject, note } => {
            let revision = c.release(release).await?;
            // Show exactly what the approval authorizes before deciding.
            print(json!({
                "release": revision.revision,
                "state": revision.state,
                "payload_hash": revision.payload_hash,
                "text": revision.actions.iter().map(|a| a.text.clone()).collect::<Vec<_>>(),
                "platform_settings": revision.actions.iter().map(|a| a.platform_settings.clone()).collect::<Vec<_>>(),
            }));
            let approval_id = revision
                .paperclip_approval_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("release {release} has no Paperclip approval; request review first"))?;
            // The operator's key stays local; the hub never sees it.
            let board = contracts::client::PaperclipBoard::from_env()?;
            board.decide(&approval_id, !reject, note.as_deref()).await?;
            print(c.link_approval(release, &LinkApprovalIn { approval_id }).await?);
        }
        Cmd::Pause { reason } => print(c.set_pause(&PauseIn { paused: true, reason }).await?),
        Cmd::Resume => print(c.set_pause(&PauseIn { paused: false, reason: None }).await?),
    }
    Ok(())
}
