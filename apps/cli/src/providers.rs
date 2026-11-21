use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Subcommand, ValueEnum};
use contracts::{client::HubClient, providers::*};
use uuid::Uuid;

mod collect;
mod transfer;

#[derive(Subcommand)]
pub(super) enum Command {
    List {
        #[arg(long)]
        json: bool,
    },
    Quota {
        #[arg(value_parser = provider_id)]
        provider_id: String,
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value_t = 48, value_parser = clap::value_parser!(u32).range(1..=840))]
        range_hours: u32,
    },
    Accounts {
        #[arg(value_parser = provider_id)]
        provider_id: String,
    },
    Refresh {
        #[arg(value_parser = provider_id)]
        provider_id: String,
        #[arg(long)]
        account: Option<String>,
    },
    Use {
        #[arg(value_parser = provider_id)]
        provider_id: String,
        account_id: String,
        #[arg(long, conflicts_with = "persistent_scope")]
        pool_override: bool,
        #[arg(long, value_enum)]
        persistent_scope: Option<Scope>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        user: Option<String>,
    },
    Collect(collect::Options),
    /// Encrypt retained activity metadata for offline transfer.
    TransferExport {
        path: PathBuf,
    },
    /// Authenticate and merge an offline activity metadata archive.
    TransferImport {
        path: PathBuf,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum Scope {
    Fleet,
    Host,
    User,
}

fn provider_id(s: &str) -> Result<String, &'static str> {
    if s.is_empty() || matches!(s, "." | "..") || !s.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c)) {
        return Err("invalid provider id");
    }
    Ok(s.into())
}

fn text(s: &str) -> String {
    s.chars().filter(|ch| !ch.is_control()).collect()
}

fn pct(n: Option<f64>) -> String {
    n.map_or_else(|| "—".into(), |n| format!("{n:.1}%"))
}

fn date(t: Option<chrono::DateTime<chrono::Utc>>) -> String {
    t.map_or_else(|| "—".into(), |t| t.to_rfc3339())
}

async fn submit(
    client: &HubClient,
    provider: &str,
    command: ProviderCommand,
    account: Option<String>,
    host: Option<String>,
    user: Option<String>,
) -> Result<()> {
    let op = client
        .submit_provider_operation(
            provider,
            &SubmitOperationIn {
                idempotency_key: Uuid::new_v4().to_string(),
                expected_revision: None,
                target_account_id: account,
                target_host: host,
                target_user: user,
                command,
            },
        )
        .await?;
    let status = op.status;
    crate::print(op);
    if matches!(status, OperationStatus::Failed | OperationStatus::Ambiguous | OperationStatus::Cancelled) {
        bail!("provider operation ended with status {status:?}");
    }
    Ok(())
}

pub(super) async fn run(client: &HubClient, cmd: Command) -> Result<()> {
    match cmd {
        Command::List { json } => {
            let rows = client.providers().await?;
            if json {
                crate::print(rows);
            } else {
                println!("PROVIDER\tHEALTH\tENABLED/TOTAL\tACTIVE\tPREFERRED\tPOOLS REMAINING");
                for row in rows {
                    let pools =
                        row.pools.iter().map(|p| format!("{} {}", text(&p.label), pct(p.weighted_remaining_percent))).collect::<Vec<_>>();
                    println!(
                        "{}\t{:?}\t{}/{}\t{}\t{}\t{}",
                        text(&row.descriptor.id),
                        row.descriptor.health,
                        row.enabled_accounts,
                        row.account_count,
                        text(row.active_account_id.as_deref().unwrap_or("—")),
                        text(row.preferred_account_id.as_deref().unwrap_or("—")),
                        if pools.is_empty() { "—".into() } else { pools.join(", ") }
                    );
                }
            }
        }
        Command::Quota { provider_id, account, range_hours } => {
            let report = client
                .provider_quota(
                    &provider_id,
                    &QuotaQuery { account_id: account.clone(), range_hours: Some(range_hours), ..Default::default() },
                )
                .await?;
            println!("{} · as of {}", text(&report.provider.label), report.as_of.to_rfc3339());
            for a in report.accounts.iter().filter(|a| account.as_ref().is_none_or(|id| id == &a.id)) {
                for w in &a.windows {
                    println!(
                        "{} / {}: {} remaining; resets {}; {:?}{}",
                        text(&a.id),
                        text(&w.label),
                        pct(w.remaining_percent),
                        date(w.resets_at),
                        w.observation_state,
                        if w.stale { " (stale)" } else { "" }
                    );
                }
            }
            for pool in report.pools {
                println!(
                    "Pool {}: {} remaining ({}/{} accounts; {}); next reset {}",
                    text(&pool.label),
                    pct(pool.weighted_remaining_percent),
                    pool.included_accounts,
                    pool.eligible_accounts,
                    text(&pool.capacity_basis),
                    date(pool.next_reset_at)
                );
                if let Some(f) = pool.forecast {
                    print_forecast(&f);
                }
            }
            for f in report.forecasts {
                print_forecast(&f);
            }
            for w in report.past_windows {
                println!(
                    "Past {} / {}: {} → {}; minimum remaining {}; {} samples",
                    text(&w.account_id),
                    text(&w.label),
                    w.started_at.to_rfc3339(),
                    w.ended_at.to_rfc3339(),
                    pct(w.min_remaining_percent),
                    w.sample_count
                );
            }
        }
        Command::Accounts { provider_id } => {
            println!("ACCOUNT\tSTATUS\tENROLLMENT\tEMAIL\tPLAN\tPRIORITY\tWEIGHT\tSELECTION");
            for a in client.provider_accounts(&provider_id).await? {
                println!(
                    "{}\t{:?}\t{:?}\t{}\t{}\t{}\t{}\t{}{}{}",
                    text(&a.id),
                    a.status,
                    a.enrollment_state,
                    text(a.masked_email.as_deref().or(a.email.as_deref()).unwrap_or("—")),
                    text(a.plan.as_deref().unwrap_or("—")),
                    a.priority,
                    a.weight,
                    if a.is_active { "active " } else { "" },
                    if a.is_preferred { "preferred " } else { "" },
                    if a.is_next_pick { "next" } else { "" }
                );
            }
        }
        Command::Refresh { provider_id, account } => {
            let cmd = ProviderCommand::Quota(QuotaCommand::RefreshQuota { account_id: account.clone() });
            submit(client, &provider_id, cmd, account, None, None).await?;
        }
        Command::Use { provider_id, account_id, pool_override, persistent_scope, host, user } => {
            let id = account_id.clone();
            let command = if pool_override {
                ProviderCommand::Routing(RoutingCommand::SetPoolOverride { account_id: Some(id) })
            } else if let Some(scope) = persistent_scope {
                let (scope, target) = match scope {
                    Scope::Fleet => ("fleet", None),
                    Scope::Host => ("host", Some(host.clone().ok_or_else(|| anyhow::anyhow!("host scope requires --host"))?)),
                    Scope::User => ("user", Some(user.clone().ok_or_else(|| anyhow::anyhow!("user scope requires --user"))?)),
                };
                ProviderCommand::Account(AccountCommand::SelectPersistentAccount { account_id: id, scope: scope.into(), target })
            } else {
                ProviderCommand::Account(AccountCommand::SelectLocalAccount { account_id: id, host: host.clone(), unix_user: user.clone() })
            };
            submit(client, &provider_id, command, Some(account_id), host, user).await?;
        }
        Command::Collect(options) => collect::run(client, options).await?,
        Command::TransferExport { path } => transfer::export(client, &path).await?,
        Command::TransferImport { path } => transfer::import(client, &path).await?,
    }
    Ok(())
}

fn print_forecast(f: &QuotaForecast) {
    println!(
        "Forecast {} / {}: {:.2} points/hour; exhaustion {}; resets first: {}",
        text(f.account_id.as_deref().unwrap_or("pool")),
        text(&f.window_key),
        f.drain_pct_per_hour,
        date(f.projected_exhaustion_at),
        f.resets_before_exhaustion
    );
}
