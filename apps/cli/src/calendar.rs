use anyhow::Result;
use clap::Subcommand;
use contracts::client::HubClient;

#[derive(Subcommand)]
pub enum Command {
    Sources,
    Agenda {
        #[arg(long, default_value_t = 14, value_parser = clap::value_parser!(u32).range(1..=366))]
        days: u32,
    },
    Sync,
}

pub async fn run(client: &HubClient, command: Command) -> Result<()> {
    match command {
        Command::Sources => super::print(client.list_calendar_sources().await?),
        Command::Agenda { days } => super::print(client.calendar_agenda_days(days).await?),
        Command::Sync => super::print(client.sync_calendar().await?),
    }
    Ok(())
}
